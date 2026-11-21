//! OS glue (mmap, arenas), IDs, hashing, thread helper, timing.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    pub fn _exit(code: i32) -> !;
    fn getrusage(who: i32, ru: *mut [i64; 18]) -> i32;
    fn pread(fd: i32, buf: *mut c_void, n: usize, off: i64) -> isize;
    #[cfg(target_os = "linux")]
    fn prctl(option: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
}

/// Experiment knob (Linux, SE_NOTHP=1): run without transparent huge pages, to measure what
/// a host with THP disabled would cost.
pub fn thp_knob() {
    #[cfg(target_os = "linux")]
    if env_flag("SE_NOTHP") {
        unsafe { prctl(41, 1, 0, 0, 0) }; // PR_SET_THP_DISABLE
    }
}

/// File descriptors of the mapped files, by mapping address (kept open until exit).
static FDS: Mutex<Vec<(usize, i32)>> = Mutex::new(Vec::new());

/// The descriptor behind a `map_file` mapping, or -1.
pub fn fd_of(data: &[u8]) -> i32 {
    let p = data.as_ptr() as usize;
    FDS.lock().unwrap().iter().find(|x| x.0 == p).map_or(-1, |x| x.1)
}

/// Reads exactly `n` bytes at `off`; false on any error or short read.
pub fn pread_exact(fd: i32, buf: *mut u8, n: usize, off: usize) -> bool {
    let mut done = 0;
    while done < n {
        let r = unsafe { pread(fd, buf.add(done) as *mut c_void, n - done, (off + done) as i64) };
        if r <= 0 {
            return false;
        }
        done += r as usize;
    }
    true
}

/// (user, system) CPU seconds used by the whole process so far.
pub fn cpu_secs() -> (f64, f64) {
    let mut r = [0i64; 18];
    unsafe { getrusage(0, &mut r) };
    let us = |x: i64| (x & 0xFFFF_FFFF) as f64 * 1e-6;
    (r[0] as f64 + us(r[1]), r[2] as f64 + us(r[3]))
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2;
#[cfg(target_os = "linux")]
const MAP_ANON: i32 = 0x20;
#[cfg(not(target_os = "linux"))]
const MAP_ANON: i32 = 0x1000;
#[cfg(target_os = "linux")]
const MAP_NORESERVE: i32 = 0x4000;
#[cfg(not(target_os = "linux"))]
const MAP_NORESERVE: i32 = 0x40;
#[cfg(target_os = "linux")]
const MAP_POPULATE: i32 = 0x8000;
const MADV_WILLNEED: i32 = 3;
#[cfg(target_os = "linux")]
const MADV_HUGEPAGE: i32 = 14;

/// A knob that is on unless set to `0`.
pub fn env_on(name: &str) -> bool {
    std::env::var(name).map(|v| v != "0").unwrap_or(true)
}

pub fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| !v.is_empty() && v != "0").unwrap_or(false)
}

/// Maps a whole file read-only. The mapping lives until exit.
pub fn map_file(path: &std::path::Path) -> std::io::Result<&'static [u8]> {
    let f = std::fs::File::open(path)?;
    let len = f.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    #[allow(unused_mut)]
    let mut flags = MAP_PRIVATE;
    #[cfg(target_os = "linux")]
    if env_flag("SE_POPULATE") {
        flags |= MAP_POPULATE;
    }
    let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, flags, f.as_raw_fd(), 0) };
    if p as isize == -1 {
        // Fallback: read into memory.
        let v = std::fs::read(path)?;
        return Ok(Box::leak(v.into_boxed_slice()));
    }
    unsafe {
        if len >= (1 << 20) && env_flag("SE_WILLNEED") {
            madvise(p, len, MADV_WILLNEED);
        }
        use std::os::fd::IntoRawFd;
        FDS.lock().unwrap().push((p as usize, f.into_raw_fd()));
        Ok(std::slice::from_raw_parts(p as *const u8, len))
    }
}

