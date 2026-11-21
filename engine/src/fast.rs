//! The fast pipeline: links -> parse (events, clicks) -> dedup -> per-person pass.

use crate::links::{LinkTable, Occ, part_of};
use crate::scan::*;
use crate::ts::*;
use crate::util::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

pub const PRICE_MUL: u32 = 10_000_001;
pub const MAX_CATS: usize = (u32::MAX / PRICE_MUL) as usize; // 429
pub const WEEK: i64 = 7 * DAY;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Scalar,
    Swar,
    #[cfg(target_arch = "aarch64")]
    Neon,
    #[cfg(target_arch = "x86_64")]
    Sse2,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "x86_64")]
    Avx512,
}

/// One well-formed event line for dedup: its event_id, ingest_ts, and where its `PRec` was
/// written (null when the copy is not a good event). A copy that loses is cancelled by
/// setting that `PRec`'s `ts` to `DEAD`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct DRec {
    pub eid: Id,
    pub ing: i64,
    pub prec: *mut PRec,
}
pub const DEAD: u32 = u32::MAX;

/// One good event: person, ts relative to
/// `window_start - 7 d`, and for a purchase `category * PRICE_MUL + price` (else 0).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct PRec {
    pub k: Id,
    pub ts: u32,
    pub pc: u32,
}

/// One valid click.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CRec {
    pub k: Id,
    pub ts: u32,
    pub camp: u32,
    pub ptr: usize,
}

pub struct Inputs {
    pub events: Vec<&'static [u8]>,
    pub clicks: Vec<&'static [u8]>,
    pub links: Vec<&'static [u8]>,
    pub products: &'static [u8],
    pub campaigns: &'static [u8],
    pub ws: i64,
    pub we: i64,
}

