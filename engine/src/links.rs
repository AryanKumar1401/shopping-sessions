//! Linked IDs: a partitioned hash table of every ID in links/, a concurrent union-find over
//! dense numbers of its IDs that always links the byte-wise larger root under the smaller one
//! (so a root is the person's name), and a final table `ID -> person name`.

use crate::util::*;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

#[repr(C)]
pub struct Slot {
    pub key: Id,
    /// During the build: the ID's dense number. Afterwards: the person's name.
    pub aux: u64,
}

#[derive(Clone, Copy)]
pub struct Occ {
    pub id: Id,
    pub edge: u32,
    /// tid << 1 | side
    pub who: u32,
}

pub struct LinkTable {
    slots: *mut Slot,
    base: Vec<u32>,
    mask: Vec<u32>,
    bits: u32,
    pub any: bool,
}
unsafe impl Send for LinkTable {}
unsafe impl Sync for LinkTable {}

/// How far ahead the build loops prefetch.
const AHEAD: usize = 12;

#[inline(always)]
pub fn part_of(h: u64, bits: u32) -> usize {
    ((h >> 1) >> (63 - bits)) as usize
}

impl LinkTable {
    pub fn empty() -> Self {
        LinkTable { slots: std::ptr::null_mut(), base: vec![0], mask: vec![0], bits: 0, any: false }
    }

    #[inline(always)]
    pub fn prefetch(&self, h: u64) {
        unsafe {
            let p = part_of(h, self.bits);
            prefetch(self.slots.add((*self.base.get_unchecked(p) + (h as u32 & *self.mask.get_unchecked(p) & !3)) as usize));
        }
    }

    /// The name of the person `id` belongs to (`h` = `id_hash(id)`).
    ///
    /// A partition is probed in buckets of four slots (one cache line), starting at the bucket
    /// `h` selects; `build` fills slots in the same order. All four keys are compared without
    /// branching: whether an ID is linked is a coin toss the branch predictor cannot learn.
    #[inline(always)]
    pub fn resolve(&self, id: Id, h: u64) -> Id {
        unsafe {
            let p = part_of(h, self.bits);
            let base = *self.base.get_unchecked(p) as usize;
            let mask = *self.mask.get_unchecked(p);
            let mut i = h as u32 & mask & !3;
            loop {
                let b = self.slots.add(base + i as usize);
                let (mut hit, mut empty, mut val) = (0u64, 0u64, 0u64);
                for j in 0..4 {
                    let s = &*b.add(j);
                    let e = (s.key == id) as u64;
                    hit |= e;
                    val |= s.aux & e.wrapping_neg();
                    empty |= (s.key == 0) as u64;
                }
                if (hit | empty) != 0 {
                    // hit: the stored name; otherwise the ID itself
                    return val | (id & hit.wrapping_sub(1));
                }
                i = (i + 4) & mask;
            }
        }
    }