/// Experiment knob (Linux, SE_POPREAD=1): map a chunk's file pages in one call before parsing it.
#[inline]
pub fn populate_read(_p: *const u8, _len: usize) {
    #[cfg(target_os = "linux")]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ON.get_or_init(|| env_flag("SE_POPREAD")) {
            let a = (_p as usize) & !4095;
            let e = (_p as usize + _len + 4095) & !4095;
            unsafe { madvise(a as *mut c_void, e - a, 22) }; // MADV_POPULATE_READ
        }
    }
}

/// Big anonymous slabs handed out by `os_alloc`, for `release_all`.
#[cfg(target_os = "linux")]
static SLABS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Drops the pages of a parsed chunk of a file mapping (Linux; the mapping stays valid and
/// re-reads from the page cache if touched again). Done by the thread that parsed it, so the
/// unmapping work is spread over all threads instead of being done serially at exit.
#[inline]
pub fn drop_range(_p: *const u8, _len: usize) {
    #[cfg(target_os = "linux")]
    {
        let a = (_p as usize + 4095) & !4095;
        let e = (_p as usize + _len) & !4095;
        if e > a {
            unsafe { madvise(a as *mut c_void, e - a, 4) }; // MADV_DONTNEED
        }
    }
}

/// Gives the big anonymous slabs back to the kernel from `nthreads` threads at once (Linux).
/// Process exit would do the same work on one thread. Nothing may use arena memory afterwards.
pub fn release_all(_nthreads: usize) {
    #[cfg(target_os = "linux")]
    {
        let slabs = std::mem::take(&mut *SLABS.lock().unwrap());
        let next = std::sync::atomic::AtomicUsize::new(0);
        par(vec![(); _nthreads.max(1)], |_, _| {
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= slabs.len() {
                    break;
                }
                unsafe { madvise(slabs[i].0 as *mut c_void, slabs[i].1, 4) }; // MADV_DONTNEED
            }
        });
    }
}

/// Zeroed, page-aligned anonymous memory that is never freed.
pub fn os_alloc(bytes: usize) -> *mut u8 {
    let bytes = bytes.max(4096);
    #[allow(unused_mut)]
    let mut flags = MAP_PRIVATE | MAP_ANON | MAP_NORESERVE;
    #[cfg(target_os = "linux")]
    {
        // Experiment knob: pre-fault whole slabs in one call instead of page by page.
        static POP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *POP.get_or_init(|| env_flag("SE_POPULATE_ANON")) {
            flags = MAP_PRIVATE | MAP_ANON | MAP_POPULATE;
        }
    }
    let p = unsafe { mmap(std::ptr::null_mut(), bytes, PROT_READ | PROT_WRITE, flags, -1, 0) };
    if p as isize == -1 {
        eprintln!("shopping-engine: out of memory ({bytes} bytes)");
        std::process::exit(1);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        if bytes >= (2 << 20) {
            madvise(p, bytes, MADV_HUGEPAGE);
        }
        if bytes >= (1 << 20) {
            // in pieces, so that `release_all` can spread one big range over its threads
            let mut s = SLABS.lock().unwrap();
            let mut o = 0;
            while o < bytes {
                let n = (bytes - o).min(4 * SLAB);
                s.push((p as usize + o, n));
                o += n;
            }
        }
    }
    p as *mut u8
}

/// One big reserved address range that several arenas carve their slabs from, so that a
/// record in it can be named by a 32-bit index instead of a pointer.
pub struct Region {
    pub base: usize,
    next: std::sync::atomic::AtomicUsize,
    end: usize,
}
impl Region {
    /// Reserves `bytes` (untouched pages cost nothing).
    pub fn new(bytes: usize) -> &'static Region {
        let bytes = (bytes + SLAB - 1) / SLAB * SLAB;
        let base = os_alloc(bytes) as usize;
        Box::leak(Box::new(Region { base, next: std::sync::atomic::AtomicUsize::new(base), end: base + bytes }))
    }
    fn slab(&self) -> Option<usize> {
        let p = self.next.fetch_add(SLAB, std::sync::atomic::Ordering::Relaxed);
        if p + SLAB <= self.end { Some(p) } else { None }
    }
}

