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
                let mut lo = _mm256_movemask_epi8(lo) as u32 as u64;
                // an optimisation barrier that costs no instruction
                std::arch::asm!("/* {0} */", inout(reg) lo, options(pure, nomem, nostack, preserves_flags));
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

/// One well-formed line of `N` fields: field `k < N` is the `ln[k]` bytes at `base + st[k]`
/// (quotes already stripped); the line starts at `base + row`. Entries from `N` on mean nothing.
#[repr(C, align(32))]
pub struct Row {
    pub st: [u32; 8],
    pub ln: [u32; 8],
    pub base: *const u8,
    pub row: u32,
}
impl Row {
    #[inline(always)]
    pub fn ptr(&self, k: usize) -> *const u8 {
        self.base.wrapping_add(self.st[k] as usize)
    }
    #[inline(always)]
    pub fn len(&self, k: usize) -> usize {
        self.ln[k] as usize
    }
    #[inline(always)]
    pub fn start(&self) -> *const u8 {
        self.base.wrapping_add(self.row as usize)
    }
    #[inline(always)]
    pub fn fields<const N: usize>(&self) -> [Field; N] {
        let mut f: [Field; N] = [(self.base, 0); N];
        for k in 0..N {
            f[k] = (self.ptr(k), self.len(k));
        }
        f
    }
    #[inline(always)]
    fn of<const N: usize>(base: *const u8, row: *const u8, f: &[Field; N]) -> Row {
        let mut r = Row { st: [0; 8], ln: [0; 8], base, row: (row as usize - base as usize) as u32 };
        for k in 0..N {
            r.st[k] = (f[k].0 as usize - base as usize) as u32;
            r.ln[k] = f[k].1 as u32;
        }
        r
    }
}

pub struct ScanBuf {
    /// Positions of the buffered delimiters (commas and newlines), relative to `base`, at
    /// index + 1 (index 0 is the delimiter before the first buffered one: the end of the
    /// previous line, or "-1" at the start of the data).
    d: Vec<u32>,
    /// One flag byte per buffered delimiter, at index + 1 (index 0 belongs to the delimiter
    /// before the first buffered one): `AQ` | `BQ` | `NL`.
    fl: Vec<u8>,
    /// Positions inside lines that hold a misplaced quote (ascending).
    bad: Vec<u32>,
}
impl ScanBuf {
    pub fn new() -> Self {
        ScanBuf { d: Vec::new(), fl: Vec::new(), bad: Vec::new() }
    }
}

/// The byte after the delimiter is a quote.
const AQ: u8 = 1;
/// The byte before the delimiter is a quote.
const BQ: u8 = 2;
/// The delimiter is a newline.
const NL: u8 = 4;

/// The bits of `x` selected by `m`, packed into the low bits.
#[inline(always)]
fn pext(x: u64, m: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    unsafe {
        std::arch::x86_64::_pext_u64(x, m)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let (mut m, mut r, mut k) = (m, 0u64, 0u32);
        while m != 0 {
            r |= ((x >> m.trailing_zeros()) & 1) << k;
            k += 1;
            m &= m - 1;
        }
        r
    }
}

/// Bit `j` of the low byte of `x` -> bit 0 of byte `j`.
#[inline(always)]
fn spread8(x: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    unsafe {
        std::arch::x86_64::_pdep_u64(x, 0x0101_0101_0101_0101)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let v = (x & 0xFF).wrapping_mul(0x0101_0101_0101_0101) & 0x8040_2010_0804_0201;
        ((v + 0x7F7F_7F7F_7F7F_7F7F) >> 7) & 0x0101_0101_0101_0101
    }
}

#[cold]
#[inline(never)]
fn push_bad(bad: &mut Vec<u32>, pos: usize, mut err: u64) {
    while err != 0 {
        bad.push(pos as u32 + err.trailing_zeros());
        err &= err - 1;
    }
}

