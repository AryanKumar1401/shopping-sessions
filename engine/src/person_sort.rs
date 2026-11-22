// SE_PEOPLE=sort: "sort once, then scan columns" (included into person.rs).
//
// Pass A: the events of a partition are sorted by (ID, 30-minute bucket) with two counting
// passes; one linear scan turns each ID's timestamps into bucket aggregates [min, max, count].
// An unlinked ID's aggregates are chained into sessions right away; a linked ID's aggregates
// leave as one run for the partition of the person's name. Clicks and purchases of linked IDs
// leave as flat (name, ts, payload) records.
// Pass B: the aggregates of a partition are sorted by (person, bucket), merged and chained.
// Both passes then hold the partition's sessions as one array ordered by (person, start), its
// clicks as one array sorted by (person, ts) and its purchases likewise, and `sweep_flat`
// credits, counts assists and writes the rows of all its people in linear scans.

/// SE_PEOPLE: `sort` = 2 (sorted front end and flat sweep), `flat` = 1 (the grouping front end
/// feeding the flat sweep), anything else = 0.
fn people_mode() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("SE_PEOPLE").as_deref() {
        Ok("sort") => 2,
        Ok("flat") => 1,
        Ok("flatb") => 3,
        _ => 0,
    })
}

const FLAT_PROF: [&str; 32] = [
    "A scan events", "A scan clicks", "A link lookups", "A group ts", "A group clicks", "A purchases", "A sessions", "A sort clicks",
    "A write runs", "B group runs", "B people", "", "", "", "", "",
    "S rows", "S purchases", "S credit", "S window", "S assists", "S emit", "", "",
    "", "", "", "", "", "", "", "(write calls)",
];
const SORT_PROF: [&str; 32] = [
    "A scan events", "A scan clicks", "A link lookups", "A sort events", "A route clicks", "A purchases", "A aggs+sessions", "A sort clicks",
    "B read runs", "B read clicks", "B sort aggs", "B sessions", "B purchases", "B sort clicks", "", "",
    "S rows", "S purchases", "S credit", "S window", "S assists", "S emit", "", "",
    "", "", "", "", "", "", "", "(write calls)",
];

/// Events of one ID (or person) in one 30-minute bucket.
#[derive(Clone, Copy)]
#[repr(C)]
struct Agg {
    min: u32,
    max: u32,
    cnt: u32,
}
#[derive(Clone, Copy)]
#[repr(C)]
struct AggR {
    rank: u32,
    a: Agg,
}
/// A session of the person `rank` of the partition.
#[derive(Clone, Copy)]
#[repr(C)]
struct FS {
    start: u32,
    end: u32,
    n: u32,
    rank: u32,
}
/// A click; `key` = rank << 33 | ts.
#[derive(Clone, Copy)]
#[repr(C)]
struct FClk {
    key: u64,
    camp: u32,
    /// which click (for `ptr_of`: the click line is only needed to break a tie)
    j: u32,
}
/// What pass A sends on for a click (`ptr != 0`, `x` = campaign) or a purchase (`ptr == 0`,
/// `x` = category * PRICE_MUL + price) of a linked ID.
#[derive(Clone, Copy)]
#[repr(C)]
struct FRec {
    name: Id,
    ptr: usize,
    ts: u32,
    x: u32,
}
const AGG_HDR: usize = 16;
#[inline(always)]
fn agg_run_bytes(n: usize) -> usize {
    AGG_HDR + ((n * 12 + 7) & !7)
}

struct Flat {
    nb: usize,
    k0: Vec<u64>,
    k1: Vec<u64>,
    hist: Vec<u32>,
    aggbuf: Vec<Agg>,
    ar: Vec<AggR>,
    ar2: Vec<AggR>,
    aggs: Vec<Agg>,
    dest: Vec<u32>,
    /// sessions by (rank, start)
    ss: Vec<FS>,
    sk: Vec<u64>,
    row_of: Vec<u32>,
    r_first: Vec<u32>,
    /// rank << 32 | day
    r_key: Vec<u64>,
    r_events: Vec<u32>,
    r_rev: Vec<u64>,
    r_cat: Vec<u32>,
    s_rev: Vec<u64>,
    fc: Vec<FClk>,
    fct: Vec<FClk>,
    ch: Vec<u32>,
    cptr: Vec<usize>,
    sa: Vec<u32>,
    sw: Vec<u32>,
    purch: Vec<(u64, u32)>,
    fpu: Vec<(u64, u32)>,
    prof: [u64; 32],
    t0: std::time::Instant,
}

impl Flat {
    fn new(span: u32) -> Self {
        Flat {
            nb: (span / BW) as usize + 1,
            k0: Vec::new(),
            k1: Vec::new(),
            hist: Vec::new(),
            aggbuf: Vec::new(),
            ar: Vec::new(),
            ar2: Vec::new(),
            aggs: Vec::new(),
            dest: Vec::new(),
            ss: Vec::new(),
            sk: Vec::new(),
            row_of: Vec::new(),
            r_first: Vec::new(),
            r_key: Vec::new(),
            r_events: Vec::new(),
            r_rev: Vec::new(),
            r_cat: Vec::new(),
            s_rev: Vec::new(),
            fc: Vec::new(),
            fct: Vec::new(),
            ch: Vec::new(),
            cptr: Vec::new(),
            sa: Vec::new(),
            sw: Vec::new(),
            purch: Vec::new(),
            fpu: Vec::new(),
            prof: [0; 32],
            t0: std::time::Instant::now(),
        }
    }
    #[inline(always)]
    fn lap(&mut self, i: usize) {
        let n = std::time::Instant::now();
        self.prof[i] += (n - self.t0).as_nanos() as u64;
        self.t0 = n;
    }
}

/// Counts to starting offsets.
#[inline(always)]
fn prefix(h: &mut [u32]) {
    let mut s = 0u32;
    for x in h {
        let c = *x;
        *x = s;
        s += c;
    }
}

/// One counting-sort pass: `src[j]` goes to `dst[cur[digit(src[j])]++]`.
#[inline(always)]
unsafe fn scatter<T: Copy>(src: &[T], dst: *mut T, cur: &mut [u32], digit: impl Fn(&T) -> usize) {
    unsafe {
        for x in src {
            let c = cur.get_unchecked_mut(digit(x));
            *dst.add(*c as usize) = *x;
            *c += 1;
        }
    }
}