/// Per-thread bump allocator over big OS slabs. Memory is never returned.
pub struct Arena {
    cur: usize,
    end: usize,
    region: Option<&'static Region>,
}
const SLAB: usize = 8 << 20;
impl Arena {
    pub fn new() -> Self {
        Arena { cur: 0, end: 0, region: None }
    }
    #[inline]
    pub fn alloc(&mut self, bytes: usize) -> *mut u8 {
        let bytes = (bytes + 63) & !63;
        if self.cur + bytes > self.end {
            let slab = bytes.max(SLAB);
            // A full region falls back to a private slab (outside the region's index range).
            self.cur = match self.region.and_then(|r| if slab == SLAB { r.slab() } else { None }) {
                Some(p) => p,
                None => os_alloc(slab) as usize,
            };
            self.end = self.cur + slab;
        }
        let p = self.cur;
        self.cur += bytes;
        p as *mut u8
    }
}

/// Copies `bytes` (a multiple of 64) from a write-combining buffer to its 64-byte-aligned
/// place in a block. On x86-64 with non-temporal stores: the block's cache lines are not
/// read first and do not displace the working set (`sfence_all` must follow before the
/// records are read by another thread).
#[inline(always)]
unsafe fn wc_copy(dst: *mut u8, src: *const u8, bytes: usize) {
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::*;
            let mut o = 0;
            while o < bytes {
                _mm_stream_si128(dst.add(o) as *mut __m128i, _mm_loadu_si128(src.add(o) as *const __m128i));
                o += 16;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        std::ptr::copy_nonoverlapping(src, dst, bytes);
    }
}

#[inline(always)]
fn sfence_all() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_sfence();
    }
}

/// Whether `Writers` use write-combining buffers. Default: on for x86-64, where the copy to
/// the block is a non-temporal store (measured on a Cascade Lake n2-standard-8 at 20M events:
/// parse 680 -> 609 ms wall), off elsewhere (arm64: about 6% more instructions, no gain).
/// SE_WC=0 / SE_WC=1 override.
pub fn wc_default() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("SE_WC").as_deref() {
        Ok("0") => false,
        Ok(v) if !v.is_empty() => true,
        _ => cfg!(target_arch = "x86_64"),
    })
}

/// Fan-out writer: one append-only stream of `T` per partition, stored as blocks.
///
/// With write combining, a record first goes into the partition's small buffer (`unit`
/// records, a multiple of 64 bytes; all buffers of a writer are contiguous), and a full
/// buffer is copied to the block in one go. The hot state then is a few dense pages
/// instead of one cache line and one page per partition scattered over the arena.
pub struct Writers<T> {
    /// (next record's place in the open block, or with write combining the place of the
    /// buffer's first record; end of the open block)
    heads: Vec<(*mut T, *mut T)>,
    blocks: Vec<Vec<(usize, usize)>>, // (start ptr, capacity) per block; the last one is open
    block_len: usize,
    wc: bool,
    unit: usize,
    buf: *mut T,
    fill: Vec<u8>,
    pub arena: Arena,
}
unsafe impl<T> Send for Writers<T> {}

