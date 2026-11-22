//! Speed-of-light probe: how fast can 4..8 threads touch / scan the input CSVs on this machine.
//! sol DATA_DIR THREADS MODE   (MODE: 0 touch, 1 newlines, 2 masks, 3 masks+positions)
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

fn files(dir: &std::path::Path) -> Vec<&'static [u8]> {
    use std::os::unix::io::AsRawFd;
    let mut v = Vec::new();
    for sub in ["events", "clicks"] {
        let mut ps: Vec<_> = std::fs::read_dir(dir.join(sub)).unwrap().flatten().map(|e| e.path()).collect();
        ps.sort();
        for p in ps {
            let f = std::fs::File::open(&p).unwrap();
            let n = f.metadata().unwrap().len() as usize;
            if n == 0 { continue; }
            let a = unsafe { mmap(std::ptr::null_mut(), n, 1, 2, f.as_raw_fd(), 0) };
            unsafe { madvise(a, n, 22) }; // MADV_POPULATE_READ (ignored if unsupported)
            v.push(unsafe { std::slice::from_raw_parts(a as *const u8, n) });
            std::mem::forget(f);
        }
    }
    v
}

#[target_feature(enable = "avx2,bmi1,bmi2,popcnt")]
unsafe fn work(d: &[u8], mode: u32, pos: &mut Vec<u32>) -> u64 {
    use std::arch::x86_64::*;
    let (kc, kn, kq) = (_mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b'"' as i8));
    let n = d.len() / 64 * 64;
    let p = d.as_ptr();
    let mut acc = _mm256_setzero_si256();
    let mut cnt = 0u64;
    if pos.capacity() < 4096 { pos.reserve(4096); }
    let mut i = 0;
    while i < n {
        let a = _mm256_loadu_si256(p.add(i) as *const __m256i);
        let b = _mm256_loadu_si256(p.add(i + 32) as *const __m256i);
        match mode {
            0 => { acc = _mm256_xor_si256(acc, _mm256_xor_si256(a, b)); }
            1 => {
                let x = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, kn)) as u32 as u64 | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, kn)) as u32 as u64) << 32);
                cnt += x.count_ones() as u64;
            }
            _ => {
                let na = _mm256_cmpeq_epi8(a, kn); let nb = _mm256_cmpeq_epi8(b, kn);
                let da = _mm256_or_si256(_mm256_cmpeq_epi8(a, kc), na); let db = _mm256_or_si256(_mm256_cmpeq_epi8(b, kc), nb);
                let dm = _mm256_movemask_epi8(da) as u32 as u64 | ((_mm256_movemask_epi8(db) as u32 as u64) << 32);
                let nl = _mm256_movemask_epi8(na) as u32 as u64 | ((_mm256_movemask_epi8(nb) as u32 as u64) << 32);
                let q = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, kq)) as u32 as u64 | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, kq)) as u32 as u64) << 32);
                if mode == 2 {
                    cnt += (dm.count_ones() + nl.count_ones() + q.count_ones()) as u64;
                } else {
                    let mut m = dm;
                    let o = pos.as_mut_ptr();
                    let mut k = 0usize;
                    loop {
                        for j in 0..8 { *o.add(k + j) = i as u32 + m.trailing_zeros(); m &= m.wrapping_sub(1); }
                        if m == 0 { break; }
                        k += 8;
                    }
                    cnt += (dm.count_ones() + nl.count_ones() + q.count_ones()) as u64 + *o as u64;
                }
            }
        }
        i += 64;
    }
    let mut t = [0u64; 4];
    _mm256_storeu_si256(t.as_mut_ptr() as *mut __m256i, acc);
    cnt + t[0] + t[1] + t[2] + t[3]
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let fs = files(std::path::Path::new(&a[1]));
    let nt: usize = a[2].parse().unwrap();
    let mode: u32 = a[3].parse().unwrap();
    let mut jobs = Vec::new();
    for f in &fs { let mut s = 0; while s < f.len() { let e = (s + (2 << 20)).min(f.len()); jobs.push(&f[s..e]); s = e; } }
    let total: usize = fs.iter().map(|f| f.len()).sum();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let next = AtomicUsize::new(0);
        let t0 = std::time::Instant::now();
        let sum: u64 = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..nt).map(|_| sc.spawn(|| { let mut pos = Vec::new(); let mut s = 0u64; loop { let j = next.fetch_add(1, Relaxed); if j >= jobs.len() { break; } s += unsafe { work(jobs[j], mode, &mut pos) }; } s })).collect();
            hs.into_iter().map(|h| h.join().unwrap()).sum()
        });
        let t = t0.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(sum);
        if t < best { best = t; }
    }
    println!("mode {mode} threads {nt}: {best:.1} ms for {:.2} GB ({:.1} GB/s)", total as f64 / 1e9, total as f64 / 1e9 / (best / 1e3));
}
