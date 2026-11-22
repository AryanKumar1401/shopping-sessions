//! OS glue (mmap, arenas), IDs, hashing, thread helper, timing.

use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
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
        #[cfg(target_os = "linux")]
        MAPS.lock().unwrap().push((p as usize, (len + 4095) & !4095));
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

/// The file mappings (address, length rounded up to pages), for `release_all`.
#[cfg(target_os = "linux")]
static MAPS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

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
        let mut slabs = std::mem::take(&mut *SLABS.lock().unwrap());
        // The file mappings too (SE_UNMAP=0: leave them to exit), in pieces for the threads.
        if env_on("SE_UNMAP") {
            for &(p, len) in MAPS.lock().unwrap().iter() {
                let mut o = 0;
                while o < len {
                    let n = (len - o).min(4 * SLAB);
                    slabs.push((p + o, n));
                    o += n;
                }
            }
        }
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
pub unsafe fn wc_copy(dst: *mut u8, src: *const u8, bytes: usize) {
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
///
/// Records whose size divides 64 take the `aligned` form of it: every partition always has an
/// open block, a buffer is exactly one cache line of the block, and the only per-partition
/// state a push reads is the address its record will have (which also says how full the
/// buffer is); the end of the block is looked at only when a buffer is flushed.
pub struct Writers<T> {
    /// (next record's place in the open block, or with unaligned write combining the place
    /// of the buffer's first record; end of the open block)
    heads: Vec<(*mut T, *mut T)>,
    blocks: Vec<Vec<(usize, usize)>>, // (start ptr, capacity) per block; the last one is open
    block_len: usize,
    wc: bool,
    aligned: bool,
    unit: usize,
    buf: *mut T,
    fill: Vec<u8>,
    pub arena: Arena,
}
unsafe impl<T> Send for Writers<T> {}

impl<T: Copy> Writers<T> {
    /// Records per cache line when `T` can use the aligned form, else 0.
    const LINE: usize = {
        let size = std::mem::size_of::<T>();
        if size >= 8 && size <= 32 && size.is_power_of_two() && std::mem::align_of::<T>() <= 16 { 64 / size } else { 0 }
    };

    pub fn new(parts: usize, block_len: usize) -> Self {
        Self::with_wc(parts, block_len, false)
    }
    pub fn with_wc(parts: usize, block_len: usize, wc: bool) -> Self {
        Self::make(parts, block_len, wc, None)
    }
    /// Like `with_wc`, with all blocks taken from `region`.
    pub fn in_region(parts: usize, block_len: usize, wc: bool, region: &'static Region) -> Self {
        Self::make(parts, block_len, wc, Some(region))
    }
    fn make(parts: usize, block_len: usize, wc: bool, region: Option<&'static Region>) -> Self {
        let size = std::mem::size_of::<T>();
        // records per 64-byte multiple
        let mut unit = 1;
        while unit * size % 64 != 0 {
            unit += 1;
        }
        let wc = wc && unit <= 16 && std::mem::align_of::<T>() <= 16 && size % 8 == 0;
        let aligned = wc && Self::LINE != 0;
        let block_len = block_len.max(4).div_ceil(unit) * unit;
        let mut w = Writers {
            heads: vec![(std::ptr::null_mut(), std::ptr::null_mut()); parts],
            blocks: vec![Vec::new(); parts],
            block_len,
            wc,
            aligned,
            unit,
            buf: if wc { os_alloc(parts * unit * size) as *mut T } else { std::ptr::null_mut() },
            fill: vec![0; if wc && !aligned { parts } else { 0 }],
            arena: Arena::new(),
        };
        w.arena.region = region;
        if aligned {
            for s in 0..parts {
                w.refill(s);
            }
        }
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
            if Self::LINE != 0 && self.aligned {
                let h = self.heads.get_unchecked_mut(s);
                let p = h.0;
                let n = (p as usize / std::mem::size_of::<T>()) & (Self::LINE - 1);
                let b = self.buf.add(s * Self::LINE);
                b.add(n).write(v);
                h.0 = p.add(1);
                if n + 1 == Self::LINE {
                    wc_copy((p as *mut u8).add(std::mem::size_of::<T>()).sub(64), b as *const u8, 64);
                    if h.0 == h.1 {
                        self.refill(s);
                    }
                }
                return p;
            }
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
    /// The aligned form's parts for a push written out by the caller (hop.rs): the heads and
    /// the buffers (`LINE` records per partition). None when this writer is not in that form.
    /// The caller does what `push_ptr` does: record to `buf[s * LINE + n]`, head advanced;
    /// after the last record of a line, `wc_line`, and `refill_full` when the block is full.
    #[inline(always)]
    pub fn raw(&mut self) -> Option<(*mut (*mut T, *mut T), *mut T)> {
        if Self::LINE != 0 && self.aligned { Some((self.heads.as_mut_ptr(), self.buf)) } else { None }
    }
    /// See `raw`: gives partition `s` a new block if its open one is full.
    pub fn refill_full(&mut self, s: usize) {
        if self.heads[s].0 == self.heads[s].1 {
            self.refill(s);
        }
    }
    /// Per partition: the filled slices.
    pub fn finish(mut self) -> Parts<T> {
        if self.aligned {
            // the partly filled buffers
            for s in 0..self.heads.len() {
                let n = (self.heads[s].0 as usize / std::mem::size_of::<T>()) & (Self::LINE - 1);
                if n > 0 {
                    unsafe { std::ptr::copy_nonoverlapping(self.buf.add(s * Self::LINE), self.heads[s].0.sub(n), n) };
                }
            }
            sfence_all();
        } else if self.wc {
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

// The interner. An ID that does not pack is rare in the generated data only by luck of the
// format (one generator version gives every eighth ID a 32-character suffix), so this path
// must scale: `intern_long` is called from every parse thread, for user IDs mostly with an
// ID that is already known.
//
// - An interned ID is one record in a bump arena of the thread that first saw it (no
//   allocation per ID): `IRec` header (hash, length, index), then the bytes. Records never
//   move and are never freed.
// - 4096 shards by the low bits of a hash of the bytes. A shard is an open-addressing table
//   of atomic words (16 hash bits, 48-bit record address); lookups take no lock and touch
//   the table word and the record. A miss takes the shard's lock, looks again, and inserts.
//   A full table is replaced by a bigger copy; the old one stays valid (and is never freed)
//   for readers that still hold it: they can only miss in it, which sends them to the lock.
// - The index comes from one atomic counter; the record's address by index sits in a
//   directory of 128K-entry chunks that is written before the table word is published and
//   read without a lock (`long_bytes`, `id_bytes`, `id_less`).
const INTERN_SHARD_BITS: u32 = 12;
const DIR_CHUNK_BITS: u32 = 17;

#[repr(C)]
struct IRec {
    hash: u64,
    len: u32,
    idx: u32,
    // the bytes follow
}
impl IRec {
    #[inline(always)]
    fn bytes(&self) -> &'static [u8] {
        unsafe { std::slice::from_raw_parts((self as *const IRec).add(1) as *const u8, self.len as usize) }
    }
}
struct InternTable {
    mask: usize,
    slots: Box<[AtomicU64]>,
}
struct InternShard {
    /// Guards inserts and table replacement; holds the number of entries.
    lock: Mutex<usize>,
    table: AtomicPtr<InternTable>,
}
static INTERN: std::sync::OnceLock<Box<[InternShard]>> = std::sync::OnceLock::new();
static INTERN_NEXT: AtomicU64 = AtomicU64::new(0);
/// Directory chunks: `1 << DIR_CHUNK_BITS` record addresses each.
static INTERN_DIR: [AtomicPtr<usize>; 1 << 15] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 1 << 15];
const ADDR: u64 = (1 << 48) - 1;

#[inline]
fn hash_bytes(s: &[u8]) -> u64 {
    let mut h = K0 ^ (s.len() as u64).wrapping_mul(K1);
    let mut c = s.chunks_exact(8);
    for w in &mut c {
        h = mum(h ^ u64::from_le_bytes(w.try_into().unwrap()), K1);
    }
    let r = c.remainder();
    if !r.is_empty() {
        let mut b = [0u8; 8];
        b[..r.len()].copy_from_slice(r);
        h = mum(h ^ u64::from_le_bytes(b), K1);
    }
    mum(h ^ K1, K0)
}

/// The directory entry of index `i`; its chunk is created if `make`.
#[inline]
fn intern_dir(i: usize, make: bool) -> *mut usize {
    let c = &INTERN_DIR[i >> DIR_CHUNK_BITS];
    let mut p = c.load(Ordering::Acquire);
    if p.is_null() {
        assert!(make);
        let n = os_alloc(std::mem::size_of::<usize>() << DIR_CHUNK_BITS) as *mut usize;
        p = match c.compare_exchange(std::ptr::null_mut(), n, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => n,
            Err(other) => other, // the lost chunk stays untouched (never freed)
        };
    }
    unsafe { p.add(i & ((1 << DIR_CHUNK_BITS) - 1)) }
}

/// The bytes of an interned ID (`is_long`). No lock; the storage never moves, so this is valid
/// for any handle obtained before the call, from any thread, while others are still interning.
#[inline]
pub fn long_bytes(id: Id) -> &'static [u8] {
    unsafe { (*(*intern_dir((id & !LONG) as usize, false) as *const IRec)).bytes() }
}

/// Where the probe sequence of hash `h` starts, and its tag (the top 16 bits, in place).
#[inline(always)]
fn intern_pos(h: u64, mask: usize) -> (usize, u64) {
    ((h >> INTERN_SHARD_BITS) as usize & mask, h & !ADDR)
}

/// Finds `s` (hash `h`) in `t`: its index.
#[inline]
fn intern_find(t: &InternTable, h: u64, s: &[u8]) -> Option<u64> {
    let (mut i, tag) = intern_pos(h, t.mask);
    loop {
        let v = unsafe { t.slots.get_unchecked(i) }.load(Ordering::Acquire);
        if v == 0 {
            return None;
        }
        if v & !ADDR == tag {
            let r = unsafe { &*((v & ADDR) as *const IRec) };
            if r.hash == h && r.bytes() == s {
                return Some(r.idx as u64);
            }
        }
        i = (i + 1) & t.mask;
    }
}

fn intern_table(cap: usize) -> *mut InternTable {
    Box::into_raw(Box::new(InternTable { mask: cap - 1, slots: (0..cap).map(|_| AtomicU64::new(0)).collect() }))
}

/// A new record for `s` in a bump arena of this thread (64 KiB at first, then 8 MiB slabs on huge pages).
fn intern_record(s: &[u8], hash: u64, idx: u32) -> *const IRec {
    thread_local! {
        static BUMP: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 64 << 10)) };
    }
    let need = (std::mem::size_of::<IRec>() + s.len() + 7) & !7;
    BUMP.with(|b| {
        let (mut cur, mut end, mut next) = b.get();
        if cur + need > end {
            let n = need.max(next);
            cur = os_alloc(n) as usize;
            end = cur + n;
            next = SLAB;
        }
        let r = cur as *mut IRec;
        unsafe {
            r.write(IRec { hash, len: s.len() as u32, idx });
            std::ptr::copy_nonoverlapping(s.as_ptr(), r.add(1) as *mut u8, s.len());
        }
        b.set((cur + need, end, next));
        r as *const IRec
    })
}