impl<T: Copy> Writers<T> {
    pub fn new(parts: usize, block_len: usize) -> Self {
        Self::with_wc(parts, block_len, false)
    }
    pub fn with_wc(parts: usize, block_len: usize, wc: bool) -> Self {
        let size = std::mem::size_of::<T>();
        // records per 64-byte multiple
        let mut unit = 1;
        while unit * size % 64 != 0 {
            unit += 1;
        }
        let wc = wc && unit <= 16 && std::mem::align_of::<T>() <= 16 && size % 8 == 0;
        let block_len = block_len.max(4).div_ceil(unit) * unit;
        Writers {
            heads: vec![(std::ptr::null_mut(), std::ptr::null_mut()); parts],
            blocks: vec![Vec::new(); parts],
            block_len,
            wc,
            unit,
            buf: if wc { os_alloc(parts * unit * size) as *mut T } else { std::ptr::null_mut() },
            fill: vec![0; if wc { parts } else { 0 }],
            arena: Arena::new(),
        }
    }
    /// Like `with_wc`, with all blocks taken from `region`.
    pub fn in_region(parts: usize, block_len: usize, wc: bool, region: &'static Region) -> Self {
        let mut w = Self::with_wc(parts, block_len, wc);
        w.arena.region = Some(region);
        w
    }
    #[cold]
    #[inline(never)]
    fn refill(&mut self, s: usize) {
        let p = self.arena.alloc(self.block_len * std::mem::size_of::<T>()) as *mut T;
        self.blocks[s].push((p as usize, self.block_len));
        self.heads[s] = (p, unsafe { p.add(self.block_len) });
    }
    #[inline(always)]
    pub fn push(&mut self, s: usize, v: T) {
        self.push_ptr(s, v);
    }
    /// Appends `v` to partition `s`, returning where the value lives (with write combining:
    /// will live once its buffer is flushed, at the latest by `finish`).
    #[inline(always)]
    pub fn push_ptr(&mut self, s: usize, v: T) -> *mut T {
        unsafe {
            if self.heads.get_unchecked(s).0 == self.heads.get_unchecked(s).1 {
                self.refill(s);
            }
            let h = self.heads.get_unchecked_mut(s);
            if self.wc {
                let n = *self.fill.get_unchecked(s) as usize;
                let b = self.buf.add(s * self.unit);
                b.add(n).write(v);
                let p = h.0.add(n);
                if n + 1 == self.unit {
                    wc_copy(h.0 as *mut u8, b as *const u8, self.unit * std::mem::size_of::<T>());
                    h.0 = h.0.add(self.unit);
                    *self.fill.get_unchecked_mut(s) = 0;
                } else {
                    *self.fill.get_unchecked_mut(s) = (n + 1) as u8;
                }
                p
            } else {
                let p = h.0;
                p.write(v);
                h.0 = p.add(1);
                p
            }
        }
    }
    /// Per partition: the filled slices.
    pub fn finish(mut self) -> Parts<T> {
        if self.wc {
            // the partly filled buffers
            for s in 0..self.heads.len() {
                let n = self.fill[s] as usize;
                if n > 0 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(self.buf.add(s * self.unit), self.heads[s].0, n);
                        self.heads[s].0 = self.heads[s].0.add(n);
                    }
                }
            }
            sfence_all();
        }
        let mut out = Vec::with_capacity(self.blocks.len());
        for (s, bl) in self.blocks.into_iter().enumerate() {
            let mut v: Vec<(usize, usize)> = Vec::with_capacity(bl.len());
            let n = bl.len();
            for (i, (p, cap)) in bl.into_iter().enumerate() {
                let len = if i + 1 == n { cap - (self.heads[s].1 as usize - self.heads[s].0 as usize) / std::mem::size_of::<T>() } else { cap };
                if len > 0 {
                    v.push((p, len));
                }
            }
            out.push(v);
        }
        Parts { parts: out, _m: std::marker::PhantomData }
    }
}

pub struct Parts<T> {
    parts: Vec<Vec<(usize, usize)>>,
    _m: std::marker::PhantomData<T>,
}
unsafe impl<T> Send for Parts<T> {}
unsafe impl<T> Sync for Parts<T> {}
impl<T: 'static> Parts<T> {
    #[inline]
    pub fn slices(&self, part: usize) -> impl Iterator<Item = &'static mut [T]> + '_ {
        self.parts[part].iter().map(|&(p, n)| unsafe { std::slice::from_raw_parts_mut(p as *mut T, n) })
    }
    pub fn count(&self, part: usize) -> usize {
        self.parts[part].iter().map(|x| x.1).sum()
    }
}

