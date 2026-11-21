//! People, after dedup. Two parallel passes:
//!
//! Pass A works on the partitions the parser left behind (by hash of the raw user ID). It
//! groups a partition's events and clicks by user ID in a small local table and asks the link
//! table once per distinct ID. An ID that is in no link line is a whole person: it is
//! sessionized, attributed and written right there. Every other ID's records leave as one
//! compact run (`name, timestamps, purchases, clicks`) for the partition of its person's name.
//!
//! Pass B works on those partitions: it groups the runs by person, merges them, and runs the
//! same per-person sweep.
//!
//! Both emit user_daily rows into per-thread Parquet files and per-thread campaign/day counters.

use crate::fast::{CRec, Ctx, DEAD, PRICE_MUL, PRec, WEEK};
use crate::links::part_of;
use crate::parquet::{Col, Kind, PqFile};
use crate::ts::DAY;
use crate::util::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const GAP: u32 = 1_800_000;
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
#[repr(C)]
struct Clk {
    ts: u32,
    camp: u32,
    ptr: usize,
}

/// A purchase: timestamp and `category * PRICE_MUL + price`.
#[derive(Clone, Copy)]
#[repr(C)]
struct Pu {
    ts: u32,
    pc: u32,
}

// ---------------------------------------------------------------- runs

/// Bytes of a run block. A run of at least a quarter of it gets a block of its own.
const RUN_BLOCK: usize = 16 << 10;
/// Run header: name u64, timestamps u32, purchases u32, clicks u32, flags u32. Then the
/// timestamps (u32, padded to 8 bytes), the purchases (`Pu`), the clicks (`Clk`).
const RUN_HDR: usize = 24;
/// Flag: the timestamps are not sorted.
const F_UNSORTED: u32 = 1;
/// Pass A sorts the timestamps of a run shorter than this; longer ones are sorted once, after
/// pass B has put the person's runs together.
const SORT_IN_A: usize = 2048;

#[inline(always)]
fn run_bytes(nts: usize, npu: usize, ncl: usize) -> usize {
    RUN_HDR + ((nts * 4 + 7) & !7) + npu * 8 + ncl * 16
}

/// One thread's runs, per destination partition. A run is contiguous in memory.
struct RunOut {
    /// (next free byte, end) of the open block
    heads: Vec<(usize, usize)>,
    /// start of the open block, or 0
    open: Vec<usize>,
    /// closed blocks: (start, used bytes)
    blocks: Vec<Vec<(usize, usize)>>,
    arena: Arena,
}
unsafe impl Send for RunOut {}

impl RunOut {
    fn new(parts: usize) -> Self {
        RunOut { heads: vec![(0, 0); parts], open: vec![0; parts], blocks: vec![Vec::new(); parts], arena: Arena::new() }
    }
    /// `bytes` (a multiple of 8) of 8-aligned space in partition `d`.
    #[inline(always)]
    fn alloc(&mut self, d: usize, bytes: usize) -> *mut u8 {
        let h = unsafe { self.heads.get_unchecked_mut(d) };
        if h.0 + bytes <= h.1 {
            let p = h.0;
            h.0 += bytes;
            return p as *mut u8;
        }
        self.alloc_slow(d, bytes)
    }
    #[cold]
    #[inline(never)]
    fn alloc_slow(&mut self, d: usize, bytes: usize) -> *mut u8 {
        if bytes >= RUN_BLOCK / 4 {
            let p = self.arena.alloc(bytes) as usize;
            self.blocks[d].push((p, bytes));
            return p as *mut u8;
        }
        self.close(d);
        let p = self.arena.alloc(RUN_BLOCK) as usize;
        self.open[d] = p;
        self.heads[d] = (p + bytes, p + RUN_BLOCK);
        p as *mut u8
    }
    fn close(&mut self, d: usize) {
        let st = self.open[d];
        if st != 0 && self.heads[d].0 > st {
            self.blocks[d].push((st, self.heads[d].0 - st));
        }
        self.open[d] = 0;
        self.heads[d] = (0, 0);
    }
    fn finish(mut self) -> Vec<Vec<(usize, usize)>> {
        for d in 0..self.heads.len() {
            self.close(d);
        }
        self.blocks
    }
}

