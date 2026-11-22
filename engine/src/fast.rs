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
    /// Built on a background thread during parse and dedup; only the person pass reads it.
    pub links: std::sync::OnceLock<LinkTable>,
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
    /// Event lines by the row-hopping kernel (hop.rs) instead of the delimiter scanner.
    pub hop: bool,
}

/// Whether event lines take the row-hopping kernel: SE_KERNEL=hop / SE_KERNEL=scan decide;
/// by default it is used where it has its vector code (x86-64 built with AVX2).
pub fn hop_default(level: Level) -> bool {
    match std::env::var("SE_KERNEL").as_deref() {
        Ok("hop") => true,
        Ok("scan") => false,
        _ => {
            #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
            {
                matches!(level, Level::Avx2 | Level::Avx512)
            }
            #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
            {
                let _ = level;
                false
            }
        }
    }
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
    /// Event lines the row-hopping kernel handed to the general path.
    slow: u64,
    rbuf: *mut u8,
    rcap: usize,
    buf: ScanBuf,
    cache: DateCache,
    dates: crate::hop::HopDates,
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

/// One well-formed event line, by the general rules (`eval_event`), into the writers.
#[cold]
#[inline(never)]
pub(crate) unsafe fn event_general(ctx: &Ctx, f: &[Field; 7], lim: *const u8, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, escape: &mut bool) {
    unsafe {
        let stage = ctx.stage;
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
    }
}

/// What `event_row` needs besides the row and the writers.
pub(crate) struct EvRow<'a> {
    pub ctx: &'a Ctx,
    /// End of the readable bytes.
    pub lim: *const u8,
    /// `DRec::ing` is a whole number of seconds from `ibase` for every F1 `ingest_ts`.
    pub fast_rows: bool,
    pub ibase_s: i64,
}

/// One well-formed event line.
#[inline(always)]
unsafe fn event_row(ev: &EvRow, r: &Row, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, escape: &mut bool) {
    unsafe {
        let l0 = r.len(0);
        if l0 == 0 {
            return;
        }
        // Fast path: ingest_ts is a valid F1 and the line is not at the very end of
        // the buffer (so that 16 bytes can be read at every field without a check).
        // Everything else takes `event_general`, which is the plain statement of the rules.
        let ing_s = if (r.ln[3] == 20) & (r.ptr(6) as usize + r.len(6) + 24 <= ev.lim as usize) & ev.fast_rows { parse_f1(r.ptr(3), cache) } else { NONE };
        if ing_s == NONE {
            event_general(ev.ctx, &r.fields::<7>(), ev.lim, cache, pw, dw, escape);
            return;
        }
        let ing = ing_s * 1000;
        let (p2, l2) = (r.ptr(2), r.len(2));
        let ts = if l2 == 20 {
            let t = parse_f1(p2, cache);
            if t == NONE { parse_ts_cold(p2, l2, cache) } else { t * 1000 }
        } else if l2 == 13 {
            parse_f3_13(p2)
        } else {
            parse_ts_cold(p2, l2, cache)
        };
        // (a bad `ts` is NONE, which is below every window)
        let mut ok = (r.ln[1] != 0) & (ts >= ev.ctx.ws) & (ts < ev.ctx.we) & (ing.wrapping_sub(ts) <= DAY) & (r.ln[5] != 0);
        let (tp, tl) = (r.ptr(4), r.ln[4]);
        let mut pc = 0u32;
        if tl == 4 {
            let w = (tp as *const u32).read_unaligned();
            ok &= (w == VIEW) | (w == CART);
        } else if tl == 8 && (tp as *const u64).read_unaligned() == PURCHASE {
            if ok {
                let price = parse_price(r.ptr(6), r.len(6));
                let mut cat = ev.ctx.products.get(pack_id(r.ptr(5), r.len(5), ev.lim));
                if cat == u32::MAX {
                    cat = ev.ctx.unknown_cat;
                }
                pc = cat * PRICE_MUL + price;
                ok = price != 0;
            }
        } else {
            ok = false;
        }
        // Both IDs at once when they are short and plain (the rule); else one by one,
        // and the user ID only for a good event.
        let both = pack2_id16(r.ptr(0), l0, r.ptr(1), r.len(1));
        let eid = match both {
            Some(x) => x.0,
            None => pack_id16(r.ptr(0), l0),
        };
        let d = ing_s - ev.ibase_s;
        if ok {
            let uid = match both {
                Some(x) => x.1,
                None => pack_id16(r.ptr(1), r.len(1)),
            };
            let prec = pw.push_ptr(part_of(id_hash(uid), ev.ctx.pbits), PRec { k: uid, ts: (ts - ev.ctx.cbase) as u32, pc });
            let idx = (prec as usize).wrapping_sub(ev.ctx.pregion.base) / std::mem::size_of::<PRec>();
            *escape |= (idx >= NOPREC as usize) | (d as u64 > u32::MAX as u64);
            dw.push(part_of(id_hash(eid), ev.ctx.dbits), DRec { eid, ing: d as u32, prec: idx as u32 });
        } else {
            *escape |= d as u64 > u32::MAX as u64;
            dw.push(part_of(id_hash(eid), ev.ctx.dbits), DRec { eid, ing: d as u32, prec: NOPREC });
        }
    }
}