/// Runs `f(tid, state)` on one thread per state (the first on the caller) and returns the results in order.
pub fn par<S: Send, R: Send>(states: Vec<S>, f: impl Fn(usize, S) -> R + Sync) -> Vec<R> {
    let n = states.len();
    if n == 1 {
        return states.into_iter().map(|s| f(0, s)).collect();
    }
    let f = &f;
    std::thread::scope(|sc| {
        let mut it = states.into_iter().enumerate();
        let (_, first) = it.next().unwrap();
        let handles: Vec<_> = it.map(|(i, s)| sc.spawn(move || f(i, s))).collect();
        let mut out = Vec::with_capacity(n);
        out.push(f(0, first));
        for h in handles {
            out.push(h.join().unwrap());
        }
        out
    })
}

pub struct Timer {
    pub on: bool,
    t0: Instant,
    last: Instant,
    cpu: (f64, f64),
}
impl Timer {
    pub fn new(on: bool) -> Self {
        let n = Instant::now();
        Timer { on, t0: n, last: n, cpu: if on { cpu_secs() } else { (0.0, 0.0) } }
    }
    pub fn lap(&mut self, name: &str) {
        if self.on {
            let n = Instant::now();
            let c = cpu_secs();
            eprintln!(
                "[timing] {:<28} wall {:>9.3} ms   user {:>9.3} ms   sys {:>8.3} ms   (t={:.3} ms)",
                name,
                (n - self.last).as_secs_f64() * 1e3,
                (c.0 - self.cpu.0) * 1e3,
                (c.1 - self.cpu.1) * 1e3,
                (n - self.t0).as_secs_f64() * 1e3
            );
            self.cpu = c;
            self.last = n;
        }
    }
}

// ---------------------------------------------------------------- IDs

/// An ID string as one word. IDs of at most 9 bytes, all in 1..=127 (every generated ID), are
/// packed 7 bits a byte, first byte most significant, zero padded, top bit clear: comparing
/// two packed IDs as integers is the byte-wise order of the strings. Every other ID (longer,
/// non-ASCII, or holding a NUL) is interned exactly: top bit set, low bits = index. Empty = 0.
pub type Id = u64;
pub const LONG: u64 = 1 << 63;

struct Interner {
    map: Mutex<HashMap<&'static [u8], u64>>,
    list: RwLock<Vec<&'static [u8]>>,
}
static INTERN: std::sync::OnceLock<Interner> = std::sync::OnceLock::new();
fn interner() -> &'static Interner {
    INTERN.get_or_init(|| Interner { map: Mutex::new(HashMap::new()), list: RwLock::new(Vec::new()) })
}

#[cold]
#[inline(never)]
pub fn intern_long(s: &[u8]) -> Id {
    let it = interner();
    let mut m = it.map.lock().unwrap();
    if let Some(&i) = m.get(s) {
        return i | LONG;
    }
    // A private copy: the caller's bytes may sit in a reused read buffer.
    let s: &'static [u8] = Box::leak(s.to_vec().into_boxed_slice());
    let mut l = it.list.write().unwrap();
    let i = l.len() as u64;
    l.push(s);
    m.insert(s, i);
    i | LONG
}

#[inline(always)]
pub fn is_long(id: Id) -> bool {
    id & LONG != 0
}

/// Appends the ID's bytes to `out`.
#[inline]
pub fn id_bytes(id: Id, out: &mut Vec<u8>) {
    if is_long(id) {
        out.extend_from_slice(interner().list.read().unwrap()[(id & !LONG) as usize]);
    } else {
        let mut sh = 56i32;
        while sh >= 0 {
            let c = ((id >> sh) & 0x7F) as u8;
            if c == 0 {
                break;
            }
            out.push(c);
            sh -= 7;
        }
    }
}