// ---------------------------------------------------------------- per-thread output state

/// What a thread carries through both passes: its user_daily file and row buffers, its
/// campaign/day counters, and the state of the click window.
struct Out {
    nd: usize,
    day0: i64,
    day_off: u64,
    organic: usize,
    acc: Vec<[u64; 4]>,
    /// Assisted-session counts `[campaign][day]`, kept apart and narrow: about ten of these
    /// are bumped per session, at random places. Folded into `acc` by `fold_assists`.
    ass: Vec<u32>,
    /// Sessions counted into `ass` since the last fold (an upper bound).
    ass_n: u64,
    acnt: Vec<u32>,
    apos: Vec<u32>,
    active: Vec<u32>,
    catsum: Vec<u64>,
    touched: Vec<u32>,
    name: Vec<u8>,
    cols: Vec<Col>,
    rows: usize,
    file: PqFile,
}
unsafe impl Send for Out {}

impl Out {
    fn fold_assists(&mut self) {
        for (a, x) in self.acc.iter_mut().zip(self.ass.iter_mut()) {
            a[3] += *x as u64;
            *x = 0;
        }
        self.ass_n = 0;
    }
    /// Call before a partition that can add at most `n` sessions: no counter of `ass` can pass u32.
    fn guard(&mut self, n: usize) {
        if self.ass_n + n as u64 >= u32::MAX as u64 {
            self.fold_assists();
        }
        self.ass_n += n as u64;
    }
    #[inline(always)]
    fn flush_row(&mut self, ctx: &Ctx, name: Id, day: u32, sess: u64, events: u64, rev: u64) {
        if self.name.is_empty() {
            id_bytes(name, &mut self.name);
        }
        self.cols[0].push_str(&self.name);
        self.cols[1].push_i32((self.day0 + day as i64) as i32);
        self.cols[2].push_i64(sess as i64);
        self.cols[3].push_i64(events as i64);
        self.cols[4].push_i64(rev as i64);
        let mut best = u32::MAX;
        if !self.touched.is_empty() {
            let mut best_sum = 0u64;
            for &c in &self.touched {
                let s = self.catsum[c as usize];
                if s > best_sum || (s == best_sum && c < best) {
                    best = c;
                    best_sum = s;
                }
                self.catsum[c as usize] = 0;
            }
            self.touched.clear();
        }
        self.cols[5].push_def(self.rows, best != u32::MAX);
        if best != u32::MAX {
            self.cols[5].push_str(&ctx.cats[best as usize]);
        }
        self.rows += 1;
        if self.rows >= GROUP_ROWS {
            self.file.write_group(&mut self.cols, self.rows).expect("write user_daily");
            self.rows = 0;
        }
    }
}

