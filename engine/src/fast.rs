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

/// One well-formed event line for dedup: its event_id, ingest_ts (whole seconds after
/// `Ctx::ibase`), and where its `PRec` was written: an index into `Ctx::pregion`, or `NOPREC`
/// when the copy is not a good event. A copy that loses is cancelled by setting that `PRec`'s
/// `ts` to `DEAD`. A line that does not fit this form (ingest_ts not on a whole second or
/// 68 years from the window, `PRec` outside the region) sets `PState::escape`, which sends the
/// whole run to the general path.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct DRec {
    pub eid: Id,
    pub ing: u32,
    pub prec: u32,
}
pub const NOPREC: u32 = u32::MAX;
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
    /// Drop each chunk's file pages from the mapping once it is parsed (SE_DROP=0 turns it off).
    pub drop_pages: bool,
    /// Zero of `DRec::ing`: `ws` - 2^31 s, in ms.
    pub ibase: i64,
    /// Where every `PRec` lives.
    pub pregion: &'static Region,
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
    /// Descriptor to `pread` the chunk from, or -1 to parse the mapping in place.
    fd: i32,
    data: &'static [u8],
    start: usize,
    end: usize,
}

fn make_jobs(files: &[&'static [u8]], kind: u8, chunk: usize, read: bool, out: &mut Vec<Job>) {
    for &data in files {
        let fd = if read { fd_of(data) } else { -1 };
        let mut s = 0;
        while s < data.len() {
            let e = (s + chunk).min(data.len());
            out.push(Job { kind, fd, data, start: s, end: e });
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
    make_jobs(files, 2, chunk_bytes(1 << 20), false, &mut jobs);
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

fn env_usize(k: &str) -> Option<usize> {
    std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok())
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

/// Records per block of the person partitions.
const PBLOCK: usize = 512;

/// Slack read past a chunk's end to find the end of its last line.
const READ_SLACK: usize = 1 << 16;

/// Reads the job's lines into `rbuf` (capacity `cap`). Returns (first line, byte length, file
/// offset of the first line), or None when the mapping must be used instead: a read error, or
/// a line too long for the slack.
unsafe fn read_job(job: &Job, rbuf: *mut u8, cap: usize) -> Option<(*const u8, usize, usize)> {
    unsafe {
        let flen = job.data.len();
        let rs = job.start.saturating_sub(1);
        let n = (job.end - rs + READ_SLACK).min(flen - rs);
        if job.fd < 0 || n > cap || !pread_exact(job.fd, rbuf, n, rs) {
            return None;
        }
        let b = std::slice::from_raw_parts(rbuf, n);
        let at_eof = rs + n == flen;
        // Same lines as `bounds`: from after the first newline at or after `start - 1`
        // to after the first newline at or after `end - 1`.
        let s = match b.iter().position(|&c| c == b'\n') {
            Some(x) => x + 1,
            None if at_eof => n,
            None => return None,
        };
        let e = if job.end >= flen {
            n
        } else {
            let from = job.end - 1 - rs;
            match b[from..].iter().position(|&c| c == b'\n') {
                Some(x) => from + x + 1,
                None if at_eof => n,
                None => return None,
            }
        };
        let e = e.max(s);
        Some((rbuf.add(s) as *const u8, e - s, rs + s))
    }
}

unsafe impl Send for PState {}
struct PState {
    escape: bool,
    rbuf: *mut u8,
    rcap: usize,
    buf: ScanBuf,
    cache: DateCache,
    dw: Writers<DRec>,
    pw: Writers<PRec>,
    cw: Writers<CRec>,
}

/// `ingest_ts` as a `DRec::ing`; sets `escape` when it does not fit.
#[inline(always)]
fn ing32(ctx: &Ctx, ing: i64, escape: &mut bool) -> u32 {
    let d = ing.wrapping_sub(ctx.ibase);
    let s = d / 1000;
    *escape |= (d < 0) | (s > u32::MAX as i64) | (s * 1000 != d);
    s as u32
}

/// Writes one good event: its `PRec` goes to the partition of its *user ID* (the person is
/// filled in after dedup, by `resolve_people`), its `DRec` to the partition of its event ID.
#[inline(always)]
fn emit_event(ctx: &Ctx, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, escape: &mut bool, uid: Id, ts: u32, pc: u32, eid: Id, ing: u32) {
    let prec = pw.push_ptr(part_of(id_hash(uid), ctx.pbits), PRec { k: uid, ts, pc });
    let idx = (prec as usize).wrapping_sub(ctx.pregion.base) / std::mem::size_of::<PRec>();
    *escape |= idx >= NOPREC as usize;
    dw.push(part_of(id_hash(eid), ctx.dbits), DRec { eid, ing, prec: idx as u32 });
}

#[inline(always)]
unsafe fn main_g<M: Masker>(ctx: &Ctx, st: &mut PState, job: &Job) {
    unsafe {
        let PState { escape, rbuf, rcap, buf, cache, dw, pw, cw } = st;
        // `base[..len]`: the job's lines; `lim`: end of the readable bytes; `fix`: what to add
        // to a row pointer to get its address in the file's mapping (which outlives the job).
        let (base, len, lim, fix) = match read_job(job, *rbuf, *rcap) {
            Some((p, n, off)) => (p, n, p.add(n), (job.data.as_ptr() as usize + off).wrapping_sub(p as usize)),
            None => {
                let (s, e) = bounds(job.data, job.start, job.end);
                let base = job.data.as_ptr().add(s);
                populate_read(base, e.saturating_sub(s));
                (base, e.saturating_sub(s), job.data.as_ptr().add(job.data.len()), 0usize)
            }
        };
        if len == 0 {
            return;
        }
        let drop_after = ctx.drop_pages && fix == 0;
        if job.kind == 0 {
            let stage = ctx.stage;
            scan::<M, 7>(buf, base, len, #[inline(always)] |f, _| {
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
                let ing = ing32(ctx, r.ing, escape);
                if r.status == 2 {
                    emit_event(ctx, pw, dw, escape, r.uid, r.ts, r.pc, eid, ing);
                } else {
                    dw.push(part_of(id_hash(eid), ctx.dbits), DRec { eid, ing, prec: NOPREC });
                }
            });
        } else {
            scan::<M, 4>(buf, base, len, #[inline(always)] |f, row| {
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
                cw.push(part_of(id_hash(uid), ctx.pbits), CRec { k: uid, ts: (ts - ctx.cbase) as u32, camp, ptr: (row as usize).wrapping_add(fix) });
            });
        }
        if drop_after {
            drop_range(base, len);
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
    let dbits = env_usize("SE_DBITS").map_or(ceil_log2(est_rows / 65_536).min(10), |b| b.min(12) as u32);
    let pbits = env_usize("SE_PBITS").map_or(ceil_log2(est_rows / 50_000).min(10), |b| b.min(12) as u32);
    // Every PRec comes from one region, so that a DRec can name it in 32 bits. A good event
    // line has at least 16 bytes, so the records need at most `ev_bytes`; the rest is for the
    // open block of every (thread, partition) and each thread's open slab. If this were ever
    // too small the arenas fall back to private slabs and the run to the general path.
    let nt_max = nthreads.max(1);
    let pregion_bytes = ev_bytes + nt_max * ((PBLOCK * std::mem::size_of::<PRec>() + 64) << pbits) + (nt_max + 1) * (16 << 20);
    if pregion_bytes / std::mem::size_of::<PRec>() >= NOPREC as usize {
        return false;
    }
    let pregion = Region::new(pregion_bytes);
    let ctx = Ctx { ws: inp.ws, we: inp.we, cbase, links, products, unknown_cat, cats, campaigns, camp_names, camp_channels, dbits, pbits, stage: std::env::var("SE_STAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(0), drop_pages: env_on("SE_DROP"), ibase: inp.ws - (1i64 << 31) * 1000, pregion };

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
    // SE_READ=1: pread each chunk into a per-thread buffer instead of parsing the mapping.
    let read = env_flag("SE_READ");
    let chunk = chunk_bytes(2 << 20);
    let rcap = if read { chunk + READ_SLACK + 64 } else { 0 };
    make_jobs(&inp.events, 0, chunk, read, &mut jobs);
    make_jobs(&inp.clicks, 1, chunk, read, &mut jobs);
    // Largest first is not needed: chunks are uniform. Interleave so that clicks spread out.
    let nt = nthreads.min(jobs.len().max(1));
    let next = AtomicUsize::new(0);
    let wc = wc_default();
    let states: Vec<PState> = (0..nt)
        .map(|_| PState {
            escape: false,
            rbuf: if read { os_alloc(rcap) } else { std::ptr::null_mut() },
            rcap,
            buf: ScanBuf::new(),
            cache: DateCache::new(),
            dw: Writers::with_wc(1 << dbits, 1024, wc),
            pw: Writers::in_region(1 << pbits, PBLOCK, wc, pregion),
            cw: Writers::with_wc(1 << pbits, 128, wc),
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
        (st.escape, st.dw.finish(), st.pw.finish(), st.cw.finish())
    });
    if res.iter().any(|r| r.0) {
        // Some line did not fit the compact dedup record: the general path redoes everything.
        return false;
    }
    let mut dparts = Vec::new();
    let mut ev = Vec::new();
    let mut cl = Vec::new();
    for (_, d, p, c) in res {
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
    let pbase_addr = ctx.pregion.base;
    par((0..nthreads).collect::<Vec<usize>>(), |_, _| {
        let mut table: Vec<u64> = Vec::new();
        let pbase = pbase_addr as *mut PRec;
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
                                    if loser != NOPREC {
                                        unsafe { (*pbase.add(loser as usize)).ts = DEAD };
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

    // People. Until here a record sits in the partition of its user ID and carries that ID.
    // Now each partition's records get their person's name; a record whose person hashes to
    // another partition is cancelled in place and re-emitted there. The link table is
    // partitioned by the same hash bits, so a partition's lookups stay within a small,
    // cache-resident part of it (one random DRAM access per row when done while parsing).
    if ctx.links.any {
        let np = 1usize << ctx.pbits;
        let next = AtomicUsize::new(0);
        let (ev_ref, cl_ref) = (&ev, &cl);
        let moved = par((0..nthreads).collect::<Vec<usize>>(), |_, _| {
            let mut pw: Writers<PRec> = Writers::with_wc(np, PBLOCK, wc);
            let mut cw: Writers<CRec> = Writers::with_wc(np, 128, wc);
            loop {
                let q = next.fetch_add(1, Relaxed);
                if q >= np {
                    break;
                }
                for p in ev_ref {
                    for sl in p.slices(q) {
                        for r in sl.iter_mut() {
                            if r.ts == DEAD {
                                continue;
                            }
                            let k = ctx.links.resolve(r.k, id_hash(r.k));
                            let q2 = part_of(id_hash(k), ctx.pbits);
                            if q2 != q {
                                pw.push(q2, PRec { k, ts: r.ts, pc: r.pc });
                                r.ts = DEAD;
                            } else {
                                r.k = k;
                            }
                        }
                    }
                }
                for p in cl_ref {
                    for sl in p.slices(q) {
                        for r in sl.iter_mut() {
                            let k = ctx.links.resolve(r.k, id_hash(r.k));
                            let q2 = part_of(id_hash(k), ctx.pbits);
                            if q2 != q {
                                cw.push(q2, CRec { k, ts: r.ts, camp: r.camp, ptr: r.ptr });
                                r.ts = DEAD;
                            } else {
                                r.k = k;
                            }
                        }
                    }
                }
            }
            (pw.finish(), cw.finish())
        });
        for (p, c) in moved {
            ev.push(p);
            cl.push(c);
        }
        tm.lap("resolve people");
    }

    let accs = crate::person::run(&ctx, &ev, &cl, out, nthreads);
    tm.lap("person pass + user_daily");
    crate::person::write_campaign_daily(&ctx, &accs, out);
    tm.lap("campaign_daily");
    true
}