pub struct Ctx {
    pub ws: i64,
    pub we: i64,
    /// `ws - 7 d`: the zero of relative timestamps.
    pub cbase: i64,
    pub links: LinkTable,
    pub products: IdMap,
    pub unknown_cat: u32,
    pub cats: Vec<Vec<u8>>,
    pub campaigns: IdMap,
    pub camp_names: Vec<Vec<u8>>,
    pub camp_channels: Vec<Vec<u8>>,
    pub dbits: u32,
    pub pbits: u32,
    /// Debug knob (SE_STAGE): 1 = scan only, 2 = scan + evaluate, 0 = everything.
    pub stage: u32,
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("csv") && std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

fn manifest_value(m: &[u8], key: &str) -> i64 {
    let pat = format!("\"{key}\"");
    let pos = m.windows(pat.len()).position(|w| w == pat.as_bytes()).unwrap_or_else(|| die("manifest.json: missing key"));
    let rest = &m[pos + pat.len()..];
    let a = rest.iter().position(|&c| c == b'"').unwrap_or_else(|| die("manifest.json: bad value"));
    let rest = &rest[a + 1..];
    let b = rest.iter().position(|&c| c == b'"').unwrap_or_else(|| die("manifest.json: bad value"));
    let v = parse_ts_slice(&rest[..b]);
    if v == NONE {
        die("manifest.json: bad timestamp");
    }
    v
}

pub fn die(msg: &str) -> ! {
    eprintln!("shopping-engine: {msg}");
    std::process::exit(1);
}

pub fn load_inputs(data: &Path) -> Inputs {
    let map = |p: &Path| map_file(p).unwrap_or_else(|e| die(&format!("{}: {e}", p.display())));
    let maps = |d: &str| -> Vec<&'static [u8]> { list_csv(&data.join(d)).iter().map(|p| map(p)).collect() };
    let manifest = std::fs::read(data.join("manifest.json")).unwrap_or_else(|e| die(&format!("manifest.json: {e}")));
    Inputs {
        events: maps("events"),
        clicks: maps("clicks"),
        links: maps("links"),
        products: map(&data.join("products.csv")),
        campaigns: map(&data.join("campaigns.csv")),
        ws: manifest_value(&manifest, "window_start"),
        we: manifest_value(&manifest, "window_end"),
    }
}

/// Byte range of the lines that start in `[start, end)`; the file's first line (header) is never included.
fn bounds(data: &[u8], start: usize, end: usize) -> (usize, usize) {
    let len = data.len();
    let after_nl = |from: usize| -> usize {
        if from >= len {
            return len;
        }
        data[from..].iter().position(|&c| c == b'\n').map_or(len, |x| from + x + 1)
    };
    let s = after_nl(start.saturating_sub(1));
    let e = if end >= len { len } else { after_nl(end - 1) };
    (s, e.max(s))
}

struct Job {
    kind: u8, // 0 events, 1 clicks, 2 links
    data: &'static [u8],
    start: usize,
    end: usize,
}

fn make_jobs(files: &[&'static [u8]], kind: u8, chunk: usize, out: &mut Vec<Job>) {
    for &data in files {
        let mut s = 0;
        while s < data.len() {
            let e = (s + chunk).min(data.len());
            out.push(Job { kind, data, start: s, end: e });
            s = e;
        }
    }
}

macro_rules! dispatch {
    ($name:ident, $generic:ident, ($($arg:ident : $ty:ty),*)) => {
        unsafe fn $name(level: Level, $($arg: $ty),*) {
            unsafe {
                match level {
                    Level::Scalar => $generic::<Scalar>($($arg),*),
                    Level::Swar => $generic::<Swar>($($arg),*),
                    #[cfg(target_arch = "aarch64")]
                    Level::Neon => $generic::<Neon>($($arg),*),
                    #[cfg(target_arch = "x86_64")]
                    Level::Sse2 => $generic::<Sse2>($($arg),*),
                    #[cfg(target_arch = "x86_64")]
                    Level::Avx2 => {
                        #[target_feature(enable = "avx2,bmi1,bmi2,popcnt,lzcnt")]
                        unsafe fn w($($arg: $ty),*) {
                            unsafe { $generic::<Avx2>($($arg),*) }
                        }
                        w($($arg),*)
                    }
                    #[cfg(target_arch = "x86_64")]
                    Level::Avx512 => {
                        #[target_feature(enable = "avx512f,avx512bw,avx2,bmi1,bmi2,popcnt,lzcnt")]
                        unsafe fn w($($arg: $ty),*) {
                            unsafe { $generic::<Avx512>($($arg),*) }
                        }
                        w($($arg),*)
                    }
                }
            }
        }
    };
}

// ---------------------------------------------------------------- self test

#[inline(always)]
unsafe fn selftest_g<M: Masker>(buf: &mut ScanBuf, data: &[u8], out: &mut Vec<usize>) {
    unsafe {
        let b = data.as_ptr() as usize;
        scan::<M, 3>(buf, data.as_ptr(), data.len(), |f, row| {
            out.push(row as usize - b);
            for x in f {
                out.push(x.0 as usize - b);
                out.push(x.1);
            }
        });
    }
}
dispatch!(selftest_d, selftest_g, (buf: &mut ScanBuf, data: &[u8], out: &mut Vec<usize>));

fn selftest(level: Level) -> bool {
    let mut x = 0x1234_5678_9ABC_DEF1u64;
    let alpha = b"abc,,,,\"\"\n\n\nxyz01 ";
    for round in 0..3 {
        let n = [20_000usize, 20_031, 63][round];
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // a long line every so often, to cross piece boundaries
            let c = if (i / 3000) % 3 == 1 && x % 50 != 0 { b'q' } else { alpha[(x % alpha.len() as u64) as usize] };
            v.push(c);
        }
        if round == 0 {
            *v.last_mut().unwrap() = b'\n';
        }
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let mut buf = ScanBuf::new();
        unsafe {
            selftest_d(level, &mut buf, &v, &mut a);
            selftest_d(Level::Scalar, &mut buf, &v, &mut b);
        }
        if a != b {
            return false;
        }
    }
    true
}

pub fn pick_level() -> Level {
    let want = std::env::var("SE_SIMD").unwrap_or_default();
    #[allow(unused_assignments)]
    let mut level = Level::Swar;
    #[cfg(target_arch = "aarch64")]
    {
        level = Level::Neon;
    }
    #[cfg(target_arch = "x86_64")]
    {
        level = Level::Sse2;
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("bmi1") && is_x86_feature_detected!("bmi2") && is_x86_feature_detected!("popcnt") && is_x86_feature_detected!("lzcnt") {
            level = Level::Avx2;
            // AVX-512 is opt-in (SE_SIMD=avx512): the mask step is a small share of the parse
            // and 512-bit code can lower the core frequency. Measure before enabling.
            if want == "avx512" && is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
                level = Level::Avx512;
            }
        }
        if want == "sse2" {
            level = Level::Sse2;
        }
    }
    if want == "scalar" {
        level = Level::Scalar;
    }
    if want == "swar" {
        level = Level::Swar;
    }
    if level != Level::Scalar && !selftest(level) {
        eprintln!("shopping-engine: SIMD self-test failed for {level:?}; using the scalar scanner");
        level = Level::Scalar;
    }
    level
}