/// Byte-wise `a < b`.
#[inline(always)]
pub fn id_less(a: Id, b: Id) -> bool {
    if (a | b) & LONG == 0 { a < b } else { id_less_slow(a, b) }
}
#[cold]
fn id_less_slow(a: Id, b: Id) -> bool {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    id_bytes(a, &mut x);
    id_bytes(b, &mut y);
    x < y
}

pub const K0: u64 = 0x9E37_79B9_7F4A_7C15;
pub const K1: u64 = 0xD6E8_FEB8_6659_FD93;

#[inline(always)]
pub fn mum(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

#[inline(always)]
pub fn id_hash(id: Id) -> u64 {
    mum(id ^ K0, K1)
}

/// First `len` (<= 16) bytes at `p` as little-endian (lo, hi), zero padded.
/// `lim` is the end of the readable buffer.
#[inline(always)]
pub unsafe fn load16(p: *const u8, len: usize, lim: *const u8) -> (u64, u64) {
    unsafe {
        let v: u128 = if (p as usize) + 16 <= lim as usize {
            (p as *const u128).read_unaligned()
        } else {
            let mut b = [0u8; 16];
            std::ptr::copy_nonoverlapping(p, b.as_mut_ptr(), len);
            u128::from_le_bytes(b)
        };
        let v = if len >= 16 { v } else { v & ((1u128 << (8 * len as u32)) - 1) };
        (v as u64, (v >> 64) as u64)
    }
}

/// Packs the field into an `Id`.
#[inline(always)]
pub unsafe fn pack_id(p: *const u8, len: usize, lim: *const u8) -> Id {
    unsafe {
        if len <= 9 {
            let (lo, hi) = load16(p, len, lim);
            // zero bytes of `lo` (exact when no byte has its top bit set)
            let z = lo.wrapping_sub(0x0101_0101_0101_0101) & !lo & 0x8080_8080_8080_8080;
            let want = if len >= 8 { 0 } else { 0x8080_8080_8080_8080u64 << (8 * len as u32) };
            if ((lo | hi) & 0x8080_8080_8080_8080) == 0 && z == want && (len < 9 || hi != 0) {
                let x = lo.swap_bytes();
                // drop the (zero) top bit of every byte: 8 bytes -> 56 bits
                #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
                let x = std::arch::x86_64::_pext_u64(x, 0x7F7F_7F7F_7F7F_7F7F);
                #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
                let x = {
                    let x = ((x & 0x7F00_7F00_7F00_7F00) >> 1) | (x & 0x007F_007F_007F_007F);
                    let x = ((x & 0x3FFF_0000_3FFF_0000) >> 2) | (x & 0x0000_3FFF_0000_3FFF);
                    ((x & 0x0FFF_FFFF_0000_0000) >> 4) | (x & 0x0000_0000_0FFF_FFFF)
                };
                return (x << 7) | hi;
            }
        }
        if len == 0 {
            return 0;
        }
        intern_long(std::slice::from_raw_parts(p, len))
    }
}

#[inline(always)]
pub fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = p;
}

/// Small open-addressing map `Id -> u32` (products, campaigns).
pub struct IdMap {
    slots: Vec<(Id, u32)>,
    mask: usize,
}
impl IdMap {
    pub fn new(n: usize) -> Self {
        let cap = (n * 2).max(8).next_power_of_two();
        IdMap { slots: vec![(0, u32::MAX); cap], mask: cap - 1 }
    }
    /// Keeps the first value for a key.
    pub fn insert(&mut self, k: Id, v: u32) {
        let mut i = id_hash(k) as usize & self.mask;
        loop {
            let s = &mut self.slots[i];
            if s.1 == u32::MAX {
                *s = (k, v);
                return;
            }
            if s.0 == k {
                return;
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    pub fn get(&self, k: Id) -> u32 {
        let mut i = id_hash(k) as usize & self.mask;
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.0 == k || s.1 == u32::MAX {
                return s.1;
            }
            i = (i + 1) & self.mask;
        }
    }
}