/// Bucket aggregates of timestamps that are ordered by bucket. Returns how many.
#[inline(always)]
unsafe fn agg_run(t: &[u32], out: *mut Agg) -> usize {
    unsafe {
        let mut k = 0usize;
        let mut pb = u32::MAX;
        let (mut mn, mut mx, mut cnt) = (0u32, 0u32, 0u32);
        for &x in t {
            let b = x / BW;
            let new = b != pb;
            pb = b;
            k += new as usize;
            mn = if new { x } else { mn.min(x) };
            mx = if new { x } else { mx.max(x) };
            cnt = if new { 1 } else { cnt + 1 };
            *out.add(k - 1) = Agg { min: mn, max: mx, cnt };
        }
        k
    }
}

/// Merges neighbours of the same bucket (aggregates ordered by bucket), in place.
#[inline(always)]
fn merge_buckets(a: &mut [Agg]) -> usize {
    let mut k = 0usize;
    let mut pb = u32::MAX;
    let (mut mn, mut mx, mut cnt) = (0u32, 0u32, 0u32);
    for j in 0..a.len() {
        let e = a[j];
        let b = e.min / BW;
        let new = b != pb;
        pb = b;
        k += new as usize;
        mn = if new { e.min } else { mn.min(e.min) };
        mx = if new { e.max } else { mx.max(e.max) };
        cnt = if new { e.cnt } else { cnt + e.cnt };
        a[k - 1] = Agg { min: mn, max: mx, cnt };
    }
    k
}

/// Sessions of one person from their aggregates (one per bucket, ascending, not empty), written
/// at `out[k..]`. Returns the new session count.
#[inline(always)]
unsafe fn sess_from_aggs(a: &[Agg], rank: u32, out: *mut FS, mut k: usize) -> usize {
    unsafe {
        let e = *a.get_unchecked(0);
        let (mut start, mut end, mut n) = (e.min, e.max, e.cnt);
        *out.add(k) = FS { start, end, n, rank };
        k += 1;
        for e in &a[1..] {
            let new = e.min - end > GAP;
            k += new as usize;
            start = if new { e.min } else { start };
            n = if new { e.cnt } else { n + e.cnt };
            end = e.max;
            *out.add(k - 1) = FS { start, end, n, rank };
        }
        k
    }
}

/// Sorts the clicks by key (rank, ts); `nk` = number of ranks.
fn sort_fc(fc: &mut Vec<FClk>, tmp: &mut Vec<FClk>, h: &mut Vec<u32>, nk: usize) {
    let n = fc.len();
    if n < 48 || n >= u32::MAX as usize {
        fc.sort_unstable_by_key(|c| c.key);
        return;
    }
    const D: usize = 2048;
    h.clear();
    h.resize(3 * D + nk, 0);
    unsafe {
        for x in fc.iter() {
            let k = x.key;
            *h.get_unchecked_mut(k as usize & (D - 1)) += 1;
            *h.get_unchecked_mut(D + ((k >> 11) as usize & (D - 1))) += 1;
            *h.get_unchecked_mut(2 * D + ((k >> 22) as usize & (D - 1))) += 1;
            *h.get_unchecked_mut(3 * D + (k >> 33) as usize) += 1;
        }
        let (h0, r) = h.split_at_mut(D);
        let (h1, r) = r.split_at_mut(D);
        let (h2, h3) = r.split_at_mut(D);
        prefix(h0);
        prefix(h1);
        prefix(h2);
        prefix(h3);
        tmp.clear();
        tmp.reserve(n);
        tmp.set_len(n);
        scatter(fc, tmp.as_mut_ptr(), h0, |x| x.key as usize & (D - 1));
        scatter(tmp, fc.as_mut_ptr(), h1, |x| (x.key >> 11) as usize & (D - 1));
        scatter(fc, tmp.as_mut_ptr(), h2, |x| (x.key >> 22) as usize & (D - 1));
        scatter(tmp, fc.as_mut_ptr(), h3, |x| (x.key >> 33) as usize);
    }
}

/// Several clicks at the latest timestamp (`top` is the last of them): the smallest click_id
/// wins. The IDs are only read (from the file) when the tied clicks disagree on the campaign.
#[cold]
#[inline(never)]
fn tie_camp(fc: &[FClk], top: usize, ptr_of: &dyn Fn(u32) -> usize) -> u32 {
    let t = fc[top];
    let mut k = top;
    let mut differ = false;
    while k > 0 && fc[k - 1].key == t.key {
        k -= 1;
        differ |= fc[k].camp != t.camp;
    }
    let mut camp = t.camp;
    if differ {
        let mut best = unsafe { click_id(ptr_of(t.j)) };
        for c in &fc[k..top] {
            let id = unsafe { click_id(ptr_of(c.j)) };
            if id < best {
                best = id;
                camp = c.camp;
            }
        }
    }
    camp
}