/// One well-formed click line (`row`: its first byte), by the general rules, into the writer.
#[inline(always)]
pub unsafe fn click_general(ctx: &Ctx, f: &[Field; 4], row: *const u8, lim: *const u8, fix: usize, cache: &mut DateCache, cw: &mut Writers<CRec>) {
    unsafe {
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
    }
}

#[inline(always)]
unsafe fn main_g<M: Masker>(ctx: &Ctx, st: &mut PState, job: &Job) {
    unsafe {
        let PState { escape, slow, rbuf, rcap, buf, cache, dates, dw, pw, cw } = st;
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
            // (SE_STAGE: 1 = scan only; 2, 3 = the general evaluator without scatter)
            let ev = EvRow { ctx, lim, fast_rows: ctx.ibase % 1000 == 0 && ctx.stage == 0, ibase_s: ctx.ibase / 1000 };
            if ctx.hop && ev.fast_rows {
                *slow += crate::hop::hop_events(&ev, base, len, dates, cache, pw, dw, escape);
            } else if ctx.stage == 1 {
                scan_rows::<M, 7>(buf, base, len, |r| {
                    std::hint::black_box(r);
                });
            } else {
                scan_rows::<M, 7>(buf, base, len, #[inline(always)] |r| event_row(&ev, r, cache, pw, dw, escape));
            }
        } else {
            if ctx.hop {
                crate::hop::hop_clicks(ctx, base, len, lim, fix, dates, cache, cw);
            } else {
                scan::<M, 4>(buf, base, len, #[inline(always)] |f, row| click_general(ctx, f, row, lim, fix, cache, cw));
            }
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
    let (products, cats, unknown_cat) = load_products(inp.products, level);
    let (campaigns, camp_names, camp_channels) = load_campaigns(inp.campaigns, level);
    // Fast-path preconditions (anything else goes to the general path in slow.rs):
    // category * PRICE_MUL + price fits u32; the dense [campaign][day] counters stay small;
    // the link table's slot numbers fit u32 (checked when the background build joins).
    let days = ((inp.we - inp.ws) / DAY + 2) as usize;
    if cats.len() > MAX_CATS || (camp_names.len() + 1).saturating_mul(days) > (4 << 20) {
        return false;
    }
    tm.lap("tables");
    std::thread::scope(|sc| {
        // Parse and dedup never look at links, so the link table builds alongside them.
        let lt = env_usize("SE_LTHREADS").unwrap_or(2).max(1);
        let lh = sc.spawn(move || build_links(&inp.links, level, lt));
        run_rest(inp, out, nthreads, tm, level, cbase, (products, cats, unknown_cat), (campaigns, camp_names, camp_channels), lh)
    })
}

#[allow(clippy::too_many_arguments)]
fn run_rest(
    inp: &Inputs,
    out: &Path,
    nthreads: usize,
    tm: &mut Timer,
    level: Level,
    cbase: i64,
    (products, cats, unknown_cat): (IdMap, Vec<Vec<u8>>, u32),
    (campaigns, camp_names, camp_channels): (IdMap, Vec<Vec<u8>>, Vec<Vec<u8>>),
    lh: std::thread::ScopedJoinHandle<'_, Option<LinkTable>>,
) -> bool {

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
    let ctx = Ctx { ws: inp.ws, we: inp.we, cbase, links: std::sync::OnceLock::new(), products, unknown_cat, cats, campaigns, camp_names, camp_channels, dbits, pbits, stage: std::env::var("SE_STAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(0), drop_pages: env_on("SE_DROP"), ibase: inp.ws - (1i64 << 31) * 1000, pregion, hop: hop_default(level) };

    if tm.on {
        eprintln!(
            "[timing] threads={nthreads} simd={level:?} hop={} event_bytes={ev_bytes} dedup_parts={} person_parts={} categories={} campaigns={} links={}",
            ctx.hop,
            1usize << dbits,
            1usize << pbits,
            ctx.cats.len(),
            ctx.camp_names.len(),
            !inp.links.is_empty()
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
            slow: 0,
            rbuf: if read { os_alloc(rcap) } else { std::ptr::null_mut() },
            rcap,
            buf: ScanBuf::new(),
            cache: DateCache::new(),
            dates: crate::hop::HopDates::new(),
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
    let Some(links) = lh.join().unwrap() else { return false };
    let _ = ctx.links.set(links);
    tm.lap("links (join)");

    // People: see person.rs. A record still sits in the partition of its user ID and carries
    // that ID; the per-person pass resolves the IDs itself, once per distinct ID.
    let accs = crate::person::run(&ctx, &ev, &cl, out, nthreads, tm);
    crate::person::write_campaign_daily(&ctx, &accs, out);
    tm.lap("campaign_daily");
    true
}

// ---------------------------------------------------------------- kernel microbenchmark

/// `--kernel-bench`: parses the event files only, with the delimiter scanner ("scan") and the
/// row-hopping kernel ("hop"), and prints for each run the wall time, the number of dedup
/// records, of good events, of lines that left the hop kernel, and an order-independent hash
/// of every record written (a `DRec` together with the `PRec` it names). Equal hashes and
/// counts mean both kernels emitted the same records.
/// KB_KERNELS=scan,hop  KB_THREADS=1,8  KB_REPS=2  KB_SIMD as SE_SIMD.
pub fn kernel_bench(inp: &Inputs, nthreads: usize) {
    let level = pick_level();
    let cbase = inp.ws - WEEK;
    let (products, cats, unknown_cat) = load_products(inp.products, level);
    let (campaigns, camp_names, camp_channels) = load_campaigns(inp.campaigns, level);
    let ev_bytes: usize = inp.events.iter().map(|f| f.len()).sum();
    let est_rows = ev_bytes / 75;
    let dbits = env_usize("SE_DBITS").map_or(ceil_log2(est_rows / 65_536).min(10), |b| b.min(12) as u32);
    let pbits = env_usize("SE_PBITS").map_or(ceil_log2(est_rows / 50_000).min(10), |b| b.min(12) as u32);
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let kernels = var("KB_KERNELS", "scan,hop");
    let threads: Vec<usize> = var("KB_THREADS", &format!("1,{nthreads}")).split(',').filter_map(|x| x.parse().ok()).collect();
    let reps = env_usize("KB_REPS").unwrap_or(2);
    let nt_max = threads.iter().copied().max().unwrap_or(1).max(1);
    let pregion_bytes = ev_bytes + nt_max * ((PBLOCK * std::mem::size_of::<PRec>() + 64) << pbits) + (nt_max + 1) * (16 << 20);
    let mut ctx = Ctx { ws: inp.ws, we: inp.we, cbase, links: std::sync::OnceLock::new(), products, unknown_cat, cats, campaigns, camp_names, camp_channels, dbits, pbits, stage: 0, drop_pages: false, ibase: inp.ws - (1i64 << 31) * 1000, pregion: Region::new(1 << 20), hop: false };
    let mut jobs = Vec::new();
    make_jobs(&inp.events, 0, chunk_bytes(2 << 20), false, &mut jobs);
    let wc = wc_default();
    eprintln!("kernel-bench: level={level:?} event_bytes={ev_bytes} jobs={}", jobs.len());
    let mut first: Option<(u64, u64, u64)> = None;
    let mut same = true;
    for &nt in &threads {
        for _ in 0..reps {
            for kernel in kernels.split(',') {
                ctx.hop = kernel == "hop";
                ctx.pregion = Region::new(pregion_bytes);
                let nt = nt.min(jobs.len().max(1));
                let next = AtomicUsize::new(0);
                let states: Vec<PState> = (0..nt)
                    .map(|_| PState { escape: false, slow: 0, rbuf: std::ptr::null_mut(), rcap: 0, buf: ScanBuf::new(), cache: DateCache::new(), dates: crate::hop::HopDates::new(), dw: Writers::with_wc(1 << dbits, 1024, wc), pw: Writers::in_region(1 << pbits, PBLOCK, wc, ctx.pregion), cw: Writers::with_wc(1 << pbits, 128, wc) })
                    .collect();
                let t0 = std::time::Instant::now();
                let res = par(states, |_, mut st| {
                    loop {
                        let j = next.fetch_add(1, Relaxed);
                        if j >= jobs.len() {
                            break;
                        }
                        unsafe { main_d(level, &ctx, &mut st, &jobs[j]) };
                    }
                    (st.escape, st.slow, st.dw.finish(), st.pw.finish())
                });
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                let (mut h, mut nd, mut ng, mut slow, mut esc) = (0u64, 0u64, 0u64, 0u64, false);
                let pbase = ctx.pregion.base as *const PRec;
                for (e, s, d, _) in &res {
                    esc |= *e;
                    slow += *s;
                    for part in 0..(1usize << dbits) {
                        for sl in d.slices(part) {
                            for r in sl.iter() {
                                nd += 1;
                                let mut x = mum(r.eid ^ K0, K1) ^ r.ing as u64;
                                if r.prec != NOPREC {
                                    ng += 1;
                                    let p = unsafe { &*pbase.add(r.prec as usize) };
                                    x = mum(x ^ p.k, K1) ^ (((p.ts as u64) << 32) | p.pc as u64);
                                }
                                h = h.wrapping_add(mum(x ^ K1, K0));
                            }
                        }
                    }
                }
                println!("kernel={kernel} threads={nt} ms={ms:.1} drecs={nd} good={ng} slow={slow} escape={esc} hash={h:016x}");
                match first {
                    None => first = Some((h, nd, ng)),
                    Some(f) => same &= f == (h, nd, ng),
                }
            }
        }
    }
    println!("{}", if same { "RECORDS IDENTICAL" } else { "RECORDS DIFFER" });
}

#[cfg(test)]
mod hop_tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn one_in(&mut self, n: u64) -> bool {
            self.below(n) == 0
        }
        fn pick<'a>(&mut self, v: &[&'a [u8]]) -> &'a [u8] {
            v[self.below(v.len() as u64) as usize]
        }
    }

    fn id(r: &mut Rng, lead: u8, out: &mut Vec<u8>) {
        match r.below(60) {
            0 => {}
            1 => out.extend_from_slice(b"E12"),
            2 => out.extend_from_slice(b"e123456789012"),
            3 => out.extend_from_slice(b"u1234567890"),
            4 => out.extend_from_slice(b"u12345-78"),
            5 => out.extend_from_slice(b"u1\xC3\xA9x"),
            6 => out.extend_from_slice(b" u1"),
            7 => out.extend_from_slice(b"u1\x00a"),
            8 => out.extend_from_slice(b"u~{|}zz"),
            _ => {
                out.push(lead);
                let n = if r.one_in(8) { r.below(16) } else { 7 + r.below(2) };
                for _ in 0..n {
                    out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[r.below(36) as usize]);
                }
            }
        }
    }

    fn two(v: u64, out: &mut Vec<u8>) {
        out.push(b'0' + (v / 10 % 10) as u8);
        out.push(b'0' + (v % 10) as u8);
    }

    fn civil(r: &mut Rng, out: &mut Vec<u8>) {
        let year: &[u8] = if r.one_in(40) { r.pick(&[b"1999", b"2100", b"2028", b"202x", b"2027"]) } else { b"2026" };
        out.extend_from_slice(year);
        out.push(b'-');
        two(if r.one_in(30) { r.below(15) } else { 9 + r.below(3) }, out);
        out.push(b'-');
        two(if r.one_in(30) { r.below(34) } else { 1 + r.below(30) }, out);
        out.push(if r.one_in(60) { *r.pick(&[b" ", b"t", b"-"]).first().unwrap() } else { b'T' });
        two(if r.one_in(30) { r.below(31) } else { r.below(24) }, out);
        out.push(b':');
        two(if r.one_in(40) { r.below(70) } else { r.below(60) }, out);
        out.push(if r.one_in(80) { b'.' } else { b':' });
        two(if r.one_in(40) { r.below(70) } else { r.below(60) }, out);
    }

    fn ts(r: &mut Rng, ingest: bool, out: &mut Vec<u8>) {
        let k = if ingest { if r.one_in(50) { r.below(12) } else { 0 } } else { r.below(12) };
        match k {
            0..=4 => {
                civil(r, out);
                out.push(if r.one_in(50) { *r.pick(&[b"z", b"", b"+", b","]).first().unwrap_or(&b'Z') } else { b'Z' });
            }
            5 | 6 => {
                civil(r, out);
                out.push(if r.one_in(30) { *r.pick(&[b"Z", b" ", b"*"]).first().unwrap() } else if r.one_in(2) { b'+' } else { b'-' });
                two(if r.one_in(20) { r.below(30) } else { r.below(14) }, out);
                if !r.one_in(40) {
                    out.push(b':');
                }
                two(if r.one_in(20) { r.below(70) } else { r.below(60) }, out);
            }
            7..=9 => {
                // around 2026-10-01 .. 2026-10-20 in ms
                let v = 1_790_812_800_000u64 + r.below(20 * 86_400_000);
                let s = format!("{v}");
                match r.below(40) {
                    0 => out.extend_from_slice(&s.as_bytes()[..r.below(13) as usize]),
                    1 => {
                        out.extend_from_slice(b"000");
                        out.extend_from_slice(s.as_bytes());
                    }
                    2 => {
                        out.extend_from_slice(s.as_bytes());
                        out.extend_from_slice(b".0");
                    }
                    3 => {
                        out.extend_from_slice(&s.as_bytes()[..12]);
                        out.push(b'x');
                    }
                    4 => out.extend_from_slice(b"99999999999999999999999"),
                    _ => out.extend_from_slice(s.as_bytes()),
                }
            }
            10 => out.extend_from_slice(r.pick(&[b"", b"-5", b"1.79e12", b"2026-10-03", b"2026-10-03T23:58:10", b"86400000", b"2026-10-03T23:58:10.5Z", b"2026-10-03T23:58:10+0200"])),
            _ => {
                // late and boundary pairs are made by the caller's window; here: exact window edges
                out.extend_from_slice(r.pick(&[b"2026-10-01T00:00:00Z", b"2026-10-15T00:00:00Z", b"2026-10-14T23:59:59Z", b"2026-09-30T23:59:59Z", b"2026-10-01T02:00:00+02:00", b"2026-09-30T23:00:00-01:00", b"2026-02-29T10:00:00Z", b"2028-02-29T10:00:00Z"]));
            }
        }
    }

    fn row(r: &mut Rng, n: usize, out: &mut Vec<u8>) {
        let mut f: Vec<Vec<u8>> = vec![Vec::new(); 7];
        if r.one_in(25) {
            id(r, b'e', &mut f[0]);
        } else {
            // mostly unique, now and then a duplicate of an earlier line
            let k = if r.one_in(10) { r.below(n as u64 + 1) } else { n as u64 };
            f[0] = format!("e{:07}", k).into_bytes();
        }
        id(r, b'u', &mut f[1]);
        ts(r, false, &mut f[2]);
        ts(r, true, &mut f[3]);
        f[4] = if r.one_in(25) { r.pick(&[b"Purchase", b"cart ", b"viewx", b"", b"vie", b"purchas", b"VIEW", b"xiew", b"zart", b"vart", b"purchasd"]).to_vec() } else { r.pick(&[b"view", b"view", b"view", b"cart", b"purchase", b"purchase"]).to_vec() };
        f[5] = if r.one_in(25) { r.pick(&[b"", b"P1", b"p123456789012345678901234567890", b"p\xFF1", b"p 1", b"x"]).to_vec() } else { format!("p{}", r.below(40)).into_bytes() };
        f[6] = if r.one_in(6) { r.pick(&[b"", b"19.99", b"-5", b"0", b"00", b"0250", b"10000000", b"10000001", b"1000\r", b"99999999999999999999", b"12a", b"123456789012345678901234567890123", b"1234567\x01", b"\x021", b"1 ", b"12345678", b"00000001", b"000000001", b"9999999", b"1234567\r", b"\t5"]).to_vec() } else { format!("{}", 1 + r.below(60_000)).into_bytes() };
        let mut nf = 7;
        if r.one_in(60) {
            nf = r.below(10) as usize;
        }
        let start = out.len();
        for k in 0..nf {
            if k > 0 {
                out.push(b',');
            }
            let v: &[u8] = if k < 7 { &f[k] } else { b"x" };
            let q = r.one_in(5);
            if q {
                out.push(b'"');
            }
            out.extend_from_slice(v);
            if q && !r.one_in(150) {
                out.push(b'"');
            }
        }
        // byte-level dirt
        if r.one_in(30) && out.len() > start {
            let pos = start + r.below((out.len() - start) as u64) as usize;
            let c = *r.pick(&[b"\"", b",", b"\n", b"\r", b"\x00", b"\xE2", b" ", b"Z", b"0", b":"]).first().unwrap();
            match r.below(3) {
                0 => out[pos] = c,
                1 => out.insert(pos, c),
                _ => {
                    out.remove(pos);
                }
            }
        }
        if r.one_in(100) {
            out.push(b'\r');
        }
        out.push(b'\n');
        if r.one_in(200) {
            out.push(b'\n');
        }
    }

    type Rec = (Id, u32, Option<(Id, u32, u32)>);

    fn run_kernel(ctx: &mut Ctx, data: &'static [u8], hop: bool, level: Level, chunk: usize) -> (bool, Vec<Rec>) {
        ctx.hop = hop;
        ctx.pregion = Region::new(64 << 20);
        let mut st = PState {
            escape: false,
            slow: 0,
            rbuf: std::ptr::null_mut(),
            rcap: 0,
            buf: ScanBuf::new(),
            cache: DateCache::new(),
            dates: crate::hop::HopDates::new(),
            dw: Writers::with_wc(1 << ctx.dbits, 8, true),
            pw: Writers::in_region(1 << ctx.pbits, 8, true, ctx.pregion),
            cw: Writers::with_wc(1 << ctx.pbits, 8, true),
        };
        let mut jobs = Vec::new();
        make_jobs(&[data], 0, chunk, false, &mut jobs);
        for j in &jobs {
            unsafe { main_d(level, ctx, &mut st, j) };
        }
        let (d, _p) = (st.dw.finish(), st.pw.finish());
        let pbase = ctx.pregion.base as *const PRec;
        let mut out = Vec::new();
        for part in 0..(1usize << ctx.dbits) {
            for sl in d.slices(part) {
                for r in sl.iter() {
                    let p = if r.prec == NOPREC {
                        None
                    } else {
                        let p = unsafe { &*pbase.add(r.prec as usize) };
                        Some((p.k, p.ts, p.pc))
                    };
                    out.push((r.eid, r.ing, p));
                }
            }
        }
        out.sort();
        (st.escape, out)
    }

    fn test_ctx() -> Ctx {
        let ws = parse_ts_slice(b"2026-10-01T00:00:00Z");
        let we = parse_ts_slice(b"2026-10-15T00:00:00Z");
        let mut products = IdMap::new(64);
        for i in 0..30u32 {
            let name = format!("p{i}");
            products.insert(unsafe { pack_id(name.as_ptr(), name.len(), name.as_ptr().add(name.len())) }, i % 5);
        }
        Ctx {
            ws,
            we,
            cbase: ws - WEEK,
            links: std::sync::OnceLock::new(),
            products,
            unknown_cat: 7,
            cats: Vec::new(),
            campaigns: IdMap::new(1),
            camp_names: Vec::new(),
            camp_channels: Vec::new(),
            dbits: 2,
            pbits: 1,
            stage: 0,
            drop_pages: false,
            ibase: ws - (1i64 << 31) * 1000,
            pregion: Region::new(1 << 20),
            hop: false,
        }
    }

    /// The row-hopping kernel against the scalar reference scanner on adversarial lines:
    /// the same records, whatever the chunking.
    #[test]
    fn hop_equals_reference() {
        let mut ctx = test_ctx();
        let level = pick_level();
        let rounds: u64 = std::env::var("HOP_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..rounds {
            let mut v = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n".to_vec();
            let rows = 1 + r.below(if round % 4 == 0 { 30 } else { 3000 }) as usize;
            for n in 0..rows {
                row(&mut r, n, &mut v);
            }
            if round % 3 == 0 {
                v.pop();
            }
            let data: &'static [u8] = Box::leak(v.into_boxed_slice());
            let want = run_kernel(&mut ctx, data, false, Level::Scalar, 1 << 30);
            // (`want.0`, the escape flag, is set by lines whose ingest_ts does not fit a `DRec`;
            // both kernels must agree on it too)
            for chunk in [1usize << 30, 7, 193, 4096] {
                let got = run_kernel(&mut ctx, data, true, level, chunk);
                if got != want {
                    let (a, b) = (&got.1, &want.1);
                    let i = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]).unwrap_or(a.len().min(b.len()));
                    let mut name = Vec::new();
                    if let Some(x) = a.get(i).or(b.get(i)) {
                        id_bytes(x.0, &mut name);
                    }
                    panic!("round {round} chunk {chunk}: {} vs {} records; first difference at {i}: {:?} vs {:?} (event {:?})", a.len(), b.len(), a.get(i), b.get(i), String::from_utf8_lossy(&name));
                }
            }
            let got = run_kernel(&mut ctx, data, false, level, 4096);
            assert!(got == want, "scan kernel differs in round {round}");
        }
    }

    fn click_line(r: &mut Rng, n: usize, out: &mut Vec<u8>) {
        let mut f: Vec<Vec<u8>> = vec![Vec::new(); 4];
        if r.one_in(25) {
            id(r, b'k', &mut f[0]);
        } else {
            f[0] = format!("k{:07}", n).into_bytes();
        }
        id(r, b'u', &mut f[1]);
        f[2] = if r.one_in(12) { r.pick(&[b"", b"C_1", b"c_99", b" c_1", b"c_1 ", b"c_12345678901", b"c\xC3\xA9", b"c_1\r"]).to_vec() } else { format!("c_{}", r.below(12)).into_bytes() };
        ts(r, false, &mut f[3]);
        let nf = if r.one_in(60) { r.below(7) as usize } else { 4 };
        let start = out.len();
        for k in 0..nf {
            if k > 0 {
                out.push(b',');
            }
            let v: &[u8] = if k < 4 { &f[k] } else { b"x" };
            let q = r.one_in(5);
            if q {
                out.push(b'"');
            }
            out.extend_from_slice(v);
            if q && !r.one_in(150) {
                out.push(b'"');
            }
        }
        if r.one_in(30) && out.len() > start {
            let pos = start + r.below((out.len() - start) as u64) as usize;
            let c = *r.pick(&[b"\"", b",", b"\n", b"\r", b"\x00", b"\xE2", b" ", b"Z", b"0", b":"]).first().unwrap();
            match r.below(3) {
                0 => out[pos] = c,
                1 => out.insert(pos, c),
                _ => {
                    out.remove(pos);
                }
            }
        }
        if r.one_in(100) {
            out.push(b'\r');
        }
        out.push(b'\n');
        if r.one_in(200) {
            out.push(b'\n');
        }
    }

    fn run_clicks(ctx: &mut Ctx, data: &'static [u8], hop: bool, level: Level, chunk: usize) -> Vec<(Id, u32, u32, usize)> {
        ctx.hop = hop;
        let mut st = PState {
            escape: false,
            slow: 0,
            rbuf: std::ptr::null_mut(),
            rcap: 0,
            buf: ScanBuf::new(),
            cache: DateCache::new(),
            dates: crate::hop::HopDates::new(),
            dw: Writers::with_wc(1 << ctx.dbits, 8, true),
            pw: Writers::in_region(1 << ctx.pbits, 8, true, ctx.pregion),
            cw: Writers::with_wc(1 << ctx.pbits, 8, true),
        };
        let mut jobs = Vec::new();
        make_jobs(&[data], 1, chunk, false, &mut jobs);
        for j in &jobs {
            unsafe { main_d(level, ctx, &mut st, j) };
        }
        let c = st.cw.finish();
        let mut out = Vec::new();
        for part in 0..(1usize << ctx.pbits) {
            for sl in c.slices(part) {
                for r in sl.iter() {
                    out.push((r.k, r.ts, r.camp, r.ptr - data.as_ptr() as usize));
                }
            }
        }
        out.sort();
        out
    }

    /// The click row-hopping kernel against the scalar reference scanner on adversarial
    /// lines: the same click records (user, time, campaign, line), whatever the chunking.
    #[test]
    fn hop_clicks_equal_reference() {
        let mut ctx = test_ctx();
        ctx.campaigns = IdMap::new(16);
        for i in 0..8u32 {
            let name = format!("c_{i}");
            ctx.campaigns.insert(unsafe { pack_id(name.as_ptr(), name.len(), name.as_ptr().add(name.len())) }, i);
        }
        let level = pick_level();
        let rounds: u64 = std::env::var("HOP_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
        let mut r = Rng(0xD1B5_4A32_D192_ED03);
        let mut kept = 0usize;
        for round in 0..rounds {
            let mut v = b"click_id,user_id,campaign_id,ts\n".to_vec();
            let rows = 1 + r.below(if round % 4 == 0 { 30 } else { 3000 }) as usize;
            for n in 0..rows {
                click_line(&mut r, n, &mut v);
            }
            if round % 3 == 0 {
                v.pop();
            }
            let data: &'static [u8] = Box::leak(v.into_boxed_slice());
            let want = run_clicks(&mut ctx, data, false, Level::Scalar, 1 << 30);
            kept += want.len();
            for chunk in [1usize << 30, 7, 193, 4096] {
                let got = run_clicks(&mut ctx, data, true, level, chunk);
                assert!(got == want, "round {round} chunk {chunk}: {} vs {} click records", got.len(), want.len());
            }
        }
        assert!(kept > 1000, "the generator must produce clicks that are kept ({kept})");
    }
}
