//! Linked IDs: a partitioned hash table of every ID in links/, a concurrent union-find over
//! its slots that always links the byte-wise larger root under the smaller one (so a root is
//! the person's name), and a final table `ID -> person name`.

use crate::util::*;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

#[repr(C)]
pub struct Slot {
    pub key: Id,
    /// During the build: low 32 bits = union-find parent, high 32 bits = root. Afterwards: the person's name.
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
            prefetch(self.slots.add((*self.base.get_unchecked(p) + (h as u32 & *self.mask.get_unchecked(p))) as usize));
        }
    }

    /// The name of the person `id` belongs to (`h` = `id_hash(id)`).
    #[inline(always)]
    pub fn resolve(&self, id: Id, h: u64) -> Id {
        unsafe {
            let p = part_of(h, self.bits);
            let base = *self.base.get_unchecked(p) as usize;
            let mask = *self.mask.get_unchecked(p);
            let mut i = h as u32 & mask;
            loop {
                let s = &*self.slots.add(base + i as usize);
                if s.key == id {
                    return s.aux;
                }
                if s.key == 0 {
                    return id;
                }
                i = (i + 1) & mask;
            }
        }
    }

    #[inline(always)]
    unsafe fn parent(&self, s: u32) -> &AtomicU32 {
        unsafe { &*(std::ptr::addr_of!((*self.slots.add(s as usize)).aux) as *const AtomicU32) }
    }
    #[inline(always)]
    unsafe fn key(&self, s: u32) -> Id {
        unsafe { (*self.slots.add(s as usize)).key }
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
                let (small, large) = if id_less(self.key(ra), self.key(rb)) { (ra, rb) } else { (rb, ra) };
                if self.parent(large).compare_exchange(large, small, Relaxed, Relaxed).is_ok() {
                    return;
                }
            }
        }
    }

    /// `occ[t]`: thread t's ID occurrences partitioned by `part_of(id_hash, bits)`;
    /// `edges[t]`: thread t's link lines, filled here with slot numbers.
    pub fn build(occ: &[Parts<Occ>], edges: &mut [Vec<[u32; 2]>], bits: u32, nthreads: usize) -> Option<LinkTable> {
        let np = 1usize << bits;
        let mut base = Vec::with_capacity(np);
        let mut mask = Vec::with_capacity(np);
        let mut total = 0u64;
        let mut any = false;
        for p in 0..np {
            let n: usize = occ.iter().map(|o| o.count(p)).sum();
            any |= n > 0;
            let cap = (n * 2).max(2).next_power_of_two();
            base.push(total as u32);
            mask.push(cap as u32 - 1);
            total += cap as u64;
        }
        if !any {
            return Some(LinkTable::empty());
        }
        if total >= u32::MAX as u64 {
            return None;
        }
        let slots = os_alloc(total as usize * 16) as *mut Slot;
        let t = LinkTable { slots, base, mask, bits, any: true };
        let eptr: Vec<usize> = edges.iter_mut().map(|e| e.as_mut_ptr() as usize).collect();
        let next = AtomicUsize::new(0);
        par(vec![(); nthreads], |_, _| unsafe {
            let t = &t;
            loop {
                let p = next.fetch_add(1, Relaxed);
                if p >= np {
                    break;
                }
                let b = t.base[p];
                let m = t.mask[p];
                for o in occ {
                    for sl in o.slices(p) {
                        for oc in sl.iter() {
                            let mut i = id_hash(oc.id) as u32 & m;
                            let g = loop {
                                let s = &mut *t.slots.add((b + i) as usize);
                                if s.key == oc.id {
                                    break b + i;
                                }
                                if s.key == 0 {
                                    s.key = oc.id;
                                    s.aux = (b + i) as u64;
                                    break b + i;
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
        let edges_ref: &[Vec<[u32; 2]>] = edges;
        par((0..edges_ref.len()).collect(), |_, i: usize| unsafe {
            let t = &t;
            for e in &edges_ref[i] {
                t.union(e[0], e[1]);
            }
        });
        let total = total as usize;
        let step = (total / (nthreads * 8)).max(4096);
        for phase in 0..2 {
            let next = AtomicUsize::new(0);
            par(vec![(); nthreads], |_, _| unsafe {
                let t = &t;
                loop {
                    let s0 = next.fetch_add(step, Relaxed);
                    if s0 >= total {
                        break;
                    }
                    for s in s0..(s0 + step).min(total) {
                        let s = s as u32;
                        let sl = t.slots.add(s as usize);
                        if (*sl).key != 0 {
                            if phase == 0 {
                                *(std::ptr::addr_of_mut!((*sl).aux) as *mut u32).add(1) = t.find(s);
                            } else {
                                (*sl).aux = t.key(((*sl).aux >> 32) as u32);
                            }
                        }
                    }
                }
            });
        }
        Some(t)
    }
}