#[inline(never)]
pub fn intern_long(s: &[u8]) -> Id {
    let shards = INTERN.get_or_init(|| (0..1usize << INTERN_SHARD_BITS).map(|_| InternShard { lock: Mutex::new(0), table: AtomicPtr::new(std::ptr::null_mut()) }).collect());
    let h = hash_bytes(s);
    let sh = &shards[h as usize & ((1 << INTERN_SHARD_BITS) - 1)];
    let t = sh.table.load(Ordering::Acquire);
    if !t.is_null() {
        if let Some(i) = intern_find(unsafe { &*t }, h, s) {
            return i | LONG;
        }
    }
    let mut count = sh.lock.lock().unwrap();
    let mut t = sh.table.load(Ordering::Acquire);
    if !t.is_null() {
        if let Some(i) = intern_find(unsafe { &*t }, h, s) {
            return i | LONG;
        }
    }
    // Insert. First make room: at most 3/4 full.
    if t.is_null() || (*count + 1) * 4 > (unsafe { &*t }.mask + 1) * 3 {
        let n = intern_table(if t.is_null() { 16 } else { (unsafe { &*t }.mask + 1) * 2 });
        if !t.is_null() {
            let (old, new) = unsafe { (&*t, &*n) };
            for slot in old.slots.iter() {
                let v = slot.load(Ordering::Relaxed);
                if v != 0 {
                    let mut i = intern_pos(unsafe { (*((v & ADDR) as *const IRec)).hash }, new.mask).0;
                    while new.slots[i].load(Ordering::Relaxed) != 0 {
                        i = (i + 1) & new.mask;
                    }
                    new.slots[i].store(v, Ordering::Relaxed);
                }
            }
        }
        sh.table.store(n, Ordering::Release);
        t = n;
    }
    let idx = INTERN_NEXT.fetch_add(1, Ordering::Relaxed);
    if idx >= 0xFFFF_FFFF || s.len() > u32::MAX as usize {
        eprintln!("shopping-engine: too many distinct long IDs");
        std::process::exit(1);
    }
    let rec = intern_record(s, h, idx as u32);
    debug_assert!(rec as u64 & !ADDR == 0);
    unsafe { *intern_dir(idx as usize, true) = rec as usize };
    let t = unsafe { &*t };
    let (mut i, tag) = intern_pos(h, t.mask);
    while t.slots[i].load(Ordering::Relaxed) != 0 {
        i = (i + 1) & t.mask;
    }
    t.slots[i].store(tag | rec as u64, Ordering::Release);
    *count += 1;
    idx | LONG
}

