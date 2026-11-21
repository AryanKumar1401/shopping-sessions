//! CSV row scanning per the rules' dialect: only `\n` ends a line, every `,` separates fields,
//! a `"` is legal only as the first and last byte of a field (len >= 2), anything else makes the
//! line malformed, as does a wrong field count. Fields are handed out with quotes stripped.

/// Delimiter bitmasks for 64 bytes: (comma | newline, newline, quote).
pub trait Masker {
    /// True: use the scalar reference scanner instead of the masks.
    const SCALAR: bool = false;
    unsafe fn masks(p: *const u8) -> (u64, u64, u64);
}

pub struct Scalar;
impl Masker for Scalar {
    const SCALAR: bool = true;
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        unsafe { Swar::masks(p) }
    }
}

pub struct Swar;
impl Masker for Swar {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        unsafe {
            let (mut c, mut n, mut q) = (0u64, 0u64, 0u64);
            for i in 0..64 {
                let b = *p.add(i);
                c |= ((b == b',') as u64) << i;
                n |= ((b == b'\n') as u64) << i;
                q |= ((b == b'"') as u64) << i;
            }
            (c | n, n, q)
        }
    }
}

#[cfg(target_arch = "aarch64")]
pub struct Neon;
#[cfg(target_arch = "aarch64")]
impl Masker for Neon {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        use std::arch::aarch64::*;
        unsafe {
            let v0 = vld1q_u8(p);
            let v1 = vld1q_u8(p.add(16));
            let v2 = vld1q_u8(p.add(32));
            let v3 = vld1q_u8(p.add(48));
            let bm: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            let bit = vld1q_u8(bm.as_ptr());
            let mm = |c: u8| -> u64 {
                let k = vdupq_n_u8(c);
                let t0 = vandq_u8(vceqq_u8(v0, k), bit);
                let t1 = vandq_u8(vceqq_u8(v1, k), bit);
                let t2 = vandq_u8(vceqq_u8(v2, k), bit);
                let t3 = vandq_u8(vceqq_u8(v3, k), bit);
                let s0 = vpaddq_u8(t0, t1);
                let s1 = vpaddq_u8(t2, t3);
                let s = vpaddq_u8(s0, s1);
                let s = vpaddq_u8(s, s);
                vgetq_lane_u64(vreinterpretq_u64_u8(s), 0)
            };
            let n = mm(b'\n');
            (mm(b',') | n, n, mm(b'"'))
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub struct Sse2;
#[cfg(target_arch = "x86_64")]
impl Masker for Sse2 {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        use std::arch::x86_64::*;
        unsafe {
            let v = [
                _mm_loadu_si128(p as *const __m128i),
                _mm_loadu_si128(p.add(16) as *const __m128i),
                _mm_loadu_si128(p.add(32) as *const __m128i),
                _mm_loadu_si128(p.add(48) as *const __m128i),
            ];
            let mm = |c: u8| -> u64 {
                let k = _mm_set1_epi8(c as i8);
                let mut r = 0u64;
                for i in 0..4 {
                    r |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v[i], k)) as u32 as u64) << (16 * i);
                }
                r
            };
            let n = mm(b'\n');
            (mm(b',') | n, n, mm(b'"'))
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub struct Avx2;
#[cfg(target_arch = "x86_64")]
impl Masker for Avx2 {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        use std::arch::x86_64::*;
        unsafe {
            let a = _mm256_loadu_si256(p as *const __m256i);
            let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
            let (kc, kn, kq) = (_mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b'"' as i8));
            let (na, nb) = (_mm256_cmpeq_epi8(a, kn), _mm256_cmpeq_epi8(b, kn));
            let da = _mm256_or_si256(_mm256_cmpeq_epi8(a, kc), na);
            let db = _mm256_or_si256(_mm256_cmpeq_epi8(b, kc), nb);
            // Each 32-bit mask is joined to its other half as plain integers. (Written as
            // `mm(',') | mm('\n')` over 64-bit masks, LLVM merged the halves into one
            // 64-lane mask vector, which AVX2 cannot hold: it was rebuilt bit by bit, at
            // several hundred instructions per 64 bytes.)
            let join = |lo: __m256i, hi: __m256i| -> u64 {
                let lo = std::hint::black_box(_mm256_movemask_epi8(lo) as u32) as u64;
                let hi = _mm256_movemask_epi8(hi) as u32 as u64;
                lo | (hi << 32)
            };
            (join(da, db), join(na, nb), join(_mm256_cmpeq_epi8(a, kq), _mm256_cmpeq_epi8(b, kq)))
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub struct Avx512;
#[cfg(target_arch = "x86_64")]
impl Masker for Avx512 {
    #[inline(always)]
    unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
        use std::arch::x86_64::*;
        unsafe {
            let v = _mm512_loadu_si512(p as *const _);
            let n = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8));
            let c = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8));
            let q = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8));
            (c | n, n, q)
        }
    }
}

/// A field: pointer and length (quotes already stripped).
pub type Field = (*const u8, usize);

pub struct ScanBuf {
    d: Vec<u32>,
    nls: Vec<u32>,
    nlq: Vec<u32>,
}
impl ScanBuf {
    pub fn new() -> Self {
        ScanBuf { d: Vec::new(), nls: Vec::new(), nlq: Vec::new() }
    }
}

const PIECE: usize = 8192;