/// All the people of a partition: `f.ss` = their sessions by (rank, start), `f.fc` = their
/// clicks sorted by key, `f.fpu` = their purchases sorted by key, `names[rank]`.
fn sweep_flat(ctx: &Ctx, o: &mut Out, f: &mut Flat, names: &[Id], ptr_of: &dyn Fn(u32) -> usize) {
    let ns = f.ss.len();
    if ns == 0 {
        return;
    }
    let nc = f.fc.len();
    // (the credit scan reads `fc[0]` even when there is no click)
    f.fc.reserve(1);
    let (nd, organic, day_off) = (o.nd, o.organic as u32, o.day_off);
    // No 16-bit counter gets more than one per session; a partition with more sessions than
    // that counts into the wide counters directly.
    let wide = ns > 60_000;
    if o.hot_n as usize + ns > u16::MAX as usize {
        o.fold_hot();
    }
    if !wide {
        o.hot_n += ns as u32;
    }
    // ---- rows: a new one at every change of (rank, day)
    f.sk.clear();
    f.sk.reserve(ns + 1);
    f.row_of.clear();
    f.row_of.reserve(ns + 1);
    f.r_first.clear();
    f.r_first.reserve(ns + 2);
    f.r_key.clear();
    f.r_key.reserve(ns + 1);
    f.r_events.clear();
    f.r_events.reserve(ns);
    let mut nr = 0usize;
    unsafe {
        let (sk, row_of, r_first, r_key, r_events) = (f.sk.as_mut_ptr(), f.row_of.as_mut_ptr(), f.r_first.as_mut_ptr(), f.r_key.as_mut_ptr(), f.r_events.as_mut_ptr());
        let mut prev = u64::MAX;
        let (mut first, mut evs) = (0u32, 0u32);
        for (s, se) in f.ss.iter().enumerate() {
            *sk.add(s) = ((se.rank as u64) << 33) | se.start as u64;
            let day = (se.start as u64 + day_off) / DAY as u64;
            let dk = ((se.rank as u64) << 32) | day;
            let new = dk != prev;
            prev = dk;
            nr += new as usize;
            // all ones for a new row
            let mask = (new as u32).wrapping_neg();
            first = (first & !mask) | (s as u32 & mask);
            evs = (evs & !mask) + se.n;
            *r_first.add(nr - 1) = first;
            *r_key.add(nr - 1) = dk;
            *r_events.add(nr - 1) = evs;
            *row_of.add(s) = nr as u32 - 1;
        }
        // one row past the end, empty, for the clicks after the last session
        *sk.add(ns) = u64::MAX;
        *row_of.add(ns) = nr as u32;
        *r_first.add(nr) = ns as u32;
        *r_first.add(nr + 1) = ns as u32;
        *r_key.add(nr) = 7;
        f.sk.set_len(ns + 1);
        f.row_of.set_len(ns + 1);
        f.r_first.set_len(nr + 2);
        f.r_key.set_len(nr + 1);
        f.r_events.set_len(nr);
    }
    f.lap(16);
    // ---- purchases: revenue per session and row, top category per row
    f.s_rev.clear();
    f.s_rev.resize(ns, 0);
    f.r_rev.clear();
    f.r_rev.resize(nr, 0);
    f.r_cat.clear();
    f.r_cat.resize(nr, u32::MAX);
    if !f.fpu.is_empty() {
        let mut s = 0usize;
        let mut cur_row = usize::MAX;
        for &(k, pc) in &f.fpu {
            while f.sk[s + 1] <= k {
                s += 1;
            }
            let price = (pc % PRICE_MUL) as u64;
            let cat = (pc / PRICE_MUL) as usize;
            let r = f.row_of[s] as usize;
            if r != cur_row {
                if cur_row != usize::MAX {
                    f.r_cat[cur_row] = best_cat(o);
                }
                cur_row = r;
            }
            f.s_rev[s] += price;
            f.r_rev[r] += price;
            if o.catsum[cat] == 0 {
                o.touched.push(cat as u32);
            }
            o.catsum[cat] += price;
        }
        f.r_cat[cur_row] = best_cat(o);
    }
    f.lap(17);
    // ---- credit: the latest click at or before the start, if within a week
    f.sa.clear();
    f.sa.reserve(nc + 1);
    unsafe {
        let sa = f.sa.as_mut_ptr();
        let fc = f.fc.as_ptr();
        let mut i = 0usize;
        for s in 0..ns {
            let k = *f.sk.get_unchecked(s);
            while i < nc && (*fc.add(i)).key <= k {
                *sa.add(i) = s as u32;
                i += 1;
            }
            // (`fc[-1]` is not read as a click: `i > 0` is part of `ok`)
            let top = *fc.add(i.saturating_sub(1));
            let ok = (i > 0) & (top.key.wrapping_add(WEEK as u64) >= k);
            // (a select without a branch: `ok` is a coin toss)
            let mask = (ok as u32).wrapping_neg();
            let mut camp = organic ^ ((organic ^ top.camp) & mask);
            // one well-predicted branch: a tie is rare, `ok` alone is a coin toss
            if ok & (i >= 2) & ((*fc.add(i.saturating_sub(2))).key == top.key) {
                camp = tie_camp(&f.fc, i - 1, ptr_of);
            }
            let se = f.ss.get_unchecked(s);
            let di = ((se.start as u64 + day_off) / DAY as u64) as usize - 7;
            let ix = camp as usize * nd + di;
            let rev = *f.s_rev.get_unchecked(s);
            if wide {
                let a = o.acc.get_unchecked_mut(ix);
                a[0] += 1;
                if rev > 0 {
                    a[1] += 1;
                    a[2] += rev;
                }
            } else {
                o.hot.get_unchecked_mut(ix)[1] += 1;
                // one more purchasing session (top 16 bits) and its revenue (low 48 bits)
                if rev > 0 {
                    let p = o.pr[ix];
                    if rev >= PR_LIM || (p & (PR_ONE - 1)) + rev >= PR_LIM {
                        o.acc[ix][2] += (p & (PR_ONE - 1)) + rev;
                        o.pr[ix] = (p & !(PR_ONE - 1)) + PR_ONE;
                    } else {
                        o.pr[ix] = p + rev + PR_ONE;
                    }
                }
            }
        }
        while i < nc {
            *sa.add(i) = ns as u32;
            i += 1;
        }
        f.sa.set_len(nc);
    }
    f.lap(18);
    if nc > 0 {
        // ---- per click: the first session it is too old for
        f.sw.clear();
        f.sw.reserve(nc);
        unsafe {
            let sw = f.sw.as_mut_ptr();
            let mut w = 0usize;
            for (i, c) in f.fc.iter().enumerate() {
                let lim = c.key + WEEK as u64;
                while *f.sk.get_unchecked(w) <= lim {
                    w += 1;
                }
                *sw.add(i) = w as u32;
            }
            f.sw.set_len(nc);
        }
        f.lap(19);
        // ---- assists: a session counts for the latest click of each campaign at or before
        // its start, if within a week. Backwards over the clicks, `mark[c]` is the first
        // session of the click of campaign `c` seen last, i.e. of the next one in time; marks
        // left by other people or partitions are larger than every session index here.
        if (o.mark_top as usize) < ns + 2 {
            o.mark.fill(u32::MAX);
            o.mark_top = u32::MAX - 1;
        }
        o.mark_top -= ns as u32 + 1;
        let base = o.mark_top;
        unsafe {
            for i in (0..nc).rev() {
                let c = f.fc.get_unchecked(i).camp as usize;
                let a = *f.sa.get_unchecked(i);
                let m = o.mark.get_unchecked_mut(c);
                let e = (*f.sw.get_unchecked(i) + base).min(*m) - base;
                *m = a + base;
                // sessions a..e, row by row; the first row unconditionally (it may get 0)
                let mut k = *f.row_of.get_unchecked(a as usize) as usize;
                loop {
                    let s0 = a.max(*f.r_first.get_unchecked(k));
                    let nx = *f.r_first.get_unchecked(k + 1);
                    let s1 = e.min(nx);
                    let cnt = s1.saturating_sub(s0);
                    let ix = c * nd + (*f.r_key.get_unchecked(k) as u32 as usize - 7);
                    if wide {
                        o.acc.get_unchecked_mut(ix)[3] += cnt as u64;
                    } else {
                        o.hot.get_unchecked_mut(ix)[0] += cnt as u16;
                    }
                    k += 1;
                    if nx >= e {
                        break;
                    }
                }
            }
        }
        f.lap(20);
    }
    // ---- rows, column by column
    let cw = o.cw;
    let day0 = o.day0;
    let mut k = 0usize;
    while k < nr {
        let m = (nr - k).min(GROUP_ROWS - o.rows);
        let r0 = o.rows;
        unsafe {
            let c0 = &mut o.cols[0].data;
            let mut j = k;
            while j < k + m {
                let rank = (*f.r_key.get_unchecked(j) >> 32) as u32;
                let mut e = j + 1;
                while e < k + m && (*f.r_key.get_unchecked(e) >> 32) as u32 == rank {
                    e += 1;
                }
                let id = names[rank as usize];
                let cnt = e - j;
                if is_long(id) {
                    let b = long_bytes(id);
                    c0.reserve(cnt * (4 + b.len()));
                    for _ in 0..cnt {
                        c0.extend_from_slice(&(b.len() as u32).to_le_bytes());
                        c0.extend_from_slice(b);
                    }
                } else {
                    let (w, nl) = unpack_short(id);
                    c0.reserve(cnt * (4 + nl) + 16);
                    let l0 = c0.len();
                    let mut p = c0.as_mut_ptr().add(l0);
                    for _ in 0..cnt {
                        (p as *mut u32).write_unaligned(nl as u32);
                        (p.add(4) as *mut [u8; 16]).write_unaligned(w);
                        p = p.add(4 + nl);
                    }
                    c0.set_len(l0 + cnt * (4 + nl));
                }
                j = e;
            }
            let p = o.cols[1].data.as_mut_ptr().add(r0 * 4) as *mut i32;
            for j in 0..m {
                p.add(j).write_unaligned((day0 + (*f.r_key.get_unchecked(k + j) as u32) as i64) as i32);
            }
            if cw == 4 {
                let p = o.cols[2].data.as_mut_ptr().add(r0 * 4) as *mut u32;
                for j in 0..m {
                    p.add(j).write_unaligned(*f.r_first.get_unchecked(k + j + 1) - *f.r_first.get_unchecked(k + j));
                }
                let p = o.cols[3].data.as_mut_ptr().add(r0 * 4) as *mut u32;
                for j in 0..m {
                    p.add(j).write_unaligned(*f.r_events.get_unchecked(k + j));
                }
            } else {
                let p = o.cols[2].data.as_mut_ptr().add(r0 * 8) as *mut u64;
                for j in 0..m {
                    p.add(j).write_unaligned((*f.r_first.get_unchecked(k + j + 1) - *f.r_first.get_unchecked(k + j)) as u64);
                }
                let p = o.cols[3].data.as_mut_ptr().add(r0 * 8) as *mut u64;
                for j in 0..m {
                    p.add(j).write_unaligned(*f.r_events.get_unchecked(k + j) as u64);
                }
            }
            let p = o.cols[4].data.as_mut_ptr().add(r0 * 8) as *mut u64;
            for j in 0..m {
                p.add(j).write_unaligned(*f.r_rev.get_unchecked(k + j));
            }
        }
        for j in 0..m {
            let best = f.r_cat[k + j];
            o.cols[5].push_def(r0 + j, best != u32::MAX);
            if best != u32::MAX {
                o.cols[5].push_str(&ctx.cats[best as usize]);
            }
        }
        o.rows += m;
        k += m;
        if o.rows >= GROUP_ROWS {
            f.lap(21);
            o.write_group();
            f.t0 = std::time::Instant::now();
        }
    }
    f.lap(21);
}