const PIECE: usize = 8192;
const PF_ROWS: bool = false;

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
///
/// Per 64 bytes: the masks of delimiters (commas and newlines), newlines and quotes. The
/// delimiter positions are written out one after the other, each with a flag byte: is it a
/// newline, is the byte before it a quote, is the byte after it a quote. A line is taken on the
/// fast path when its first `N` delimiters are `N - 1` commas and a newline, every field has a
/// quote at both ends or at neither, and no quote of the line is misplaced. A misplaced quote
/// is one that is neither the first byte of a field nor directly before a delimiter, or that
/// is a whole field on its own; the masks find those 64 bytes at a time and their positions go
/// to a list that is almost always empty. Every other line goes through `split_row`.
#[inline(always)]
pub unsafe fn scan<M: Masker, const N: usize>(buf: &mut ScanBuf, base: *const u8, len: usize, mut f: impl FnMut(&[Field; N], *const u8)) {
    unsafe { scan_rows::<M, N>(buf, base, len, #[inline(always)] |r| f(&r.fields::<N>(), r.start())) }
}

/// `scan` handing out each line as a `Row`.
#[inline(always)]
pub unsafe fn scan_rows<M: Masker, const N: usize>(buf: &mut ScanBuf, base: *const u8, len: usize, mut f: impl FnMut(&Row)) {
    unsafe {
        if M::SCALAR || N > 7 || N == 0 || len >= (1usize << 31) {
            return scan_scalar::<N>(base, len, |fields, row| f(&Row::of(base, row, fields)));
        }
        if len == 0 {
            return;
        }
        // flag bytes of the delimiter before a line and of its N delimiters, as one word
        let (mut nl_mask, mut pair_mask) = (0u64, 0u64);
        for k in 0..N {
            nl_mask |= (NL as u64) << (8 * (k + 1));
            pair_mask |= 1u64 << (8 * k);
        }
        let nl_want = (NL as u64) << (8 * N);
        // A last line without a newline gets a virtual one at `len`.
        let total = len + (*base.add(len - 1) != b'\n') as usize;
        let mut pos = 0usize;
        let mut dlen = 0usize;
        let mut row_start = 0u32;
        // carries from the previous 64 bytes: its last byte was a delimiter (the data starts
        // as if after one), a quote, a quote not at a field start, a quote at a field start
        let (mut c_d, mut c_q, mut c_no, mut c_o) = (1u64, 0u64, 0u64, 0u64);
        buf.bad.clear();
        let mut bi = 0usize;
        let mut next_bad = u32::MAX;
        if buf.fl.is_empty() {
            buf.fl.resize(1, 0);
            buf.d.resize(1, 0);
        }
        buf.fl[0] = 0;
        buf.d[0] = u32::MAX;
        let mut r = Row { st: [0; 8], ln: [0; 8], base, row: 0 };
        while pos < total {
            let pend = (pos + PIECE).min(total);
            if buf.d.len() < dlen + PIECE + 137 {
                buf.d.resize(dlen + PIECE + 137, 0);
                buf.fl.resize(dlen + PIECE + 137, 0);
            }
            let d = buf.d.as_mut_ptr().add(1);
            let fl = buf.fl.as_mut_ptr().add(1);
            let mut nrows = 0usize;
            while pos < pend {
                let (dm, nl, q) = if pos + 64 <= len {
                    M::masks(base.add(pos))
                } else {
                    let mut t = [0u8; 64];
                    std::ptr::copy_nonoverlapping(base.add(pos), t.as_mut_ptr(), len - pos);
                    if total != len {
                        t[len - pos] = b'\n';
                    }
                    M::masks(t.as_ptr())
                };
                let fs = (dm << 1) | c_d;
                let open = q & fs;
                let nonopen = q & !fs;
                let err = (((nonopen << 1) | c_no) & !dm) | (((open << 1) | c_o) & dm);
                let bq = ((q << 1) | c_q) & dm;
                let aq = (q >> 1) & dm;
                // the previous delimiter was the byte before these 64: its `AQ` is known now
                *fl.add(dlen).sub(1) |= (c_d & q) as u8 & AQ;
                c_d = dm >> 63;
                c_q = q >> 63;
                c_no = nonopen >> 63;
                c_o = open >> 63;
                if err != 0 {
                    push_bad(&mut buf.bad, pos, err);
                    next_bad = *buf.bad.get_unchecked(bi);
                }
                nrows += nl.count_ones() as usize;
                let cnt = dm.count_ones() as usize;
                let (mut fa, mut fb, mut fn_) = (pext(aq, dm), pext(bq, dm), pext(nl, dm));
                let mut m = dm;
                let mut o = d.add(dlen);
                let mut fo = fl.add(dlen);
                loop {
                    for k in 0..8 {
                        *o.add(k) = pos as u32 + m.trailing_zeros();
                        m &= m.wrapping_sub(1);
                    }
                    (fo as *mut u64).write_unaligned(spread8(fa) * AQ as u64 | spread8(fb) * BQ as u64 | spread8(fn_) * NL as u64);
                    if m == 0 {
                        break;
                    }
                    o = o.add(8);
                    fo = fo.add(8);
                    fa >>= 8;
                    fb >>= 8;
                    fn_ >>= 8;
                }
                dlen += cnt;
                pos += 64;
            }
            let mut i = 0usize;
            // The rows take far longer than the masks: meanwhile the next piece is fetched,
            // two cache lines a row (the hardware prefetcher does not run ahead of a burst).
            let mut pf = base.wrapping_add(pos);
            let pf_end = base.wrapping_add((pos + PIECE).min(len));
            for _ in 0..nrows {
                if PF_ROWS {
                    let t = if pf < pf_end { pf } else { base };
                    crate::util::prefetch(t);
                    crate::util::prefetch(t.wrapping_add(64));
                    pf = pf.wrapping_add(128);
                }
                let w = (fl.add(i).sub(1) as *const u64).read_unaligned();
                let e = *d.add(i + N - 1);
                if ((w & nl_mask) == nl_want) & ((((w >> 9) ^ w) & pair_mask) == 0) & (e < next_bad) {
                    // field k: from one past the delimiter before it (and past its quote)
                    // to its own delimiter (less its quote)
                    let dp = d.add(i).sub(1);
                    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
                    {
                        use std::arch::x86_64::*;
                        let one = _mm256_set1_epi32(1);
                        let a = _mm256_and_si256(_mm256_cvtepu8_epi32(_mm_cvtsi64_si128(w as i64)), one);
                        let st = _mm256_add_epi32(_mm256_add_epi32(_mm256_loadu_si256(dp as *const __m256i), one), a);
                        let ln = _mm256_sub_epi32(_mm256_sub_epi32(_mm256_loadu_si256(dp.add(1) as *const __m256i), st), a);
                        _mm256_store_si256(r.st.as_mut_ptr() as *mut __m256i, st);
                        _mm256_store_si256(r.ln.as_mut_ptr() as *mut __m256i, ln);
                    }
                    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
                    {
                        let a = (w & 0x0101_0101_0101_0101).to_le_bytes();
                        for k in 0..8 {
                            let s = (*dp.add(k)).wrapping_add(1 + a[k] as u32);
                            r.st[k] = s;
                            r.ln[k] = (*dp.add(k + 1)).wrapping_sub(s).wrapping_sub(a[k] as u32);
                        }
                    }
                    r.row = row_start;
                    f(&r);
                    i += N;
                    row_start = e + 1;
                } else {
                    let mut j = i;
                    while *fl.add(j) & NL == 0 {
                        j += 1;
                    }
                    let e = *d.add(j);
                    if let Some(fields) = split_row::<N>(base.add(row_start as usize), (e - row_start) as usize) {
                        f(&Row::of(base, base.add(row_start as usize), &fields));
                    }
                    i = j + 1;
                    row_start = e + 1;
                    while bi < buf.bad.len() && *buf.bad.get_unchecked(bi) <= e {
                        bi += 1;
                    }
                    next_bad = if bi < buf.bad.len() { *buf.bad.get_unchecked(bi) } else { u32::MAX };
                }
            }
            if i > 0 {
                std::ptr::copy(d.add(i).sub(1), d.sub(1), dlen - i + 1);
                std::ptr::copy(fl.add(i).sub(1), fl.sub(1), dlen - i + 1);
                dlen -= i;
            }
            if bi > 0 && bi == buf.bad.len() {
                buf.bad.clear();
                bi = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<M: Masker, const N: usize>(v: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let b = v.as_ptr() as usize;
        let mut buf = ScanBuf::new();
        unsafe {
            scan::<M, N>(&mut buf, v.as_ptr(), v.len(), |f, row| {
                out.push(row as usize - b);
                for x in f {
                    out.push(x.0 as usize - b);
                    out.push(x.1);
                }
            });
        }
        out
    }

    fn check<M: Masker>(v: &[u8]) {
        assert!(run::<M, 1>(v) == run::<Scalar, 1>(v), "N=1 {:?}", String::from_utf8_lossy(v));
        assert!(run::<M, 2>(v) == run::<Scalar, 2>(v), "N=2 {:?}", String::from_utf8_lossy(v));
        assert!(run::<M, 4>(v) == run::<Scalar, 4>(v), "N=4 {:?}", String::from_utf8_lossy(v));
        assert!(run::<M, 7>(v) == run::<Scalar, 7>(v), "N=7 {:?}", String::from_utf8_lossy(v));
    }

    fn all(v: &[u8]) {
        check::<Swar>(v);
        #[cfg(target_arch = "aarch64")]
        check::<Neon>(v);
        #[cfg(target_arch = "x86_64")]
        {
            check::<Sse2>(v);
            if is_x86_feature_detected!("avx2") {
                check::<Avx2>(v);
            }
        }
    }

    #[test]
    fn random_lines() {
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alphas: [&[u8]; 4] = [b"abc,,,,\"\"\n\n\nxyz01 ", b"a,,,,,,,,\"\n", b"ab\"\",,,,,,\n", b"abcdefghijk,,,\"\n"];
        for round in 0..6000 {
            let alpha = alphas[round % 4];
            let n = match round % 5 {
                0 => (rnd() % 200) as usize,
                1 => 64 * (1 + (rnd() % 4) as usize),
                2 => 8192 * (1 + (rnd() % 2) as usize) + (rnd() % 3) as usize - 1,
                3 => (rnd() % 20_000) as usize,
                _ => 1 + (rnd() % 700) as usize,
            };
            let long = round % 7 == 3;
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                let r = rnd();
                let c = if long && (i / 3000) % 3 == 1 && r % 50 != 0 { b'q' } else { alpha[(r % alpha.len() as u64) as usize] };
                v.push(c);
            }
            if round % 3 == 0 && n > 0 {
                v[n - 1] = b'\n';
            }
            all(&v);
        }
    }

    #[test]
    fn plausible_rows() {
        // rows shaped like the real files, with a little dirt, so that the fast path is taken
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..300 {
            let mut v = Vec::new();
            let rows = 1 + rnd() % 400;
            for _ in 0..rows {
                let nf = if rnd() % 50 == 0 { 1 + rnd() % 9 } else { [7, 7, 4, 2][round % 4] };
                for k in 0..nf {
                    if k > 0 {
                        v.push(b',');
                    }
                    let l = rnd() % 12;
                    let quoted = rnd() % 5 == 0;
                    if quoted {
                        v.push(b'"');
                    }
                    for _ in 0..l {
                        v.push(b'a' + (rnd() % 26) as u8);
                    }
                    match rnd() % 400 {
                        0 => v.push(b'"'),
                        1 => v.extend_from_slice(b"\"\""),
                        2 => v.extend_from_slice(b"x\"y"),
                        _ => {}
                    }
                    if quoted && rnd() % 100 != 0 {
                        v.push(b'"');
                    }
                }
                v.push(b'\n');
            }
            if round % 2 == 0 {
                v.pop();
            }
            all(&v);
        }
    }
}