    /// `occ[t]`: thread t's ID occurrences partitioned by `part_of(id_hash, bits)`;
    /// `edges[t]`: thread t's link lines, filled here with the dense numbers of their IDs.
    ///
    /// The slot table is too big for the caches (64 MB at 20M events), so the union-find does
    /// not live in it: while a partition's IDs are inserted, each distinct ID gets a dense
    /// number (the partition's first number + its rank there) and the build works on two
    /// dense arrays, `parent` (4 bytes an ID) and `keys` (8 bytes): a few MB. A union puts the
    /// byte-wise larger root under the smaller one, so a root's key is the person's name, and
    /// one last pass writes `keys[find(number)]` into every used slot.
    pub fn build(occ: &[Parts<Occ>], edges: &mut [Vec<[u32; 2]>], bits: u32, nthreads: usize) -> Option<LinkTable> {
        let np = 1usize << bits;
        let mut base = Vec::with_capacity(np);
        let mut mask = Vec::with_capacity(np);
        // first dense number of each partition (room for every occurrence being distinct)
        let mut dbase = Vec::with_capacity(np);
        let mut total = 0u64;
        let mut nocc = 0u64;
        for p in 0..np {
            let n: usize = occ.iter().map(|o| o.count(p)).sum();
            // `n` counts occurrences; about 0.7 n IDs are distinct in the generated data, so
            // this is 30-60% full there and never full (the probe loops need an empty slot).
            // (Twice as many slots were measured: link build 232 vs 206 ms on one thread at
            // 20M events, people phase the same.)
            let cap = (n + n / 8 + 4).next_power_of_two();
            base.push(total as u32);
            mask.push(cap as u32 - 1);
            dbase.push(nocc as u32);
            total += cap as u64;
            nocc += n as u64;
        }
        if nocc == 0 {
            return Some(LinkTable::empty());
        }
        if total >= u32::MAX as u64 {
            return None;
        }
        let tt = std::time::Instant::now();
        let slots = os_alloc(total as usize * 16) as *mut Slot;
        let t = LinkTable { slots, base, mask, bits, any: true };
        let uf = Dense { parent: os_alloc(nocc as usize * 4) as *const AtomicU32, keys: os_alloc(nocc as usize * 8) as *mut Id };
        let eptr: Vec<usize> = edges.iter_mut().map(|e| e.as_mut_ptr() as usize).collect();
        let next = AtomicUsize::new(0);
        par(vec![(); nthreads], |_, _| unsafe {
            let (t, uf) = (&t, &uf);
            loop {
                let p = next.fetch_add(1, Relaxed);
                if p >= np {
                    break;
                }
                let b = t.base[p];
                let m = t.mask[p];
                let mut d = dbase[p];
                for o in occ {
                    for sl in o.slices(p) {
                        for (k, oc) in sl.iter().enumerate() {
                            // a few records ahead: its bucket and the edge word it will fill
                            if let Some(n) = sl.get(k + AHEAD) {
                                prefetch(t.slots.add((b + (id_hash(n.id) as u32 & m & !3)) as usize));
                                prefetch((eptr[(n.who >> 1) as usize] as *const [u32; 2]).add(n.edge as usize));
                            }
                            let mut i = id_hash(oc.id) as u32 & m & !3;
                            let g = loop {
                                let s = &mut *t.slots.add((b + i) as usize);
                                if s.key == oc.id {
                                    break s.aux as u32;
                                }
                                if s.key == 0 {
                                    s.key = oc.id;
                                    s.aux = d as u64;
                                    (*uf.parent.add(d as usize)).store(d, Relaxed);
                                    *uf.keys.add(d as usize) = oc.id;
                                    d += 1;
                                    break d - 1;
                                }
                                i = (i + 1) & m;
                            };
                            let e = eptr[(oc.who >> 1) as usize] as *mut [u32; 2];
                            (*e.add(oc.edge as usize))[(oc.who & 1) as usize] = g;
                        }
                    }
                }
            }
        });
        let t_ins = tt.elapsed();
        let edges_ref: &[Vec<[u32; 2]>] = edges;
        par((0..edges_ref.len()).collect(), |_, i: usize| unsafe {
            let uf = &uf;
            let es = &edges_ref[i];
            for (k, e) in es.iter().enumerate() {
                if let Some(n) = es.get(k + AHEAD) {
                    prefetch(uf.parent.add(n[0] as usize));
                    prefetch(uf.parent.add(n[1] as usize));
                }
                uf.union(e[0], e[1]);
            }
        });
        let t_un = tt.elapsed();
        let total = total as usize;
        let step = (total / (nthreads * 8)).max(4096);
        let next = AtomicUsize::new(0);
        par(vec![(); nthreads], |_, _| unsafe {
            let (t, uf) = (&t, &uf);
            loop {
                let s0 = next.fetch_add(step, Relaxed);
                if s0 >= total {
                    break;
                }
                let end = (s0 + step).min(total);
                for s in s0..end {
                    if s + AHEAD < end {
                        let n = t.slots.add(s + AHEAD);
                        if (*n).key != 0 {
                            prefetch(uf.parent.add((*n).aux as usize));
                        }
                    }
                    let sl = t.slots.add(s);
                    if (*sl).key != 0 {
                        (*sl).aux = *uf.keys.add(uf.find((*sl).aux as u32) as usize);
                    }
                }
            }
        });
        if env_flag("SE_TIMING") {
            eprintln!("[links] slots {} ({} MB), occurrences {}, lines {}: insert {:.1} ms, union {:.1} ms, names {:.1} ms", total, total * 16 >> 20, nocc, edges.iter().map(|e| e.len()).sum::<usize>(), t_ins.as_secs_f64() * 1e3, (t_un - t_ins).as_secs_f64() * 1e3, (tt.elapsed() - t_un).as_secs_f64() * 1e3);
        }
        Some(t)
    }
}

/// The union-find of `LinkTable::build`, over dense ID numbers: `parent[x]` (a root is its
/// own parent) and `keys[x]`, the ID. Lock-free: several threads union at once.
struct Dense {
    parent: *const AtomicU32,
    keys: *mut Id,
}
unsafe impl Sync for Dense {}

impl Dense {
    #[inline(always)]
    unsafe fn parent(&self, x: u32) -> &AtomicU32 {
        unsafe { &*self.parent.add(x as usize) }
    }

    unsafe fn find(&self, mut x: u32) -> u32 {
        unsafe {
            loop {
                let p = self.parent(x).load(Relaxed);
                if p == x {
                    return x;
                }
                let gp = self.parent(p).load(Relaxed);
                if gp != p {
                    let _ = self.parent(x).compare_exchange_weak(p, gp, Relaxed, Relaxed);
                }
                x = p;
            }
        }
    }

    unsafe fn union(&self, a: u32, b: u32) {
        unsafe {
            loop {
                let ra = self.find(a);
                let rb = self.find(b);
                if ra == rb {
                    return;
                }
                let (small, large) = if id_less(*self.keys.add(ra as usize), *self.keys.add(rb as usize)) { (ra, rb) } else { (rb, ra) };
                if self.parent(large).compare_exchange(large, small, Relaxed, Relaxed).is_ok() {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- person pass helpers (begin)
impl LinkTable {
    /// The name of the person `id` belongs to, or 0 when `id` is in no link line
    /// (`h` = `id_hash(id)`). Must not be called on an empty table (`any` false).
    #[inline(always)]
    pub fn lookup(&self, id: Id, h: u64) -> Id {
        unsafe {
            let p = part_of(h, self.bits);
            let base = *self.base.get_unchecked(p) as usize;
            let mask = *self.mask.get_unchecked(p);
            let mut i = h as u32 & mask & !3;
            loop {
                let b = self.slots.add(base + i as usize);
                let (mut hit, mut empty, mut val) = (0u64, 0u64, 0u64);
                for j in 0..4 {
                    let s = &*b.add(j);
                    let e = (s.key == id) as u64;
                    hit |= e;
                    val |= s.aux & e.wrapping_neg();
                    empty |= (s.key == 0) as u64;
                }
                if (hit | empty) != 0 {
                    return val;
                }
                i = (i + 4) & mask;
            }
        }
    }
}
// ---------------------------------------------------------------- person pass helpers (end)