/// The bytes (zero padded) and the length of an ID that is packed 7 bits per character.
#[inline(always)]
fn unpack_short(id: Id) -> ([u8; 16], usize) {
    let mut w = [0u8; 16];
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        let nl = 9 - (id.trailing_zeros() as usize) / 7;
        let rest = unsafe { std::arch::x86_64::_pdep_u64(id, 0x7F7F_7F7F_7F7F_7F7F) }.swap_bytes();
        w[0] = (id >> 56) as u8 & 0x7F;
        w[1..9].copy_from_slice(&rest.to_le_bytes());
        (w, nl)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let mut nl = 0;
        let mut sh = 56i32;
        while sh >= 0 {
            let c = ((id >> sh) & 0x7F) as u8;
            if c == 0 {
                break;
            }
            w[nl] = c;
            nl += 1;
            sh -= 7;
        }
        (w, nl)
    }
}

/// The category with the largest sum among the touched ones (ties: the smaller index); resets them.
#[inline(always)]
fn best_cat(o: &mut Out) -> u32 {
    let mut best = u32::MAX;
    let mut best_sum = 0u64;
    for &c in &o.touched {
        let s = o.catsum[c as usize];
        if s > best_sum || (s == best_sum && c < best) {
            best = c;
            best_sum = s;
        }
        o.catsum[c as usize] = 0;
    }
    o.touched.clear();
    best
}

#[allow(clippy::too_many_arguments)]
/// The line of click `j` of partition `q` (in the order of its records).
#[cold]
#[inline(never)]
fn nth_click(cl: &[Parts<CRec>], q: usize, j: u32) -> usize {
    let mut j = j as usize;
    for p in cl {
        for sl in p.slices(q) {
            if j < sl.len() {
                return sl[j].ptr;
            }
            j -= sl.len();
        }
    }
    unreachable!()
}