// ---------------------------------------------------------------- small tables

#[inline(always)]
unsafe fn pairs_g<M: Masker>(buf: &mut ScanBuf, data: &'static [u8], out: &mut Vec<(&'static [u8], &'static [u8])>) {
    unsafe {
        let (s, e) = bounds(data, 0, data.len());
        if s >= e {
            return;
        }
        scan::<M, 2>(buf, data.as_ptr().add(s), e - s, |f, _| {
            out.push((std::slice::from_raw_parts(f[0].0, f[0].1), std::slice::from_raw_parts(f[1].0, f[1].1)));
        });
    }
}
dispatch!(pairs_d, pairs_g, (buf: &mut ScanBuf, data: &'static [u8], out: &mut Vec<(&'static [u8], &'static [u8])>));

fn load_products(data: &'static [u8], level: Level) -> (IdMap, Vec<Vec<u8>>, u32) {
    let mut rows = Vec::new();
    unsafe { pairs_d(level, &mut ScanBuf::new(), data, &mut rows) };
    let lim = unsafe { data.as_ptr().add(data.len()) };
    // distinct categories in first-seen order, then their byte-wise ranks
    let mut seen: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
    let mut first: Vec<&[u8]> = vec![b"unknown"];
    seen.insert(b"unknown", 0);
    let mut prods: Vec<(Id, u32)> = Vec::with_capacity(rows.len());
    let mut last: (&[u8], u32) = (b"unknown", 0);
    for &(id, cat) in &rows {
        if id.is_empty() {
            continue;
        }
        if last.0 != cat {
            let n = first.len() as u32;
            let i = *seen.entry(cat).or_insert_with(|| {
                first.push(cat);
                n
            });
            last = (cat, i);
        }
        prods.push((unsafe { pack_id(id.as_ptr(), id.len(), lim) }, last.1));
    }
    let mut order: Vec<u32> = (0..first.len() as u32).collect();
    order.sort_unstable_by_key(|&i| first[i as usize]);
    let mut rank = vec![0u32; first.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i as usize] = r as u32;
    }
    let mut map = IdMap::new(prods.len());
    for (id, c) in prods {
        map.insert(id, rank[c as usize]);
    }
    let cats: Vec<Vec<u8>> = order.iter().map(|&i| first[i as usize].to_vec()).collect();
    (map, cats, rank[0])
}

fn load_campaigns(data: &'static [u8], level: Level) -> (IdMap, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut rows = Vec::new();
    unsafe { pairs_d(level, &mut ScanBuf::new(), data, &mut rows) };
    let lim = unsafe { data.as_ptr().add(data.len()) };
    let mut map = IdMap::new(rows.len());
    let (mut names, mut chans) = (Vec::new(), Vec::new());
    for (name, ch) in rows {
        if name.is_empty() {
            continue;
        }
        let id = unsafe { pack_id(name.as_ptr(), name.len(), lim) };
        if map.get(id) == u32::MAX {
            map.insert(id, names.len() as u32);
            names.push(name.to_vec());
            chans.push(ch.to_vec());
        }
    }
    (map, names, chans)
}

// ---------------------------------------------------------------- links

struct LState {
    tid: u32,
    buf: ScanBuf,
    occ: Writers<Occ>,
    edges: Vec<[u32; 2]>,
}

#[inline(always)]
unsafe fn links_g<M: Masker>(st: &mut LState, job: &Job, bits: u32) {
    unsafe {
        let (s, e) = bounds(job.data, job.start, job.end);
        if s >= e {
            return;
        }
        let lim = job.data.as_ptr().add(job.data.len());
        let LState { tid, buf, occ, edges } = st;
        scan::<M, 2>(buf, job.data.as_ptr().add(s), e - s, |f, _| {
            if f[0].1 == 0 || f[1].1 == 0 {
                return;
            }
            let a = pack_id(f[0].0, f[0].1, lim);
            let b = pack_id(f[1].0, f[1].1, lim);
            if a == b {
                return;
            }
            let edge = edges.len() as u32;
            edges.push([0, 0]);
            occ.push(part_of(id_hash(a), bits), Occ { id: a, edge, who: *tid << 1 });
            occ.push(part_of(id_hash(b), bits), Occ { id: b, edge, who: (*tid << 1) | 1 });
        });
    }
}
dispatch!(links_d, links_g, (st: &mut LState, job: &Job, bits: u32));

fn build_links(files: &[&'static [u8]], level: Level, nthreads: usize) -> Option<LinkTable> {
    let bytes: usize = files.iter().map(|f| f.len()).sum();
    if bytes == 0 {
        return Some(LinkTable::empty());
    }
    let est_occ = bytes / 10;
    let bits = ceil_log2(est_occ / 32_768).min(9);
    let mut jobs = Vec::new();
    make_jobs(files, 2, chunk_bytes(1 << 20), &mut jobs);
    let nt = nthreads.min(jobs.len().max(1));
    let next = AtomicUsize::new(0);
    let states: Vec<LState> = (0..nt)
        .map(|t| LState { tid: t as u32, buf: ScanBuf::new(), occ: Writers::new(1 << bits, 512), edges: Vec::new() })
        .collect();
    let res = par(states, |_, mut st| {
        loop {
            let j = next.fetch_add(1, Relaxed);
            if j >= jobs.len() {
                break;
            }
            unsafe { links_d(level, &mut st, &jobs[j], bits) };
        }
        (st.occ.finish(), st.edges)
    });
    let (occ, mut edges): (Vec<_>, Vec<_>) = res.into_iter().unzip();
    LinkTable::build(&occ, &mut edges, bits, nthreads)
}

/// Work-queue chunk size; SE_CHUNK overrides it (testing).
fn chunk_bytes(default: usize) -> usize {
    std::env::var("SE_CHUNK").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0).unwrap_or(default)
}

pub fn ceil_log2(x: usize) -> u32 {
    if x <= 1 { 0 } else { usize::BITS - (x - 1).leading_zeros() }
}

// ---------------------------------------------------------------- events and clicks

pub struct EvOut {
    /// 0: the line is dropped before dedup; 1: takes part in dedup but is not a good event; 2: good.
    pub status: u8,
    pub uid: Id,
    pub ts: u32,
    pub pc: u32,
    pub ing: i64,
}

const VIEW: u32 = u32::from_le_bytes(*b"view");
const CART: u32 = u32::from_le_bytes(*b"cart");
const PURCHASE: u64 = u64::from_le_bytes(*b"purchase");

/// Everything the rules say about one well-formed event line.
#[inline(always)]
pub unsafe fn eval_event(ctx: &Ctx, f: &[Field; 7], lim: *const u8, cache: &mut DateCache) -> EvOut {
    unsafe {
        let mut o = EvOut { status: 0, uid: 0, ts: 0, pc: 0, ing: 0 };
        if f[0].1 == 0 {
            return o;
        }
        o.ing = parse_ts(f[3].0, f[3].1, cache);
        if o.ing == NONE {
            return o; // out of spec ("ingest_ts is always valid F1"): treated as a malformed line
        }
        o.status = 1;
        let ts = parse_ts(f[2].0, f[2].1, cache);
        if !(f[1].1 != 0 && ts != NONE && ts >= ctx.ws && ts < ctx.we && o.ing - ts <= DAY && f[5].1 != 0) {
            return o;
        }
        let (tp, tl) = f[4];
        if tl == 4 {
            let w = (tp as *const u32).read_unaligned();
            if w != VIEW && w != CART {
                return o;
            }
        } else if tl == 8 && (tp as *const u64).read_unaligned() == PURCHASE {
            let price = parse_price(f[6].0, f[6].1);
            if price == 0 {
                return o;
            }
            let mut cat = ctx.products.get(pack_id(f[5].0, f[5].1, lim));
            if cat == u32::MAX {
                cat = ctx.unknown_cat;
            }
            o.pc = cat * PRICE_MUL + price;
        } else {
            return o;
        }
        o.uid = pack_id(f[1].0, f[1].1, lim);
        o.ts = (ts - ctx.cbase) as u32;
        o.status = 2;
        o
    }
}

#[derive(Clone, Copy)]
struct PendE {
    uid: Id,
    h: u64,
    ts: u32,
    pc: u32,
    eid: Id,
    ing: i64,
}
#[derive(Clone, Copy)]
struct PendC {
    uid: Id,
    h: u64,
    ts: u32,
    camp: u32,
    ptr: usize,
}
const RING: usize = 8;

struct PState {
    buf: ScanBuf,
    cache: DateCache,
    dw: Writers<DRec>,
    pw: Writers<PRec>,
    cw: Writers<CRec>,
}

#[inline(always)]
fn emit_event(ctx: &Ctx, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, p: &PendE) {
    let (k, h) = if ctx.links.any {
        let k = ctx.links.resolve(p.uid, p.h);
        (k, if k == p.uid { p.h } else { id_hash(k) })
    } else {
        (p.uid, p.h)
    };
    let prec = pw.push_ptr(part_of(h, ctx.pbits), PRec { k, ts: p.ts, pc: p.pc });
    dw.push(part_of(id_hash(p.eid), ctx.dbits), DRec { eid: p.eid, ing: p.ing, prec });
}

#[inline(always)]
unsafe fn main_g<M: Masker>(ctx: &Ctx, st: &mut PState, job: &Job) {
    unsafe {
        let (s, e) = bounds(job.data, job.start, job.end);
        if s >= e {
            return;
        }
        let base = job.data.as_ptr().add(s);
        let lim = job.data.as_ptr().add(job.data.len());
        populate_read(base, e - s);
        let PState { buf, cache, dw, pw, cw } = st;
        if job.kind == 0 {
            let mut ring = [PendE { uid: 0, h: 0, ts: 0, pc: 0, eid: 0, ing: 0 }; RING];
            let mut rn = 0usize;
            let stage = ctx.stage;
            scan::<M, 7>(buf, base, e - s, |f, _| {
                if stage == 1 {
                    return;
                }
                let r = eval_event(ctx, f, lim, cache);
                if stage == 2 {
                    std::hint::black_box(&r);
                    return;
                }
                if r.status == 0 {
                    return;
                }
                let eid = pack_id(f[0].0, f[0].1, lim);
                if stage == 3 {
                    std::hint::black_box(id_hash(eid) ^ id_hash(r.uid));
                    return;
                }
                if stage == 4 {
                    if r.status == 2 {
                        std::hint::black_box(ctx.links.resolve(r.uid, id_hash(r.uid)));
                    }
                    return;
                }
                if r.status == 2 {
                    let hu = id_hash(r.uid);
                    let slot = rn % RING;
                    if rn >= RING {
                        emit_event(ctx, pw, dw, &ring[slot]);
                    }
                    if ctx.links.any {
                        ctx.links.prefetch(hu);
                    }
                    ring[slot] = PendE { uid: r.uid, h: hu, ts: r.ts, pc: r.pc, eid, ing: r.ing };
                    rn += 1;
                } else {
                    dw.push(part_of(id_hash(eid), ctx.dbits), DRec { eid, ing: r.ing, prec: std::ptr::null_mut() });
                }
            });
            for p in &ring[..rn.min(RING)] {
                emit_event(ctx, pw, dw, p);
            }
        } else {
            let mut ring = [PendC { uid: 0, h: 0, ts: 0, camp: 0, ptr: 0 }; RING];
            let mut rn = 0usize;
            let mut emit = |p: PendC| {
                let (k, h) = if ctx.links.any {
                    let k = ctx.links.resolve(p.uid, p.h);
                    (k, if k == p.uid { p.h } else { id_hash(k) })
                } else {
                    (p.uid, p.h)
                };
                cw.push(part_of(h, ctx.pbits), CRec { k, ts: p.ts, camp: p.camp, ptr: p.ptr });
            };
            scan::<M, 4>(buf, base, e - s, |f, row| {
                if f[0].1 == 0 || f[1].1 == 0 || f[2].1 == 0 {
                    return;
                }
                let ts = parse_ts(f[3].0, f[3].1, cache);
                if ts == NONE || ts < ctx.cbase || ts >= ctx.we {
                    return;
                }
                let camp = ctx.campaigns.get(pack_id(f[2].0, f[2].1, lim));
                if camp == u32::MAX {
                    return;
                }
                let uid = pack_id(f[1].0, f[1].1, lim);
                let hu = id_hash(uid);
                let slot = rn % RING;
                if rn >= RING {
                    emit(ring[slot]);
                }
                if ctx.links.any {
                    ctx.links.prefetch(hu);
                }
                ring[slot] = PendC { uid, h: hu, ts: (ts - ctx.cbase) as u32, camp, ptr: row as usize };
                rn += 1;
            });
            for p in &ring[..rn.min(RING)] {
                emit(*p);
            }
        }
    }
}
dispatch!(main_d, main_g, (ctx: &Ctx, st: &mut PState, job: &Job));

pub fn run(inp: &Inputs, out: &Path, nthreads: usize, tm: &mut Timer) -> bool {
    let level = pick_level();
    let cbase = inp.ws - WEEK;
    if inp.we.saturating_sub(cbase) >= u32::MAX as i64 || inp.we < inp.ws {
        return false;
    }
    tm.lap("setup");
    // The two small tables load on their own thread while the links are built.
    let (links, (products, cats, unknown_cat), (campaigns, camp_names, camp_channels)) = std::thread::scope(|sc| {
        let h = sc.spawn(|| (load_products(inp.products, level), load_campaigns(inp.campaigns, level)));
        let links = build_links(&inp.links, level, nthreads);
        let (p, c) = h.join().unwrap();
        (links, p, c)
    });
    // Fast-path preconditions (anything else goes to the general path in slow.rs):
    // category * PRICE_MUL + price fits u32; the dense [campaign][day] counters stay small;
    // the link table's slot numbers fit u32.
    let days = ((inp.we - inp.ws) / DAY + 2) as usize;
    if cats.len() > MAX_CATS || (camp_names.len() + 1).saturating_mul(days) > (4 << 20) {
        return false;
    }
    let Some(links) = links else { return false };
    tm.lap("links+tables");

    let ev_bytes: usize = inp.events.iter().map(|f| f.len()).sum();
    let est_rows = ev_bytes / 75;
    let dbits = ceil_log2(est_rows / 65_536).min(10);
    let pbits = ceil_log2(est_rows / 50_000).min(10);
    let ctx = Ctx { ws: inp.ws, we: inp.we, cbase, links, products, unknown_cat, cats, campaigns, camp_names, camp_channels, dbits, pbits, stage: std::env::var("SE_STAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(0) };

    if tm.on {
        eprintln!(
            "[timing] threads={nthreads} simd={level:?} event_bytes={ev_bytes} dedup_parts={} person_parts={} categories={} campaigns={} links={}",
            1usize << dbits,
            1usize << pbits,
            ctx.cats.len(),
            ctx.camp_names.len(),
            ctx.links.any
        );
    }
    let mut jobs = Vec::new();
    let chunk = chunk_bytes(2 << 20);
    make_jobs(&inp.events, 0, chunk, &mut jobs);
    make_jobs(&inp.clicks, 1, chunk, &mut jobs);
    // Largest first is not needed: chunks are uniform. Interleave so that clicks spread out.
    let nt = nthreads.min(jobs.len().max(1));
    let next = AtomicUsize::new(0);
    let states: Vec<PState> = (0..nt)
        .map(|_| PState {
            buf: ScanBuf::new(),
            cache: DateCache::new(),
            dw: Writers::new(1 << dbits, 1024),
            pw: Writers::new(1 << pbits, 512),
            cw: Writers::new(1 << pbits, 128),
        })
        .collect();
    let res = par(states, |_, mut st| {
        loop {
            let j = next.fetch_add(1, Relaxed);
            if j >= jobs.len() {
                break;
            }
            unsafe { main_d(level, &ctx, &mut st, &jobs[j]) };
        }
        (st.dw.finish(), st.pw.finish(), st.cw.finish())
    });
    let mut dparts = Vec::new();
    let mut ev = Vec::new();
    let mut cl = Vec::new();
    for (d, p, c) in res {
        dparts.push(d);
        ev.push(p);
        cl.push(c);
    }
    tm.lap("parse events+clicks");

    // Dedup: per partition, an open-addressing table of pointers to the records (16 hash bits
    // in the top of each slot); equality is on the exact event_id. The copy with the smaller
    // ingest_ts stays; the other one's PRec, if any, is marked dead.
    let np = 1usize << dbits;
    let next = AtomicUsize::new(0);
    let dparts_ref = &dparts;
    par((0..nthreads).collect::<Vec<usize>>(), |_, _| {
        let mut table: Vec<u64> = Vec::new();
        loop {
            let p = next.fetch_add(1, Relaxed);
            if p >= np {
                break;
            }
            let n: usize = dparts_ref.iter().map(|d| d.count(p)).sum();
            if n < 2 {
                continue;
            }
            let cap = (n * 2).next_power_of_two();
            table.clear();
            table.resize(cap, 0);
            let mask = cap - 1;
            for d in dparts_ref {
                for sl in d.slices(p) {
                    for rec in sl.iter_mut() {
                        let h = id_hash(rec.eid);
                        let tag = (h >> 32) << 48;
                        let me = (rec as *mut DRec as u64) | tag;
                        let mut i = h as usize & mask;
                        loop {
                            let s = unsafe { *table.get_unchecked(i) };
                            if s == 0 {
                                unsafe { *table.get_unchecked_mut(i) = me };
                                break;
                            }
                            if s & (0xFFFF << 48) == tag {
                                let other = unsafe { &*((s & ((1 << 48) - 1)) as *const DRec) };
                                if other.eid == rec.eid {
                                    let loser = if rec.ing < other.ing {
                                        table[i] = me;
                                        other.prec
                                    } else {
                                        rec.prec
                                    };
                                    if !loser.is_null() {
                                        unsafe { (*loser).ts = DEAD };
                                    }
                                    break;
                                }
                            }
                            i = (i + 1) & mask;
                        }
                    }
                }
            }
        }
    });
    tm.lap("dedup");

    let accs = crate::person::run(&ctx, &ev, &cl, out, nthreads);
    tm.lap("person pass + user_daily");
    crate::person::write_campaign_daily(&ctx, &accs, out);
    tm.lap("campaign_daily");
    true
}
