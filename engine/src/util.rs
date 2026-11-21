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
    }
    p as *mut u8
}

/// Per-thread bump allocator over big OS slabs. Memory is never returned.
pub struct Arena {
    cur: usize,
    end: usize,
}
const SLAB: usize = 8 << 20;
impl Arena {
    pub fn new() -> Self {
        Arena { cur: 0, end: 0 }
    }
    #[inline]
    pub fn alloc(&mut self, bytes: usize) -> *mut u8 {
        let bytes = (bytes + 63) & !63;
        if self.cur + bytes > self.end {
            let slab = bytes.max(SLAB);
            self.cur = os_alloc(slab) as usize;
            self.end = self.cur + slab;
        }
        let p = self.cur;
        self.cur += bytes;
        p as *mut u8
    }
}

/// Fan-out writer: one append-only stream of `T` per partition, stored as blocks.
pub struct Writers<T> {
    heads: Vec<(*mut T, *mut T)>,
    blocks: Vec<Vec<(usize, usize)>>, // (start ptr, capacity) per block; the last one is open
    block_len: usize,
    pub arena: Arena,
}
unsafe impl<T> Send for Writers<T> {}

impl<T: Copy> Writers<T> {
    pub fn new(parts: usize, block_len: usize) -> Self {
        Writers {
            heads: vec![(std::ptr::null_mut(), std::ptr::null_mut()); parts],
            blocks: vec![Vec::new(); parts],
            block_len: block_len.max(4),
            arena: Arena::new(),
        }
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
        unsafe {
            if self.heads.get_unchecked(s).0 == self.heads.get_unchecked(s).1 {
                self.refill(s);
            }
            let h = self.heads.get_unchecked_mut(s);
            h.0.write(v);
            h.0 = h.0.add(1);
        }
    }
    /// Like `push`, returning where the value now lives.
    #[inline(always)]
    pub fn push_ptr(&mut self, s: usize, v: T) -> *mut T {
        unsafe {
            if self.heads.get_unchecked(s).0 == self.heads.get_unchecked(s).1 {
                self.refill(s);
            }
            let h = self.heads.get_unchecked_mut(s);
            let p = h.0;
            p.write(v);
            h.0 = p.add(1);
            p
        }
    }
    /// Per partition: the filled slices.
    pub fn finish(self) -> Parts<T> {
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
pub fn intern_long(s: &'static [u8]) -> Id {
    let it = interner();
    let mut m = it.map.lock().unwrap();
    if let Some(&i) = m.get(s) {
        return i | LONG;
    }
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
                let x = ((x & 0x7F00_7F00_7F00_7F00) >> 1) | (x & 0x007F_007F_007F_007F);
                let x = ((x & 0x3FFF_0000_3FFF_0000) >> 2) | (x & 0x0000_3FFF_0000_3FFF);
                let x = ((x & 0x0FFF_FFFF_0000_0000) >> 4) | (x & 0x0000_0000_0FFF_FFFF);
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
