//! Per-person pass: group a partition's events by person, sort, sessionize, attribute clicks,
//! and emit user_daily rows plus per-thread campaign/day accumulators.

use crate::fast::{CRec, Ctx, DEAD, PRICE_MUL, PRec, WEEK};
use crate::parquet::{Col, Kind, PqFile};
use crate::ts::DAY;
use crate::util::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const GAP: u32 = 1_800_000;
const EMPTY: u32 = u32::MAX;
const GROUP_ROWS: usize = 1 << 16;

pub const UD_SCHEMA: [(&str, Kind); 6] = [
    ("user_id", Kind::Str),
    ("day", Kind::Date),
    ("sessions", Kind::I64),
    ("events", Kind::I64),
    ("revenue_cents", Kind::I64),
    ("top_category", Kind::StrOpt),
];
pub const CD_SCHEMA: [(&str, Kind); 7] = [
    ("campaign_id", Kind::Str),
    ("channel", Kind::Str),
    ("day", Kind::Date),
    ("sessions", Kind::I64),
    ("purchasing_sessions", Kind::I64),
    ("revenue_cents", Kind::I64),
    ("assisted_sessions", Kind::I64),
];

#[derive(Clone, Copy)]
struct Clk {
    ts: u32,
    camp: u32,
    ptr: usize,
}

#[derive(Clone, Copy)]
struct Sess {
    start: u32,
    end: u32,
    n: u32,
}

struct Scratch {
    slots: Vec<(Id, u32)>,
    mask: usize,
    keys: Vec<Id>,
    cnt: Vec<u32>,
    off: Vec<u32>,
    cur: Vec<u32>,
    pidx: Vec<u32>,
    tstmp: Vec<u32>,
    ts: Vec<u32>,
    purch: Vec<(u64, u32)>,
    ccnt: Vec<u32>,
    coff: Vec<u32>,
    cidx: Vec<u32>,
    ctmp: Vec<Clk>,
    clk: Vec<Clk>,
    sess: Vec<Sess>,
    acnt: Vec<u32>,
    apos: Vec<u32>,
    active: Vec<u32>,
    catsum: Vec<u64>,
    touched: Vec<u32>,
    acc: Vec<[u64; 4]>,
    radix: Vec<u32>,
    name: Vec<u8>,
    cols: Vec<Col>,
    rows: usize,
    file: PqFile,
}