#[allow(clippy::too_many_arguments)]
fn pass_a_sort(ctx: &Ctx, q: usize, ev: &[Parts<PRec>], cl: &[Parts<CRec>], sc: &mut Scratch, f: &mut Flat, o: &mut Out, runs: &mut RunOut, cruns: &mut RunOut) {
    let n: usize = ev.iter().map(|p| p.count(q)).sum();
    let nc: usize = cl.iter().map(|p| p.count(q)).sum();
    if n + nc == 0 {
        return;
    }
    f.t0 = std::time::Instant::now();
    sc.reset();
    f.purch.clear();
    f.k0.clear();
    f.k0.reserve(n);
    f.hist.clear();
    f.hist.resize(f.nb, 0);
    let mut m = 0usize;
    {
        let k0 = f.k0.as_mut_ptr();
        for p in ev {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    if r.ts == DEAD {
                        continue;
                    }
                    let idx = sc.get_or_insert(r.k);
                    unsafe {
                        *sc.cnt.get_unchecked_mut(idx as usize) += 1;
                        *k0.add(m) = ((idx as u64) << 32) | r.ts as u64;
                        *f.hist.get_unchecked_mut((r.ts / BW) as usize) += 1;
                    }
                    m += 1;
                    if r.pc != 0 {
                        f.purch.push((((idx as u64) << 33) | r.ts as u64, r.pc));
                    }
                }
            }
        }
        unsafe { f.k0.set_len(m) };
    }
    let n = m;
    if env_recycle() {
        for p in ev {
            for sl in p.slices(q) {
                let base = sl.as_ptr() as usize;
                if base % 8 == 0 {
                    for k in 0..sl.len() * std::mem::size_of::<PRec>() / RUN_BLOCK {
                        runs.free.push(base + k * RUN_BLOCK);
                    }
                }
            }
        }
    }
    f.lap(0);
    sc.cidx.clear();
    sc.cidx.reserve(nc);
    {
        let ci = sc.cidx.as_mut_ptr();
        let mut m = 0usize;
        for p in cl {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    let idx = sc.get_or_insert(r.k);
                    unsafe { *ci.add(m) = idx };
                    m += 1;
                }
            }
        }
        unsafe { sc.cidx.set_len(m) };
    }
    let nk = sc.keys.len();
    f.lap(1);
    // one link lookup per distinct ID
    sc.names.clear();
    f.dest.clear();
    f.dest.resize(nk, 0);
    let links = ctx.links.get().unwrap();
    if links.any {
        const AHEAD: usize = 8;
        for i in 0..nk.min(AHEAD) {
            links.prefetch(id_hash(sc.keys[i]));
        }
        for i in 0..nk {
            if i + AHEAD < nk {
                links.prefetch(id_hash(sc.keys[i + AHEAD]));
            }
            let k = sc.keys[i];
            let name = links.lookup(k, id_hash(k));
            sc.names.push(name);
            if name != 0 {
                f.dest[i] = part_of(id_hash(name), ctx.pbits) as u32;
            }
        }
    } else {
        sc.names.resize(nk, 0);
    }
    f.lap(2);
    // events by (ID, bucket): one pass by bucket, one by ID
    prefix(&mut f.hist);
    f.k1.clear();
    f.k1.reserve(n);
    sc.off.clear();
    sc.cur.clear();
    let mut sum = 0u32;
    for &c in &sc.cnt {
        sc.off.push(sum);
        sc.cur.push(sum);
        sum += c;
    }
    sc.off.push(sum);
    sc.ts.clear();
    sc.ts.reserve(n + OVER);
    unsafe {
        scatter(&f.k0, f.k1.as_mut_ptr(), &mut f.hist, |&k| (k as u32 / BW) as usize);
        f.k1.set_len(n);
        let ts = sc.ts.as_mut_ptr();
        for &k in &f.k1 {
            let c = sc.cur.get_unchecked_mut((k >> 32) as usize);
            *ts.add(*c as usize) = k as u32;
            *c += 1;
        }
        sc.ts.set_len(n);
    }
    f.lap(3);
    // aggregates per ID; sessions of the unlinked, runs of the linked
    f.aggbuf.clear();
    f.aggbuf.reserve(n + 4);
    f.ss.clear();
    f.ss.reserve(n + 1);
    let mut nss = 0usize;
    unsafe {
        let ab = f.aggbuf.as_mut_ptr();
        let ss = f.ss.as_mut_ptr();
        for i in 0..nk {
            let (a, b) = (*sc.off.get_unchecked(i) as usize, *sc.off.get_unchecked(i + 1) as usize);
            if a == b {
                continue;
            }
            let na = agg_run(sc.ts.get_unchecked(a..b), ab);
            let name = *sc.names.get_unchecked(i);
            if name == 0 {
                nss = sess_from_aggs(std::slice::from_raw_parts(ab, na), i as u32, ss, nss);
            } else {
                let p = runs.alloc(*f.dest.get_unchecked(i) as usize, agg_run_bytes(na));
                (p as *mut u64).write(name);
                (p.add(8) as *mut u64).write(na as u64);
                copy_over(ab as *const u8, p.add(AGG_HDR), na * 12);
            }
        }
        f.ss.set_len(nss);
    }
    f.lap(6);
    // clicks: the unlinked stay, the linked go to their person's partition
    f.fc.clear();
    f.fc.reserve(nc);
    {
        let mut j = 0usize;
        for p in cl {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    let i = unsafe { *sc.cidx.get_unchecked(j) } as usize;
                    j += 1;
                    let name = unsafe { *sc.names.get_unchecked(i) };
                    if name == 0 {
                        f.fc.push(FClk { key: ((i as u64) << 33) | r.ts as u64, camp: r.camp, j: j as u32 - 1 });
                    } else {
                        let p = cruns.alloc(f.dest[i] as usize, 24) as *mut FRec;
                        unsafe { p.write(FRec { name, ptr: r.ptr, ts: r.ts, x: r.camp }) };
                    }
                }
            }
        }
    }
    f.lap(4);
    f.fpu.clear();
    if !f.purch.is_empty() {
        f.purch.sort_unstable();
        for &(k, pc) in &f.purch {
            let i = (k >> 33) as usize;
            let name = sc.names[i];
            if name == 0 {
                f.fpu.push((k, pc));
            } else {
                let p = cruns.alloc(f.dest[i] as usize, 24) as *mut FRec;
                unsafe { p.write(FRec { name, ptr: 0, ts: k as u32, x: pc }) };
            }
        }
    }
    f.lap(5);
    if nss == 0 {
        return;
    }
    sort_fc(&mut f.fc, &mut f.fct, &mut f.ch, nk);
    f.lap(7);
    sweep_flat(ctx, o, f, &sc.keys, &|j| nth_click(cl, q, j));
}