/// One person: `t` = their events' timestamps, sorted, not empty; `pu` = their purchases
/// sorted by timestamp; `cl` = their clicks sorted by timestamp.
#[inline(always)]
fn person(ctx: &Ctx, o: &mut Out, name: Id, t: &[u32], pu: &[Pu], cl: &[Clk]) {
    let n = t.len();
    let (nd, organic, day_off) = (o.nd, o.organic, o.day_off);
    o.name.clear();
    let (mut lo, mut hi) = (0usize, 0usize);
    let mut pj = 0usize;
    let mut cur_day = u32::MAX;
    let (mut d_sess, mut d_events, mut d_rev) = (0u64, 0u64, 0u64);
    let mut i = 0usize;
    loop {
        // the next session: events i..j, from `start` to `end`
        let (mut start, mut end, mut cnt, mut day) = (0u32, 0u32, 0usize, u32::MAX);
        if i < n {
            unsafe {
                start = *t.get_unchecked(i);
                end = start;
                let mut j = i + 1;
                while j < n {
                    let x = *t.get_unchecked(j);
                    if x - end > GAP {
                        break;
                    }
                    end = x;
                    j += 1;
                }
                cnt = j - i;
                i = j;
            }
            day = ((start as u64 + day_off) / DAY as u64) as u32;
        }
        if day != cur_day {
            if cur_day != u32::MAX {
                o.flush_row(ctx, name, cur_day, d_sess, d_events, d_rev);
            }
            if day == u32::MAX {
                break;
            }
            cur_day = day;
            d_sess = 0;
            d_events = 0;
            d_rev = 0;
        }
        let mut rev = 0u64;
        while pj < pu.len() && pu[pj].ts <= end {
            let pc = pu[pj].pc;
            let price = (pc % PRICE_MUL) as u64;
            let cat = (pc / PRICE_MUL) as usize;
            rev += price;
            if o.catsum[cat] == 0 {
                o.touched.push(cat as u32);
            }
            o.catsum[cat] += price;
            pj += 1;
        }
        let di = day as usize - 7;
        let mut camp = organic;
        if !cl.is_empty() {
            while hi < cl.len() && cl[hi].ts <= start {
                let c = cl[hi].camp as usize;
                if o.acnt[c] == 0 {
                    o.apos[c] = o.active.len() as u32;
                    o.active.push(c as u32);
                }
                o.acnt[c] += 1;
                hi += 1;
            }
            while lo < hi && (cl[lo].ts as u64 + WEEK as u64) < start as u64 {
                let c = cl[lo].camp as usize;
                o.acnt[c] -= 1;
                if o.acnt[c] == 0 {
                    let pos = o.apos[c] as usize;
                    let last = o.active.pop().unwrap();
                    if last as usize != c {
                        o.active[pos] = last;
                        o.apos[last as usize] = pos as u32;
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
                for &c in &o.active {
                    unsafe { *o.ass.get_unchecked_mut(c as usize * nd + di) += 1 };
                }
            }
        }
        let acc = &mut o.acc[camp * nd + di];
        acc[0] += 1;
        acc[1] += (rev > 0) as u64;
        acc[2] += rev;
        d_sess += 1;
        d_events += cnt as u64;
        d_rev += rev;
    }
    for k in lo..hi {
        o.acnt[cl[k].camp as usize] = 0;
    }
    o.active.clear();
}

// ---------------------------------------------------------------- grouping scratch

struct Scratch {
    /// Open addressing `Id -> index`; key 0 = empty.
    slots: Vec<(Id, u32)>,
    mask: usize,
    keys: Vec<Id>,
    kslot: Vec<u32>,
    /// per key: events (pass A) or runs (pass B)
    cnt: Vec<u32>,
    /// per key: clicks
    ccnt: Vec<u32>,
    /// per key: the person's name, or 0 for an ID that is in no link line
    names: Vec<Id>,
    off: Vec<u32>,
    cur: Vec<u32>,
    pidx: Vec<u32>,
    tstmp: Vec<u32>,
    ts: Vec<u32>,
    tmp: Vec<u32>,
    bounds: Vec<u32>,
    purch: Vec<(u64, u32)>,
    pu: Vec<Pu>,
    coff: Vec<u32>,
    cidx: Vec<u32>,
    clk: Vec<Clk>,
    runs: Vec<(u32, usize)>,
    rp: Vec<usize>,
}

impl Scratch {
    fn new() -> Self {
        Scratch {
            slots: vec![(0, 0); 4096],
            mask: 4095,
            keys: Vec::new(),
            kslot: Vec::new(),
            cnt: Vec::new(),
            ccnt: Vec::new(),
            names: Vec::new(),
            off: Vec::new(),
            cur: Vec::new(),
            pidx: Vec::new(),
            tstmp: Vec::new(),
            ts: Vec::new(),
            tmp: Vec::new(),
            bounds: Vec::new(),
            purch: Vec::new(),
            pu: Vec::new(),
            coff: Vec::new(),
            cidx: Vec::new(),
            clk: Vec::new(),
            runs: Vec::new(),
            rp: Vec::new(),
        }
    }
    fn reset(&mut self) {
        for &s in &self.kslot {
            self.slots[s as usize].0 = 0;
        }
        self.keys.clear();
        self.kslot.clear();
        self.cnt.clear();
        self.ccnt.clear();
    }
    #[inline(always)]
    fn get_or_insert(&mut self, k: Id) -> u32 {
        let mut i = id_hash(k) as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.0 == k {
                return s.1;
            }
            if s.0 == 0 {
                break;
            }
            i = (i + 1) & self.mask;
        }
        self.insert_at(i, k)
    }
    #[inline(never)]
    fn insert_at(&mut self, i: usize, k: Id) -> u32 {
        let idx = self.keys.len() as u32;
        self.slots[i] = (k, idx);
        self.keys.push(k);
        self.kslot.push(i as u32);
        self.cnt.push(0);
        self.ccnt.push(0);
        if self.keys.len() * 2 > self.slots.len() {
            self.grow();
        }
        idx
    }
    #[cold]
    fn grow(&mut self) {
        let cap = self.slots.len() * 2;
        self.slots.clear();
        self.slots.resize(cap, (0, 0));
        self.mask = cap - 1;
        for (idx, &k) in self.keys.iter().enumerate() {
            let mut i = id_hash(k) as usize & self.mask;
            while self.slots[i].0 != 0 {
                i = (i + 1) & self.mask;
            }
            self.slots[i] = (k, idx as u32);
            self.kslot[idx] = i as u32;
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


#[inline]
fn sort_clicks(c: &mut [Clk]) {
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

/// Merges the sorted runs `t[bounds[i]..bounds[i + 1]]` (bounds starts with 0 and ends with
/// `t.len()`) into one sorted sequence in `t`.
fn merge_runs(t: &mut Vec<u32>, tmp: &mut Vec<u32>, bounds: &mut Vec<u32>) {
    let n = t.len();
    tmp.clear();
    tmp.resize(n, 0);
    while bounds.len() > 2 {
        let nb = bounds.len() - 1;
        let mut w = 1;
        let mut r = 0;
        while r < nb {
            let a0 = bounds[r] as usize;
            if r + 1 < nb {
                let (a1, b1) = (bounds[r + 1] as usize, bounds[r + 2] as usize);
                let (mut i, mut j, mut o) = (a0, a1, a0);
                unsafe {
                    while i < a1 && j < b1 {
                        let (x, y) = (*t.get_unchecked(i), *t.get_unchecked(j));
                        let tb = y < x;
                        *tmp.get_unchecked_mut(o) = if tb { y } else { x };
                        i += !tb as usize;
                        j += tb as usize;
                        o += 1;
                    }
                }
                tmp[o..o + (a1 - i)].copy_from_slice(&t[i..a1]);
                o += a1 - i;
                tmp[o..o + (b1 - j)].copy_from_slice(&t[j..b1]);
                bounds[w] = b1 as u32;
                r += 2;
            } else {
                let b0 = bounds[r + 1] as usize;
                tmp[a0..b0].copy_from_slice(&t[a0..b0]);
                bounds[w] = b0 as u32;
                r += 1;
            }
            w += 1;
        }
        bounds.truncate(w);
        std::mem::swap(t, tmp);
    }
}

// ---------------------------------------------------------------- pass A

fn pass_a(ctx: &Ctx, q: usize, ev: &[Parts<PRec>], cl: &[Parts<CRec>], sc: &mut Scratch, o: &mut Out, runs: &mut RunOut) {
    let n: usize = ev.iter().map(|p| p.count(q)).sum();
    let nc: usize = cl.iter().map(|p| p.count(q)).sum();
    if n + nc == 0 {
        return;
    }
    o.guard(n);
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
    // clicks: count per ID
    sc.cidx.clear();
    sc.cidx.reserve(nc);
    {
        let ci = sc.cidx.as_mut_ptr();
        let mut m = 0usize;
        for p in cl {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    let idx = sc.get_or_insert(r.k);
                    unsafe {
                        *sc.ccnt.get_unchecked_mut(idx as usize) += 1;
                        *ci.add(m) = idx;
                    }
                    m += 1;
                }
            }
        }
        unsafe { sc.cidx.set_len(m) };
    }
    let nk = sc.keys.len();
    // one link lookup per distinct ID
    sc.names.clear();
    if ctx.links.any {
        const AHEAD: usize = 8;
        for i in 0..nk.min(AHEAD) {
            ctx.links.prefetch(id_hash(sc.keys[i]));
        }
        for i in 0..nk {
            if i + AHEAD < nk {
                ctx.links.prefetch(id_hash(sc.keys[i + AHEAD]));
            }
            let k = sc.keys[i];
            sc.names.push(ctx.links.lookup(k, id_hash(k)));
        }
    } else {
        sc.names.resize(nk, 0);
    }
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
    sc.ts.reserve(n);
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
    // clicks grouped by ID
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
    sc.clk.reserve(nc);
    unsafe {
        let ck = sc.clk.as_mut_ptr();
        let mut j = 0usize;
        for p in cl {
            for sl in p.slices(q) {
                for r in sl.iter() {
                    let i = *sc.cidx.get_unchecked(j) as usize;
                    j += 1;
                    let c = sc.cur.get_unchecked_mut(i);
                    *ck.add(*c as usize) = Clk { ts: r.ts, camp: r.camp, ptr: r.ptr };
                    *c += 1;
                }
            }
        }
        sc.clk.set_len(nc);
    }
    // purchases by (ID, timestamp)
    sc.pu.clear();
    if !sc.purch.is_empty() {
        sc.purch.sort_unstable();
        for &(k, pc) in &sc.purch {
            sc.pu.push(Pu { ts: k as u32, pc });
        }
    }
    let mut pcur = 0usize;
    for i in 0..nk {
        let (a, b) = (sc.off[i] as usize, sc.off[i + 1] as usize);
        let (ca, cb) = (sc.coff[i] as usize, sc.coff[i + 1] as usize);
        let pa = pcur;
        while pcur < sc.purch.len() && (sc.purch[pcur].0 >> 32) as usize == i {
            pcur += 1;
        }
        let name = sc.names[i];
        if name == 0 {
            // a person of their own
            if a == b {
                continue;
            }
            sort_u32(&mut sc.ts[a..b], &mut sc.tmp);
            sort_clicks(&mut sc.clk[ca..cb]);
            person(ctx, o, sc.keys[i], &sc.ts[a..b], &sc.pu[pa..pcur], &sc.clk[ca..cb]);
        } else {
            let (nts, npu, ncl) = (b - a, pcur - pa, cb - ca);
            let mut flags = 0;
            if nts < SORT_IN_A {
                sort_u32(&mut sc.ts[a..b], &mut sc.tmp);
            } else {
                flags = F_UNSORTED;
            }
            let d = part_of(id_hash(name), ctx.pbits);
            let p = runs.alloc(d, run_bytes(nts, npu, ncl));
            unsafe {
                (p as *mut u64).write(name);
                let h = p.add(8) as *mut u32;
                h.write(nts as u32);
                h.add(1).write(npu as u32);
                h.add(2).write(ncl as u32);
                h.add(3).write(flags);
                let mut w = p.add(RUN_HDR);
                std::ptr::copy_nonoverlapping(sc.ts.as_ptr().add(a), w as *mut u32, nts);
                w = w.add((nts * 4 + 7) & !7);
                std::ptr::copy_nonoverlapping(sc.pu.as_ptr().add(pa), w as *mut Pu, npu);
                w = w.add(npu * 8);
                std::ptr::copy_nonoverlapping(sc.clk.as_ptr().add(ca), w as *mut Clk, ncl);
            }
        }
    }
}

// ---------------------------------------------------------------- pass B

#[derive(Clone, Copy)]
struct Hdr {
    name: Id,
    nts: usize,
    npu: usize,
    ncl: usize,
    flags: u32,
}
#[inline(always)]
unsafe fn hdr(p: usize) -> Hdr {
    unsafe {
        let h = (p + 8) as *const u32;
        Hdr { name: *(p as *const u64), nts: *h as usize, npu: *h.add(1) as usize, ncl: *h.add(2) as usize, flags: *h.add(3) }
    }
}

fn pass_b(ctx: &Ctx, d: usize, blocks: &[Vec<Vec<(usize, usize)>>], sc: &mut Scratch, o: &mut Out) {
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
    o.guard(total);
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
    for i in 0..nk {
        let rs = &sc.rp[sc.off[i] as usize..sc.off[i + 1] as usize];
        let mut nts = 0usize;
        let (mut kts, mut kpu, mut kcl) = (0usize, 0usize, 0usize);
        let mut unsorted = false;
        for &r in rs {
            let h = unsafe { hdr(r) };
            nts += h.nts;
            kts += (h.nts > 0) as usize;
            kpu += (h.npu > 0) as usize;
            kcl += (h.ncl > 0) as usize;
            unsorted |= h.nts > 0 && h.flags & F_UNSORTED != 0;
        }
        if nts == 0 {
            continue;
        }
        let (mut t_one, mut pu_one, mut cl_one): (&[u32], &[Pu], &mut [Clk]) = (&[], &[], &mut []);
        let (direct_t, direct_pu, direct_cl) = (kts == 1 && !unsorted, kpu <= 1, kcl <= 1);
        if !direct_t {
            sc.ts.clear();
            sc.ts.reserve(nts);
            sc.bounds.clear();
            sc.bounds.push(0);
        }
        if !direct_pu {
            sc.pu.clear();
        }
        if !direct_cl {
            sc.clk.clear();
        }
        for &r in rs {
            let h = unsafe { hdr(r) };
            unsafe {
                let tp = (r + RUN_HDR) as *const u32;
                let pp = (r + RUN_HDR + ((h.nts * 4 + 7) & !7)) as *const Pu;
                let cp = (pp as usize + h.npu * 8) as *mut Clk;
                if h.nts > 0 {
                    let s = std::slice::from_raw_parts(tp, h.nts);
                    if direct_t {
                        t_one = s;
                    } else {
                        sc.ts.extend_from_slice(s);
                        sc.bounds.push(sc.ts.len() as u32);
                    }
                }
                if h.npu > 0 {
                    let s = std::slice::from_raw_parts(pp, h.npu);
                    if direct_pu {
                        pu_one = s;
                    } else {
                        sc.pu.extend_from_slice(s);
                    }
                }
                if h.ncl > 0 {
                    let s = std::slice::from_raw_parts_mut(cp, h.ncl);
                    if direct_cl {
                        cl_one = s;
                    } else {
                        sc.clk.extend_from_slice(s);
                    }
                }
            }
        }
        let t: &[u32] = if direct_t {
            t_one
        } else {
            if nts <= 24 || unsorted || kts > 8 {
                sort_u32(&mut sc.ts, &mut sc.tmp);
            } else {
                merge_runs(&mut sc.ts, &mut sc.tmp, &mut sc.bounds);
            }
            &sc.ts
        };
        let pu: &[Pu] = if direct_pu {
            pu_one
        } else {
            sc.pu.sort_unstable_by_key(|p| p.ts);
            &sc.pu
        };
        let cl: &[Clk] = if direct_cl {
            sort_clicks(cl_one);
            cl_one
        } else {
            sort_clicks(&mut sc.clk);
            &sc.clk
        };
        person(ctx, o, sc.keys[i], t, pu, cl);
    }
}

fn day_params(ctx: &Ctx) -> (usize, i64, u64) {
    let day0 = ctx.cbase.div_euclid(DAY);
    let day_off = ctx.cbase.rem_euclid(DAY) as u64;
    // event days run from index 7 to the day of `we - 1`
    let nd = if ctx.we > ctx.ws { ((ctx.we - 1 - ctx.cbase) as u64 + day_off) / DAY as u64 - 7 + 1 } else { 0 };
    (nd as usize, day0, day_off)
}


/// `0..n` ordered by descending `size`: the big partitions go first, so that none of them is
/// still running on one thread when the others have nothing left to do.
fn largest_first(n: usize, size: impl Fn(usize) -> usize) -> Vec<u32> {
    let mut v: Vec<(usize, u32)> = (0..n).map(|i| (size(i), i as u32)).collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v.into_iter().map(|x| x.1).collect()
}

/// Returns each thread's `[campaign][day] -> [sessions, purchasing, revenue, assisted]`.
pub fn run(ctx: &Ctx, ev: &[Parts<PRec>], cl: &[Parts<CRec>], out: &Path, nthreads: usize, tm: &mut Timer) -> Vec<Vec<[u64; 4]>> {
    let (nd, day0, day_off) = day_params(ctx);
    let ncamp = ctx.camp_names.len();
    let np = 1usize << ctx.pbits;
    let dir = out.join("user_daily");
    let order = largest_first(np, |q| ev.iter().map(|p| p.count(q)).sum::<usize>() * 2 + cl.iter().map(|p| p.count(q)).sum::<usize>() * 3);
    let next = AtomicUsize::new(0);
    let res = par((0..nthreads).collect::<Vec<usize>>(), |tid, _| {
        let file = PqFile::create(&dir.join(format!("part-{tid:03}.parquet")), &UD_SCHEMA).expect("create user_daily file");
        let mut o = Out {
            nd,
            day0,
            day_off,
            organic: ncamp,
            acc: vec![[0; 4]; (ncamp + 1) * nd.max(1)],
            ass: vec![0; (ncamp + 1) * nd.max(1)],
            ass_n: 0,
            acnt: vec![0; ncamp],
            apos: vec![0; ncamp],
            active: Vec::new(),
            catsum: vec![0; ctx.cats.len()],
            touched: Vec::new(),
            name: Vec::new(),
            cols: UD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect(),
            rows: 0,
            file,
        };
        let mut sc = Scratch::new();
        let mut runs = RunOut::new(np);
        loop {
            let j = next.fetch_add(1, Relaxed);
            if j >= np {
                break;
            }
            pass_a(ctx, order[j] as usize, ev, cl, &mut sc, &mut o, &mut runs);
        }
        (o, sc, runs.finish())
    });
    tm.lap("people A (by user id)");
    let mut states = Vec::new();
    let mut blocks = Vec::new();
    for (o, sc, b) in res {
        states.push((o, sc));
        blocks.push(b);
    }
    let blocks = &blocks;
    let order = largest_first(np, |d| blocks.iter().map(|b| b[d].iter().map(|x| x.1).sum::<usize>()).sum());
    let next = AtomicUsize::new(0);
    let accs = par(states, |_, (mut o, mut sc)| {
        loop {
            let j = next.fetch_add(1, Relaxed);
            if j >= np {
                break;
            }
            pass_b(ctx, order[j] as usize, blocks, &mut sc, &mut o);
        }
        o.file.write_group(&mut o.cols, o.rows).expect("write user_daily");
        o.fold_assists();
        o.file.finish().expect("finish user_daily");
        o.acc
    });
    tm.lap("people B (linked) + user_daily");
    accs
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