/// Splits one line (without its `\n`) into exactly `N` fields; `None` when malformed.
#[inline]
pub unsafe fn split_row<const N: usize>(p: *const u8, len: usize) -> Option<[Field; N]> {
    unsafe {
        let b = std::slice::from_raw_parts(p, len);
        let mut out: [Field; N] = [(p, 0); N];
        let mut k = 0;
        let mut start = 0;
        for i in 0..=len {
            if i == len || b[i] == b',' {
                if k == N {
                    return None;
                }
                let f = &b[start..i];
                let nq = f.iter().filter(|&&c| c == b'"').count();
                if nq == 0 {
                    out[k] = (f.as_ptr(), f.len());
                } else if nq == 2 && f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' {
                    out[k] = (f.as_ptr().add(1), f.len() - 2);
                } else {
                    return None;
                }
                k += 1;
                start = i + 1;
            }
        }
        if k == N { Some(out) } else { None }
    }
}

/// Reference scanner: calls `f(fields, row_start)` for every well-formed row of `base[..len]`.
pub unsafe fn scan_scalar<const N: usize>(base: *const u8, len: usize, mut f: impl FnMut(&[Field; N], *const u8)) {
    unsafe {
        let b = std::slice::from_raw_parts(base, len);
        let mut rs = 0;
        while rs < len {
            let re = b[rs..].iter().position(|&c| c == b'\n').map_or(len, |x| rs + x);
            if let Some(fields) = split_row::<N>(base.add(rs), re - rs) {
                f(&fields, base.add(rs));
            }
            rs = re + 1;
        }
    }
}

/// Vectorised scanner, same contract as `scan_scalar`. `base[..len]` must hold whole lines
/// (the last one may lack its `\n`).
#[inline(always)]
pub unsafe fn scan<M: Masker, const N: usize>(buf: &mut ScanBuf, base: *const u8, len: usize, mut f: impl FnMut(&[Field; N], *const u8)) {
    unsafe {
        if M::SCALAR || len >= (u32::MAX as usize) - 256 {
            return scan_scalar::<N>(base, len, f);
        }
        if len == 0 {
            return;
        }
        let mut pos = 0usize;
        let mut dlen = 0usize;
        let mut row_start = 0u32;
        let mut qrun = 0u32;
        let mut prev_q = 0u32;
        buf.nls.reserve(PIECE + 2);
        buf.nlq.reserve(PIECE + 2);
        while pos < len {
            let pend = (pos + PIECE).min(len);
            if buf.d.len() < dlen + PIECE + 128 + N {
                buf.d.resize(dlen + PIECE + 128 + N, 0);
            }
            let d = buf.d.as_mut_ptr();
            let nls = buf.nls.as_mut_ptr();
            let nlq = buf.nlq.as_mut_ptr();
            let mut nn = 0usize;
            while pos < pend {
                let (dm, nl, q) = if pos + 64 <= len {
                    M::masks(base.add(pos))
                } else {
                    let mut t = [0u8; 64];
                    std::ptr::copy_nonoverlapping(base.add(pos), t.as_mut_ptr(), len - pos);
                    M::masks(t.as_ptr())
                };
                let mut m = nl;
                while m != 0 {
                    let b = m.trailing_zeros();
                    *nls.add(nn) = pos as u32 + b;
                    *nlq.add(nn) = qrun + (q & ((1u64 << b) - 1)).count_ones();
                    nn += 1;
                    m &= m - 1;
                }
                qrun += q.count_ones();
                let cnt = dm.count_ones() as usize;
                let mut m = dm;
                let mut o = d.add(dlen);
                loop {
                    for k in 0..8 {
                        *o.add(k) = pos as u32 + m.trailing_zeros();
                        m &= m.wrapping_sub(1);
                    }
                    if m == 0 {
                        break;
                    }
                    o = o.add(8);
                }
                dlen += cnt;
                pos += 64;
            }
            if pos >= len && *base.add(len - 1) != b'\n' {
                // Last line of the file without a newline: a virtual one at `len`.
                *nls.add(nn) = len as u32;
                *nlq.add(nn) = qrun;
                nn += 1;
                *d.add(dlen) = len as u32;
                dlen += 1;
            }
            for k in 0..N {
                *d.add(dlen + k) = u32::MAX;
            }
            let mut i = 0usize;
            for j in 0..nn {
                let e = *nls.add(j);
                let qn = *nlq.add(j) - prev_q;
                prev_q = *nlq.add(j);
                if *d.add(i + N - 1) == e {
                    let mut fields: [Field; N] = [(base, 0); N];
                    let mut s = row_start;
                    let mut nq = 0u32;
                    for k in 0..N {
                        let en = *d.add(i + k);
                        let mut fp = base.add(s as usize);
                        let mut fl = (en - s) as usize;
                        // branch-free quote strip (reads stay inside the line or its delimiter)
                        let isq = ((fl >= 2) & (*fp == b'"') & (*base.add((en as usize).wrapping_sub(1)) == b'"')) as usize;
                        fp = fp.add(isq);
                        fl -= 2 * isq;
                        nq += 2 * isq as u32;
                        fields[k] = (fp, fl);
                        s = en + 1;
                    }
                    if nq == qn {
                        f(&fields, base.add(row_start as usize));
                    }
                    i += N;
                } else {
                    while *d.add(i) != e {
                        i += 1;
                    }
                    i += 1;
                }
                row_start = e + 1;
            }
            if i > 0 {
                std::ptr::copy(d.add(i), d, dlen - i);
                dlen -= i;
            }
        }
    }
}