fn pass_b_sort(ctx: &Ctx, d: usize, blocks: &[Vec<Vec<(usize, usize)>>], cblocks: &[Vec<Vec<(usize, usize)>>], sc: &mut Scratch, f: &mut Flat, o: &mut Out) {
    f.t0 = std::time::Instant::now();
    sc.reset();
    f.ar.clear();
    f.hist.clear();
    f.hist.resize(f.nb, 0);
    for th in blocks {
        for &(st, used) in &th[d] {
            let (mut p, end) = (st, st + used);
            while p < end {
                unsafe {
                    let name = *(p as *const u64);
                    let na = *((p + 8) as *const u64) as usize;
                    let idx = sc.get_or_insert(name);
                    *sc.cnt.get_unchecked_mut(idx as usize) += na as u32;
                    let ap = (p + AGG_HDR) as *const Agg;
                    f.ar.reserve(na);
                    let l = f.ar.len();
                    let w = f.ar.as_mut_ptr().add(l);
                    for j in 0..na {
                        let a = *ap.add(j);
                        *f.hist.get_unchecked_mut((a.min / BW) as usize) += 1;
                        *w.add(j) = AggR { rank: idx, a };
                    }
                    f.ar.set_len(l + na);
                    p += agg_run_bytes(na);
                }
            }
        }
    }
    let n = f.ar.len();
    if n == 0 {
        return;
    }
    f.lap(8);
    f.fc.clear();
    f.fpu.clear();
    f.cptr.clear();
    let (mut last, mut last_idx) = (0u64, 0u32);
    for th in cblocks {
        for &(st, used) in &th[d] {
            let recs = unsafe { std::slice::from_raw_parts(st as *const FRec, used / 24) };
            for r in recs {
                if r.name != last {
                    last = r.name;
                    last_idx = sc.get_or_insert(r.name);
                }
                let key = ((last_idx as u64) << 33) | r.ts as u64;
                if r.ptr != 0 {
                    f.fc.push(FClk { key, camp: r.x, j: f.cptr.len() as u32 });
                    f.cptr.push(r.ptr);
                } else {
                    f.fpu.push((key, r.x));
                }
            }
        }
    }
    let nk = sc.keys.len();
    f.lap(9);
    // aggregates by (person, bucket)
    prefix(&mut f.hist);
    f.ar2.clear();
    f.ar2.reserve(n);
    sc.off.clear();
    sc.cur.clear();
    let mut sum = 0u32;
    for &c in &sc.cnt {
        sc.off.push(sum);
        sc.cur.push(sum);
        sum += c;
    }
    sc.off.push(sum);
    f.aggs.clear();
    f.aggs.reserve(n);
    unsafe {
        scatter(&f.ar, f.ar2.as_mut_ptr(), &mut f.hist, |x| (x.a.min / BW) as usize);
        f.ar2.set_len(n);
        let ag = f.aggs.as_mut_ptr();
        for x in &f.ar2 {
            let c = sc.cur.get_unchecked_mut(x.rank as usize);
            *ag.add(*c as usize) = x.a;
            *c += 1;
        }
        f.aggs.set_len(n);
    }
    f.lap(10);
    f.ss.clear();
    f.ss.reserve(n + 1);
    let mut nss = 0usize;
    unsafe {
        let ss = f.ss.as_mut_ptr();
        for i in 0..nk {
            let (a, b) = (*sc.off.get_unchecked(i) as usize, *sc.off.get_unchecked(i + 1) as usize);
            if a == b {
                continue;
            }
            let sl = f.aggs.get_unchecked_mut(a..b);
            let na = merge_buckets(sl);
            nss = sess_from_aggs(&sl[..na], i as u32, ss, nss);
        }
        f.ss.set_len(nss);
    }
    f.lap(11);
    f.fpu.sort_unstable();
    f.lap(12);
    sort_fc(&mut f.fc, &mut f.fct, &mut f.ch, nk);
    f.lap(13);
    let cptr = std::mem::take(&mut f.cptr);
    sweep_flat(ctx, o, f, &sc.keys, &|j| cptr[j as usize]);
    f.cptr = cptr;
}

// ---------------------------------------------------------------- SE_PEOPLE=flat
// The grouping front end of the default mode (group by ID, 30-minute buckets, runs for linked
// IDs) feeding `sweep_flat`: sessions, clicks and purchases of a partition's people are laid
// out as flat arrays ordered by (person, time) instead of being swept person by person.

impl Buckets {
    /// `drain` into flat sessions of `rank`.
    #[inline(always)]
    fn drain_fs(&mut self, sess: &mut Vec<FS>, rank: u32) {
        let mut cur = FS { start: 0, end: 0, n: 0, rank };
        let mut wm = std::mem::take(&mut self.wm);
        while wm != 0 {
            let w = wm.trailing_zeros() as usize;
            wm &= wm - 1;
            let mut m = std::mem::take(unsafe { self.bm.get_unchecked_mut(w) });
            while m != 0 {
                let b = w * 64 + m.trailing_zeros() as usize;
                m &= m - 1;
                let e = std::mem::replace(unsafe { self.b.get_unchecked_mut(b) }, [u32::MAX, 0, 0]);
                if cur.n != 0 && e[0] - cur.end > GAP {
                    sess.push(cur);
                    cur.n = 0;
                }
                if cur.n == 0 {
                    cur.start = e[0];
                }
                cur.end = e[1];
                cur.n += e[2];
            }
        }
        if cur.n != 0 {
            sess.push(cur);
        }
    }
}

/// `sessions_small` into flat sessions of `rank`.
#[inline(always)]
fn sessions_small_fs(t: &mut [u32], sess: &mut Vec<FS>, rank: u32) {
    let n = t.len();
    for i in 1..n {
        let x = t[i];
        let mut j = i;
        while j > 0 && t[j - 1] > x {
            t[j] = t[j - 1];
            j -= 1;
        }
        t[j] = x;
    }
    let mut cur = FS { start: t[0], end: t[0], n: 0, rank };
    for &x in t.iter() {
        if x - cur.end > GAP {
            sess.push(cur);
            cur = FS { start: x, end: x, n: 0, rank };
        }
        cur.end = x;
        cur.n += 1;
    }
    sess.push(cur);
}

/// Sorts one person's clicks by key (ties in any order).
#[inline(always)]
fn sort_fclk(c: &mut [FClk]) {
    let n = c.len();
    if n <= 24 {
        for i in 1..n {
            let x = c[i];
            let mut j = i;
            while j > 0 && c[j - 1].key > x.key {
                c[j] = c[j - 1];
                j -= 1;
            }
            c[j] = x;
        }
    } else {
        c.sort_unstable_by_key(|c| c.key);
    }
}