impl Scratch {
    #[inline(always)]
    fn get_or_insert(&mut self, k: Id) -> u32 {
        let mut i = id_hash(k) as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.1 == EMPTY {
                break;
            }
            if s.0 == k {
                return s.1;
            }
            i = (i + 1) & self.mask;
        }
        let idx = self.keys.len() as u32;
        self.slots[i] = (k, idx);
        self.keys.push(k);
        self.cnt.push(0);
        if self.keys.len() * 2 > self.slots.len() {
            self.grow();
        }
        idx
    }
    #[cold]
    fn grow(&mut self) {
        let cap = self.slots.len() * 2;
        self.slots.clear();
        self.slots.resize(cap, (0, EMPTY));
        self.mask = cap - 1;
        for (idx, &k) in self.keys.iter().enumerate() {
            let mut i = id_hash(k) as usize & self.mask;
            while self.slots[i].1 != EMPTY {
                i = (i + 1) & self.mask;
            }
            self.slots[i] = (k, idx as u32);
        }
    }
    #[inline(always)]
    fn find(&self, k: Id) -> u32 {
        let mut i = id_hash(k) as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.1 == EMPTY || s.0 == k {
                return s.1;
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[inline]
fn sort_u32(t: &mut [u32], tmp: &mut Vec<u32>) {
    let n = t.len();
    if n <= 24 {
        for i in 1..n {
            let x = t[i];
            let mut j = i;
            while j > 0 && t[j - 1] > x {
                t[j] = t[j - 1];
                j -= 1;
            }
            t[j] = x;
        }
    } else if n < 2048 {
        t.sort_unstable();
    } else {
        radix_sort(t, tmp);
    }
}

/// LSD radix sort, 11 bits a pass, skipping passes where every key shares the digit.
fn radix_sort(t: &mut [u32], tmp: &mut Vec<u32>) {
    let n = t.len();
    tmp.clear();
    tmp.resize(n, 0);
    let mut hist = vec![[0u32; 2048]; 3];
    for &x in t.iter() {
        hist[0][(x & 2047) as usize] += 1;
        hist[1][((x >> 11) & 2047) as usize] += 1;
        hist[2][(x >> 22) as usize] += 1;
    }
    let mut src_is_t = true;
    for pass in 0..3 {
        let h = &mut hist[pass];
        if h.iter().any(|&c| c as usize == n) {
            continue;
        }
        let mut sum = 0u32;
        for c in h.iter_mut() {
            let v = *c;
            *c = sum;
            sum += v;
        }
        let shift = 11 * pass as u32;
        let (src, dst): (&[u32], &mut [u32]) = if src_is_t { (&*t, &mut tmp[..]) } else { (&tmp[..], &mut *t) };
        // SAFETY: src and dst are distinct buffers.
        let src: &[u32] = unsafe { std::slice::from_raw_parts(src.as_ptr(), n) };
        for &x in src {
            let d = ((x >> shift) & 2047) as usize;
            dst[h[d] as usize] = x;
            h[d] += 1;
        }
        src_is_t = !src_is_t;
    }
    if !src_is_t {
        t.copy_from_slice(&tmp[..]);
    }
}

/// The click_id of the click line at `ptr`.
unsafe fn click_id(ptr: usize) -> &'static [u8] {
    unsafe {
        let p = ptr as *const u8;
        let mut n = 0;
        while *p.add(n) != b',' {
            n += 1;
        }
        if n >= 2 && *p == b'"' { std::slice::from_raw_parts(p.add(1), n - 2) } else { std::slice::from_raw_parts(p, n) }
    }
}

fn do_part(ctx: &Ctx, q: usize, ev: &[Parts<PRec>], cl: &[Parts<CRec>], sc: &mut Scratch, nd: usize, day0: i64, day_off: u64) {
    let n: usize = ev.iter().map(|p| p.count(q)).sum();
    if n == 0 {
        return;
    }
    for s in sc.slots.iter_mut() {
        s.1 = EMPTY;
    }
    sc.keys.clear();
    sc.cnt.clear();
    sc.pidx.clear();
    sc.tstmp.clear();
    sc.purch.clear();
    sc.pidx.reserve(n);
    sc.tstmp.reserve(n);
    for p in ev {
        for sl in p.slices(q) {
            for r in sl.iter() {
                if r.ts == DEAD {
                    continue;
                }
                let idx = sc.get_or_insert(r.k);
                unsafe {
                    *sc.cnt.get_unchecked_mut(idx as usize) += 1;
                }
                sc.pidx.push(idx);
                sc.tstmp.push(r.ts);
                if r.pc != 0 {
                    sc.purch.push((((idx as u64) << 32) | r.ts as u64, r.pc));
                }
            }
        }
    }
    let np = sc.keys.len();
    let n = sc.pidx.len();
    // counting sort of the timestamps by person
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
    sc.ts.resize(n, 0);
    for j in 0..n {
        unsafe {
            let i = *sc.pidx.get_unchecked(j) as usize;
            let c = sc.cur.get_unchecked_mut(i);
            *sc.ts.get_unchecked_mut(*c as usize) = *sc.tstmp.get_unchecked(j);
            *c += 1;
        }
    }
    // clicks of the people who have events
    sc.ccnt.clear();
    sc.ccnt.resize(np, 0);
    sc.cidx.clear();
    sc.ctmp.clear();
    for p in cl {
        for sl in p.slices(q) {
            for r in sl.iter() {
                let idx = sc.find(r.k);
                if idx != EMPTY {
                    sc.ccnt[idx as usize] += 1;
                    sc.cidx.push(idx);
                    sc.ctmp.push(Clk { ts: r.ts, camp: r.camp, ptr: r.ptr });
                }
            }
        }
    }
    sc.coff.clear();
    sc.cur.clear();
    let mut sum = 0u32;
    for &c in &sc.ccnt {
        sc.coff.push(sum);
        sc.cur.push(sum);
        sum += c;
    }
    sc.coff.push(sum);
    sc.clk.clear();
    sc.clk.resize(sc.ctmp.len(), Clk { ts: 0, camp: 0, ptr: 0 });
    for j in 0..sc.ctmp.len() {
        let i = sc.cidx[j] as usize;
        sc.clk[sc.cur[i] as usize] = sc.ctmp[j];
        sc.cur[i] += 1;
    }
    sc.purch.sort_unstable();

    let organic = ctx.camp_names.len();
    let mut pcur = 0usize;
    for i in 0..np {
        let (a, b) = (sc.off[i] as usize, sc.off[i + 1] as usize);
        sort_u32(&mut sc.ts[a..b], &mut sc.radix);
        let tl = b - a;
        let pstart = pcur;
        while pcur < sc.purch.len() && (sc.purch[pcur].0 >> 32) as usize == i {
            pcur += 1;
        }
        if tl == 0 {
            continue;
        }
        // sessions
        sc.sess.clear();
        {
            let t = &sc.ts[a..a + tl];
            let mut start = t[0];
            let mut prev = t[0];
            let mut cnt = 0u32;
            for &x in t {
                if x - prev > GAP {
                    sc.sess.push(Sess { start, end: prev, n: cnt });
                    start = x;
                    cnt = 0;
                }
                cnt += 1;
                prev = x;
            }
            sc.sess.push(Sess { start, end: prev, n: cnt });
        }
        let (ca, cb) = (sc.coff[i] as usize, sc.coff[i + 1] as usize);
        if cb - ca > 1 {
            let c = &mut sc.clk[ca..cb];
            if c.len() <= 16 {
                for i in 1..c.len() {
                    let x = c[i];
                    let mut j = i;
                    while j > 0 && c[j - 1].ts > x.ts {
                        c[j] = c[j - 1];
                        j -= 1;
                    }
                    c[j] = x;
                }
            } else {
                c.sort_unstable_by_key(|c| c.ts);
            }
        }
        let cl = &sc.clk[ca..cb];
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut pj = pstart;
        let mut cur_day = u32::MAX;
        let (mut d_sess, mut d_events, mut d_rev) = (0u64, 0u64, 0u64);
        let nsess = sc.sess.len();
        for si in 0..=nsess {
            let day = if si < nsess { ((sc.sess[si].start as u64 + day_off) / DAY as u64) as u32 } else { u32::MAX };
            if day != cur_day {
                if cur_day != u32::MAX {
                    // flush the (person, day) row
                    sc.name.clear();
                    id_bytes(sc.keys[i], &mut sc.name);
                    sc.cols[0].push_str(&sc.name);
                    sc.cols[1].push_i32((day0 + cur_day as i64) as i32);
                    sc.cols[2].push_i64(d_sess as i64);
                    sc.cols[3].push_i64(d_events as i64);
                    sc.cols[4].push_i64(d_rev as i64);
                    let mut best = u32::MAX;
                    let mut best_sum = 0u64;
                    for &c in &sc.touched {
                        let s = sc.catsum[c as usize];
                        if s > best_sum || (s == best_sum && c < best) {
                            best = c;
                            best_sum = s;
                        }
                        sc.catsum[c as usize] = 0;
                    }
                    sc.touched.clear();
                    sc.cols[5].push_def(sc.rows, best != u32::MAX);
                    if best != u32::MAX {
                        sc.cols[5].push_str(&ctx.cats[best as usize]);
                    }
                    sc.rows += 1;
                    if sc.rows >= GROUP_ROWS {
                        sc.file.write_group(&mut sc.cols, sc.rows).expect("write user_daily");
                        sc.rows = 0;
                    }
                }
                if si == nsess {
                    break;
                }
                cur_day = day;
                d_sess = 0;
                d_events = 0;
                d_rev = 0;
            }
            let s = sc.sess[si];
            let mut rev = 0u64;
            while pj < pcur && (sc.purch[pj].0 as u32) <= s.end {
                let pc = sc.purch[pj].1;
                if pc != 0 {
                    let price = (pc % PRICE_MUL) as u64;
                    let cat = pc / PRICE_MUL;
                    rev += price;
                    if sc.catsum[cat as usize] == 0 {
                        sc.touched.push(cat);
                    }
                    sc.catsum[cat as usize] += price;
                }
                pj += 1;
            }
            let di = day as usize - 7;
            let mut camp = organic;
            if !cl.is_empty() {
                while hi < cl.len() && cl[hi].ts <= s.start {
                    let c = cl[hi].camp as usize;
                    if sc.acnt[c] == 0 {
                        sc.apos[c] = sc.active.len() as u32;
                        sc.active.push(c as u32);
                    }
                    sc.acnt[c] += 1;
                    hi += 1;
                }
                while lo < hi && (cl[lo].ts as u64 + WEEK as u64) < s.start as u64 {
                    let c = cl[lo].camp as usize;
                    sc.acnt[c] -= 1;
                    if sc.acnt[c] == 0 {
                        let pos = sc.apos[c] as usize;
                        let last = sc.active.pop().unwrap();
                        if last as usize != c {
                            sc.active[pos] = last;
                            sc.apos[last as usize] = pos as u32;
                        }
                    }
                    lo += 1;
                }
                if lo < hi {
                    let top = cl[hi - 1];
                    camp = top.camp as usize;
                    if hi - lo >= 2 && cl[hi - 2].ts == top.ts {
                        let mut best = unsafe { click_id(top.ptr) };
                        let mut k = hi - 1;
                        while k > lo && cl[k - 1].ts == top.ts {
                            k -= 1;
                            let id = unsafe { click_id(cl[k].ptr) };
                            if id < best {
                                best = id;
                                camp = cl[k].camp as usize;
                            }
                        }
                    }
                    for &c in &sc.active {
                        sc.acc[c as usize * nd + di][3] += 1;
                    }
                }
            }
            let acc = &mut sc.acc[camp * nd + di];
            acc[0] += 1;
            acc[1] += (rev > 0) as u64;
            acc[2] += rev;
            d_sess += 1;
            d_events += s.n as u64;
            d_rev += rev;
        }
        for k in lo..hi {
            sc.acnt[cl[k].camp as usize] = 0;
        }
        sc.active.clear();
    }
}

fn day_params(ctx: &Ctx) -> (usize, i64, u64) {
    let day0 = ctx.cbase.div_euclid(DAY);
    let day_off = ctx.cbase.rem_euclid(DAY) as u64;
    // event days run from index 7 to the day of `we - 1`
    let nd = if ctx.we > ctx.ws { ((ctx.we - 1 - ctx.cbase) as u64 + day_off) / DAY as u64 - 7 + 1 } else { 0 };
    (nd as usize, day0, day_off)
}

/// Returns each thread's `[campaign][day] -> [sessions, purchasing, revenue, assisted]`.
pub fn run(ctx: &Ctx, ev: &[Parts<PRec>], cl: &[Parts<CRec>], out: &Path, nthreads: usize) -> Vec<Vec<[u64; 4]>> {
    let (nd, day0, day_off) = day_params(ctx);
    let ncamp = ctx.camp_names.len();
    let np = 1usize << ctx.pbits;
    let dir = out.join("user_daily");
    let next = AtomicUsize::new(0);
    par((0..nthreads).collect::<Vec<usize>>(), |tid, _| {
        let file = PqFile::create(&dir.join(format!("part-{tid:03}.parquet")), &UD_SCHEMA).expect("create user_daily file");
        let mut sc = Scratch {
            slots: vec![(0, EMPTY); 1024],
            mask: 1023,
            keys: Vec::new(),
            cnt: Vec::new(),
            off: Vec::new(),
            cur: Vec::new(),
            pidx: Vec::new(),
            tstmp: Vec::new(),
            ts: Vec::new(),
            purch: Vec::new(),
            ccnt: Vec::new(),
            coff: Vec::new(),
            cidx: Vec::new(),
            ctmp: Vec::new(),
            clk: Vec::new(),
            sess: Vec::new(),
            acnt: vec![0; ncamp],
            apos: vec![0; ncamp],
            active: Vec::new(),
            catsum: vec![0; ctx.cats.len()],
            touched: Vec::new(),
            acc: vec![[0; 4]; (ncamp + 1) * nd.max(1)],
            radix: Vec::new(),
            name: Vec::new(),
            cols: UD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect(),
            rows: 0,
            file,
        };
        loop {
            let q = next.fetch_add(1, Relaxed);
            if q >= np {
                break;
            }
            do_part(ctx, q, ev, cl, &mut sc, nd, day0, day_off);
        }
        sc.file.write_group(&mut sc.cols, sc.rows).expect("write user_daily");
        sc.file.finish().expect("finish user_daily");
        sc.acc
    })
}

pub fn write_campaign_daily(ctx: &Ctx, accs: &[Vec<[u64; 4]>], out: &Path) {
    let (nd, day0, _) = day_params(ctx);
    let ncamp = ctx.camp_names.len();
    let mut cols: Vec<Col> = CD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect();
    let mut rows = 0;
    for c in 0..=ncamp {
        for d in 0..nd {
            let mut t = [0u64; 4];
            for a in accs {
                for k in 0..4 {
                    t[k] += a[c * nd + d][k];
                }
            }
            if t[0] == 0 && t[3] == 0 {
                continue;
            }
            if c == ncamp {
                cols[0].push_str(b"organic");
                cols[1].push_str(b"organic");
            } else {
                cols[0].push_str(&ctx.camp_names[c]);
                cols[1].push_str(&ctx.camp_channels[c]);
            }
            cols[2].push_i32((day0 + 7 + d as i64) as i32);
            for k in 0..4 {
                cols[3 + k].push_i64(t[k] as i64);
            }
            rows += 1;
        }
    }
    let mut f = PqFile::create(&out.join("campaign_daily").join("part-000.parquet"), &CD_SCHEMA).expect("create campaign_daily file");
    f.write_group(&mut cols, rows).expect("write campaign_daily");
    f.finish().expect("finish campaign_daily");
}