#[inline(always)]
pub fn is_long(id: Id) -> bool {
    id & LONG != 0
}

/// Appends the ID's bytes to `out`.
#[inline]
pub fn id_bytes(id: Id, out: &mut Vec<u8>) {
    if is_long(id) {
        out.extend_from_slice(long_bytes(id));
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
    // the bytes of an interned ID as stored; a packed one decoded on the stack
    fn bytes(id: Id, buf: &mut [u8; 9]) -> &[u8] {
        if is_long(id) {
            return long_bytes(id);
        }
        let mut n = 0;
        while n < 9 {
            let c = ((id >> (56 - 7 * n as u32)) & 0x7F) as u8;
            if c == 0 {
                break;
            }
            buf[n] = c;
            n += 1;
        }
        &buf[..n]
    }
    let (mut x, mut y) = ([0u8; 9], [0u8; 9]);
    bytes(a, &mut x) < bytes(b, &mut y)
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
            return pack_words(p, len, lo, hi);
        }
        intern_long(std::slice::from_raw_parts(p, len))
    }
}

/// `pack_id` for a field with 16 readable bytes at `p`.
#[inline(always)]
pub unsafe fn pack_id16(p: *const u8, len: usize) -> Id {
    unsafe {
        if len <= 9 {
            // the first 8 bytes (zero padded), and the ninth
            let m = if len >= 8 { !0u64 } else { (1u64 << (8 * len as u32)) - 1 };
            let lo = (p as *const u64).read_unaligned() & m;
            let b8 = *p.add(8) as u64;
            let hi = if len == 9 { b8 } else { 0 };
            // Packable when no byte has its top bit set and none of the `len` bytes is zero
            // (`t`: the padding filled with ones, so that any zero byte left is a real one).
            let t = lo | !m;
            let zero = t.wrapping_sub(0x0101_0101_0101_0101) & !t;
            if (((lo | zero) & 0x8080_8080_8080_8080) == 0) & ((len < 9) | (b8.wrapping_sub(1) < 127)) {
                let x = lo.swap_bytes();
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
            if len == 0 {
                return 0;
            }
        }
        intern_long(std::slice::from_raw_parts(p, len))
    }
}

/// Byte masks of the first `len` (0..=9) of 16 bytes.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
pub static LEN_MASK: [[u8; 16]; 10] = {
    let mut t = [[0u8; 16]; 10];
    let mut l = 0;
    while l < 10 {
        let mut i = 0;
        while i < l {
            t[l][i] = 0xFF;
            i += 1;
        }
        l += 1;
    }
    t
};

/// Two IDs packed at once: `Some((pack_id16(p0, l0), pack_id16(p1, l1)))` when both are
/// short and plain (at most 9 bytes, each in 1..=127; an empty one is 0), else `None` and
/// nothing has happened. 16 bytes must be readable at both pointers.
#[inline(always)]
pub unsafe fn pack2_id16(p0: *const u8, l0: usize, p1: *const u8, l1: usize) -> Option<(Id, Id)> {
    unsafe {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
        {
            use std::arch::x86_64::*;
            if (l0 > 9) | (l1 > 9) {
                return None;
            }
            let ld = |a: *const u8, b: *const u8| _mm256_inserti128_si256(_mm256_castsi128_si256(_mm_loadu_si128(a as *const __m128i)), _mm_loadu_si128(b as *const __m128i), 1);
            let data = ld(p0, p1);
            let mask = ld(LEN_MASK.get_unchecked(l0).as_ptr(), LEN_MASK.get_unchecked(l1).as_ptr());
            // every byte of both IDs is in 1..=127
            let plain = _mm256_cmpgt_epi8(data, _mm256_setzero_si256());
            if _mm256_testc_si256(plain, mask) == 0 {
                return None;
            }
            let d = _mm256_and_si256(data, mask);
            // 7 bits a byte, first byte most significant: pairs of bytes, pairs of those,
            // then the two halves of the first eight bytes; the ninth byte goes below them
            let w = _mm256_maddubs_epi16(_mm256_set1_epi16(0x0180), d);
            let dd = _mm256_madd_epi16(w, _mm256_set1_epi32(0x0001_4000));
            let q = _mm256_or_si256(_mm256_mul_epu32(dd, _mm256_set1_epi64x(1 << 28)), _mm256_srli_epi64(dd, 32));
            let id = _mm256_or_si256(_mm256_slli_epi64(q, 7), _mm256_srli_si256(d, 8));
            Some((_mm_cvtsi128_si64(_mm256_castsi256_si128(id)) as u64, _mm_cvtsi128_si64(_mm256_extracti128_si256(id, 1)) as u64))
        }
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
        {
            let plain = |p: *const u8, l: usize| l <= 9 && std::slice::from_raw_parts(p, l).iter().all(|&c| (1..=127).contains(&c));
            if plain(p0, l0) && plain(p1, l1) { Some((pack_id16(p0, l0), pack_id16(p1, l1))) } else { None }
        }
    }
}

/// `lo`, `hi`: the field's `len` (<= 9) bytes, little-endian, zero padded.
#[inline(always)]
unsafe fn pack_words(p: *const u8, len: usize, lo: u64, hi: u64) -> Id {
    unsafe {
        // zero bytes of `lo` (exact when no byte has its top bit set)
        let z = lo.wrapping_sub(0x0101_0101_0101_0101) & !lo & 0x8080_8080_8080_8080;
        let want = if len >= 8 { 0 } else { 0x8080_8080_8080_8080u64 << (8 * len as u32) };
        if (((lo | hi) & 0x8080_8080_8080_8080) == 0) & (z == want) & ((len < 9) | (hi != 0)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    /// Byte string number `k` of a family of distinct strings of 10..=40 bytes.
    fn long_name(k: u64) -> Vec<u8> {
        let mut v = format!("u{k:x}-").into_bytes();
        let mut x = k.wrapping_mul(K0) | 1;
        while v.len() < 10 + (k % 31) as usize {
            v.push(b"0123456789abcdef\xC3\xA9"[(rng(&mut x) % 18) as usize]);
        }
        v
    }

    #[test]
    fn intern_concurrent() {
        // 8 threads intern the same 131072 strings in different orders (plus private ones).
        const N: u64 = 1 << 17; // a power of two: the odd multipliers below give permutations
        let res = par((0..8u64).collect::<Vec<u64>>(), |_, t| {
            let mut out = vec![0u64; N as usize];
            let mut x = 0x1234_5678 + t;
            for round in 0..3 * N {
                let k = if round < N { (round * (2 * t + 1) + t * 7919) % N } else { rng(&mut x) % N };
                let id = intern_long(&long_name(k));
                assert!(is_long(id));
                assert!(out[k as usize] == 0 || out[k as usize] == id);
                out[k as usize] = id;
            }
            let own = intern_long(format!("private-thread-{t}").as_bytes());
            (out, own)
        });
        let mut seen = std::collections::HashSet::new();
        for k in 0..N as usize {
            for r in &res {
                assert_eq!(r.0[k], res[0].0[k], "one handle per string");
            }
            assert!(seen.insert(res[0].0[k]), "distinct strings, distinct handles");
            let mut b = Vec::new();
            id_bytes(res[0].0[k], &mut b);
            assert_eq!(b, long_name(k as u64));
        }
        for (t, r) in res.iter().enumerate() {
            assert!(seen.insert(r.1));
            let mut b = Vec::new();
            id_bytes(r.1, &mut b);
            assert_eq!(b, format!("private-thread-{t}").into_bytes());
            assert_eq!(intern_long(&b), r.1);
        }
    }

    #[test]
    fn id_less_is_bytewise() {
        let mut x = 0xFEED_F00D_u64;
        let mut ids: Vec<(Vec<u8>, Id)> = Vec::new();
        for _ in 0..3000 {
            let len = 1 + (rng(&mut x) % 14) as usize;
            let alpha: &[u8] = if rng(&mut x) % 4 == 0 { b"ab\x01\x7f\x80\xff\x00z" } else { b"abz019" };
            // few distinct letters and shared prefixes: many near-equal pairs
            let s: Vec<u8> = (0..len).map(|_| alpha[(rng(&mut x) % alpha.len() as u64) as usize]).collect();
            let id = unsafe { pack_id(s.as_ptr(), s.len(), s.as_ptr().add(s.len())) };
            let mut b = Vec::new();
            id_bytes(id, &mut b);
            assert_eq!(b, s);
            ids.push((s, id));
        }
        for i in 0..ids.len() {
            for j in (i % 7..ids.len()).step_by(7) {
                assert_eq!(id_less(ids[i].1, ids[j].1), ids[i].0 < ids[j].0, "{:?} {:?}", ids[i].0, ids[j].0);
                assert_eq!(ids[i].1 == ids[j].1, ids[i].0 == ids[j].0);
            }
        }
    }

    #[test]
    fn pack_id16_is_pack_id() {
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let mut buf = [0u8; 64];
        for round in 0..2_000_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mut y = x;
            for b in buf.iter_mut().take(48) {
                y = y.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23);
                // mostly letters and digits, now and then a NUL, a high byte or anything
                *b = match (y >> 40) % 23 {
                    0 => 0,
                    1 => 0x80 | (y >> 8) as u8,
                    2 => (y >> 8) as u8,
                    _ => b"abcxyz019_~\x01\x7f"[((y >> 16) % 13) as usize],
                };
            }
            let len = (round % 11) as usize;
            let lim = unsafe { buf.as_ptr().add(64) };
            // long or unpackable IDs are interned by content: both sides give the same index
            let (a, b) = unsafe { (pack_id16(buf.as_ptr(), len), pack_id(buf.as_ptr(), len, lim)) };
            assert_eq!(a, b, "{:?}", &buf[..len]);
            let b2 = unsafe { pack_id(buf.as_ptr(), len, buf.as_ptr().add(len)) };
            assert_eq!(a, b2);
            // the pair version: the same values, or None exactly when one is not short and plain
            let len2 = ((x >> 20) % 11) as usize;
            let q = unsafe { buf.as_ptr().add(30) };
            let plain = |o: usize, l: usize| l <= 9 && buf[o..o + l].iter().all(|&c| (1..=127).contains(&c));
            let want = if plain(0, len) && plain(30, len2) { Some((a, unsafe { pack_id16(q, len2) })) } else { None };
            assert_eq!(unsafe { pack2_id16(buf.as_ptr(), len, q, len2) }, want, "{:?} {:?}", &buf[..len], &buf[30..30 + len2]);
        }
    }
}