fn pass_a_flat(ctx: &Ctx, q: usize, ev: &[Parts<PRec>], cl: &[Parts<CRec>], sc: &mut Scratch, f: &mut Flat, o: &mut Out, runs: &mut RunOut) {
    let n: usize = ev.iter().map(|p| p.count(q)).sum();
    let nc: usize = cl.iter().map(|p| p.count(q)).sum();
    if n + nc == 0 {
        return;
    }
    f.t0 = std::time::Instant::now();
    sc.reset();
    sc.purch.clear();
    sc.pidx.clear();
    sc.tstmp.clear();
    sc.pidx.reserve(n);
    sc.tstmp.reserve(n);
    let mut m = 0usize;
    {
        let (pi, tt) = (sc.pidx.as_mut_ptr(), sc.tstmp.as_mut_ptr());
        for p in ev {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    if r.ts == DEAD {
                        continue;
                    }
                    let idx = sc.get_or_insert(r.k);
                    unsafe {
                        *sc.cnt.get_unchecked_mut(idx as usize) += 1;
                        *pi.add(m) = idx;
                        *tt.add(m) = r.ts;
                    }
                    m += 1;
                    if r.pc != 0 {
                        sc.purch.push((((idx as u64) << 32) | r.ts as u64, r.pc));
                    }
                }
            }
        }
        unsafe {
            sc.pidx.set_len(m);
            sc.tstmp.set_len(m);
        }
    }
    let n = m;
    if env_recycle() {
        for p in ev {
            for sl in p.slices(q) {
                let base = sl.as_ptr() as usize;
                if base % 8 == 0 {
                    for k in 0..sl.len() * std::mem::size_of::<PRec>() / RUN_BLOCK {
                        runs.free.push(base + k * RUN_BLOCK);
                    }
                }
            }
        }
    }
    f.lap(0);
    sc.cidx.clear();
    sc.cidx.reserve(nc);
    f.cptr.clear();
    f.cptr.reserve(nc);
    {
        let ci = sc.cidx.as_mut_ptr();
        let cp = f.cptr.as_mut_ptr();
        let mut m = 0usize;
        for p in cl {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    let idx = sc.get_or_insert(r.k);
                    unsafe {
                        *sc.ccnt.get_unchecked_mut(idx as usize) += 1;
                        *ci.add(m) = idx;
                        *cp.add(m) = r.ptr;
                    }
                    m += 1;
                }
            }
        }
        unsafe {
            sc.cidx.set_len(m);
            f.cptr.set_len(m);
        }
    }
    let nk = sc.keys.len();
    f.lap(1);
    sc.names.clear();
    let links = ctx.links.get().unwrap();
    if links.any {
        const AHEAD: usize = 8;
        for i in 0..nk.min(AHEAD) {
            links.prefetch(id_hash(sc.keys[i]));
        }
        for i in 0..nk {
            if i + AHEAD < nk {
                links.prefetch(id_hash(sc.keys[i + AHEAD]));
            }
            let k = sc.keys[i];
            sc.names.push(links.lookup(k, id_hash(k)));
        }
    } else {
        sc.names.resize(nk, 0);
    }
    f.lap(2);
    // timestamps grouped by ID
    sc.off.clear();
    sc.cur.clear();
    let mut sum = 0u32;
    for &c in &sc.cnt {
        sc.off.push(sum);
        sc.cur.push(sum);
        sum += c;
    }
    sc.off.push(sum);
    sc.ts.clear();
    sc.ts.reserve(n + OVER);
    unsafe {
        let ts = sc.ts.as_mut_ptr();
        for j in 0..n {
            let i = *sc.pidx.get_unchecked(j) as usize;
            let c = sc.cur.get_unchecked_mut(i);
            *ts.add(*c as usize) = *sc.tstmp.get_unchecked(j);
            *c += 1;
        }
        sc.ts.set_len(n);
    }
    f.lap(3);
    // clicks grouped by ID, with their keys: the unlinked IDs first (in ID order), so that
    // their part of the array is what the sweep reads
    sc.coff.clear();
    sc.coff.resize(nk, 0);
    sc.cur.clear();
    sc.cur.resize(nk, 0);
    let mut sum = 0u32;
    for i in 0..nk {
        if sc.names[i] == 0 {
            sc.coff[i] = sum;
            sc.cur[i] = sum;
            sum += sc.ccnt[i];
        }
    }
    let unlinked = sum as usize;
    for i in 0..nk {
        if sc.names[i] != 0 {
            sc.coff[i] = sum;
            sc.cur[i] = sum;
            sum += sc.ccnt[i];
        }
    }
    f.fc.clear();
    f.fc.reserve(nc + 2);
    unsafe {
        let ck = f.fc.as_mut_ptr();
        let mut j = 0usize;
        for p in cl {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    let i = *sc.cidx.get_unchecked(j) as usize;
                    j += 1;
                    let c = sc.cur.get_unchecked_mut(i);
                    *ck.add(*c as usize) = FClk { key: ((i as u64) << 33) | r.ts as u64, camp: r.camp, j: j as u32 - 1 };
                    *c += 1;
                }
            }
        }
        f.fc.set_len(nc);
    }
    f.lap(4);
    // purchases by (ID, timestamp): all of them for the runs, the unlinked ones for the sweep
    sc.pu.clear();
    sc.pu.reserve(sc.purch.len() + OVER);
    f.fpu.clear();
    if !sc.purch.is_empty() {
        sc.purch.sort_unstable();
        for &(k, pc) in &sc.purch {
            sc.pu.push(Pu { ts: k as u32, pc });
            if sc.names[(k >> 32) as usize] == 0 {
                f.fpu.push((((k >> 32) << 33) | (k as u32) as u64, pc));
            }
        }
    }
    f.lap(5);
    sc.cur.clear();
    sc.cur.resize(nk + 1, 0);
    for &(k, _) in &sc.purch {
        sc.cur[(k >> 32) as usize + 1] += 1;
    }
    for i in 0..nk {
        sc.cur[i + 1] += sc.cur[i];
    }
    // people of their own: sessions, then clicks in order
    f.ss.clear();
    for i in 0..nk {
        let (a, b) = (sc.off[i] as usize, sc.off[i + 1] as usize);
        if sc.names[i] != 0 || a == b {
            continue;
        }
        if b - a <= SMALL {
            sessions_small_fs(&mut sc.ts[a..b], &mut f.ss, i as u32);
        } else {
            sc.bk.add(&sc.ts[a..b]);
            sc.bk.drain_fs(&mut f.ss, i as u32);
        }
    }
    f.lap(6);
    for i in 0..nk {
        // (the clicks of an ID without events stay as they are: no session looks at them)
        if sc.names[i] == 0 && sc.ccnt[i] > 1 && sc.off[i] != sc.off[i + 1] {
            let a = sc.coff[i] as usize;
            sort_fclk(&mut f.fc[a..a + sc.ccnt[i] as usize]);
        }
    }
    f.lap(7);
    // linked IDs: one run each, to the partition of the person's name
    for i in 0..nk {
        let name = sc.names[i];
        if name == 0 {
            continue;
        }
        let (a, b) = (sc.off[i] as usize, sc.off[i + 1] as usize);
        let (ca, ncl) = (sc.coff[i] as usize, sc.ccnt[i] as usize);
        let (pa, pb) = (sc.cur[i] as usize, sc.cur[i + 1] as usize);
        let (nts, npu) = (b - a, pb - pa);
        if i + PF_RUNS < nk && sc.names[i + PF_RUNS] != 0 {
            let d = part_of(id_hash(sc.names[i + PF_RUNS]), ctx.pbits);
            let p = runs.heads[d].0 as *const u8;
            prefetch(p);
            prefetch(p.wrapping_add(64));
            prefetch(p.wrapping_add(128));
        }
        let d = part_of(id_hash(name), ctx.pbits);
        let p = runs.alloc(d, run_bytes(nts, npu, ncl));
        unsafe {
            (p as *mut u64).write(name);
            let h = p.add(8) as *mut u32;
            h.write(nts as u32);
            h.add(1).write(npu as u32);
            h.add(2).write(ncl as u32);
            h.add(3).write(0);
            let mut w = p.add(RUN_HDR);
            copy_over(sc.ts.as_ptr().add(a) as *const u8, w, nts * 4);
            w = w.add((nts * 4 + 7) & !7);
            copy_over(sc.pu.as_ptr().add(pa) as *const u8, w, npu * 8);
            let w = w.add(npu * 8) as *mut Clk;
            for (j, c) in f.fc.get_unchecked(ca..ca + ncl).iter().enumerate() {
                w.add(j).write(Clk { ts: c.key as u32, camp: c.camp, ptr: *f.cptr.get_unchecked(c.j as usize) });
            }
        }
    }
    f.lap(8);
    f.fc.truncate(unlinked);
    let cptr = std::mem::take(&mut f.cptr);
    sweep_flat(ctx, o, f, &sc.keys, &|j| cptr[j as usize]);
    f.cptr = cptr;
}

fn pass_b_flat(ctx: &Ctx, d: usize, blocks: &[Vec<Vec<(usize, usize)>>], sc: &mut Scratch, f: &mut Flat, o: &mut Out) {
    f.t0 = std::time::Instant::now();
    sc.reset();
    sc.runs.clear();
    let mut total = 0usize;
    for th in blocks {
        for &(st, used) in &th[d] {
            let (mut p, end) = (st, st + used);
            while p < end {
                let h = unsafe { hdr(p) };
                let idx = sc.get_or_insert(h.name);
                sc.cnt[idx as usize] += 1;
                sc.runs.push((idx, p));
                total += h.nts;
                p += run_bytes(h.nts, h.npu, h.ncl);
            }
        }
    }
    if total == 0 {
        return;
    }
    let nk = sc.keys.len();
    sc.off.clear();
    sc.cur.clear();
    let mut sum = 0u32;
    for &c in &sc.cnt {
        sc.off.push(sum);
        sc.cur.push(sum);
        sum += c;
    }
    sc.off.push(sum);
    sc.rp.clear();
    sc.rp.resize(sc.runs.len(), 0);
    for &(idx, p) in &sc.runs {
        let c = &mut sc.cur[idx as usize];
        sc.rp[*c as usize] = p;
        *c += 1;
    }
    f.lap(9);
    f.ss.clear();
    f.fc.clear();
    f.fpu.clear();
    f.cptr.clear();
    for i in 0..nk {
        let rs = &sc.rp[sc.off[i] as usize..sc.off[i + 1] as usize];
        let mut nts = 0usize;
        for &r in rs {
            nts += unsafe { hdr(r) }.nts;
        }
        if nts == 0 {
            continue;
        }
        let small = nts <= SMALL;
        let mut few = [0u32; SMALL];
        let mut nfew = 0usize;
        let (c0, p0) = (f.fc.len(), f.fpu.len());
        let hi = (i as u64) << 33;
        let mut kpu = 0usize;
        for &r in rs {
            let h = unsafe { hdr(r) };
            unsafe {
                let tp = (r + RUN_HDR) as *const u32;
                let pp = (r + RUN_HDR + ((h.nts * 4 + 7) & !7)) as *const Pu;
                let cp = (pp as usize + h.npu * 8) as *const Clk;
                if h.nts > 0 {
                    let t = std::slice::from_raw_parts(tp, h.nts);
                    if small {
                        few[nfew..nfew + h.nts].copy_from_slice(t);
                        nfew += h.nts;
                    } else {
                        sc.bk.add(t);
                    }
                }
                if h.npu > 0 {
                    kpu += 1;
                    f.fpu.extend(std::slice::from_raw_parts(pp, h.npu).iter().map(|p| (hi | p.ts as u64, p.pc)));
                }
                if h.ncl > 0 {
                    let j0 = f.cptr.len() as u32;
                    let cs = std::slice::from_raw_parts(cp, h.ncl);
                    f.fc.extend(cs.iter().enumerate().map(|(j, c)| FClk { key: hi | c.ts as u64, camp: c.camp, j: j0 + j as u32 }));
                    f.cptr.extend(cs.iter().map(|c| c.ptr));
                }
            }
        }
        if small {
            sessions_small_fs(&mut few[..nfew], &mut f.ss, i as u32);
        } else {
            sc.bk.drain_fs(&mut f.ss, i as u32);
        }
        if kpu > 1 {
            f.fpu[p0..].sort_unstable();
        }
        sort_fclk(&mut f.fc[c0..]);
    }
    f.lap(10);
    let cptr = std::mem::take(&mut f.cptr);
    sweep_flat(ctx, o, f, &sc.keys, &|j| cptr[j as usize]);
    f.cptr = cptr;
}
