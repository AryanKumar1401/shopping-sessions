//! Event rows by "row hopping": no stream of delimiters is built. Each line is walked from
//! left to right, speculating that it has the usual shape
//!
//!   id , id , ts , ingest_ts , type , product , price \n
//!
//! where each `id` is 1 to 9 bytes that are all in `0x30..=0x7F`, `ts` is a 13-digit F3, or
//! an F1 or F2 of the years 2000 to 2099, `ingest_ts` is such an F1, `type` is `view`, `cart`
//! or `purchase`, the product is not empty, and product, price and the newline fit in 32
//! bytes. Any of the first five fields may be wrapped in quotes; quotes in the last two are
//! checked with masks.
//!
//! Exactness: *every* byte of an accepted line is checked against that shape. An ID's bytes
//! are at least 0x30 and are followed by exactly `,` or `",`; a timestamp or a type is
//! compared byte for byte (ranges for digits) with a template that includes its opening
//! quote, its closing quote and the comma after it; the tail has exactly one comma before the
//! first newline and its quotes wrap whole fields. So an accepted line is well-formed, its
//! fields are the ones `split_row` finds, and it is evaluated with the same arithmetic as
//! `event_row`. Every other line (and every line within `GUARD` bytes of the end of the
//! readable bytes) takes `split_row` + `event_general`, the plain statement of the rules.

use crate::fast::*;
use crate::links::part_of;
use crate::scan::Field;
use crate::ts::*;
use crate::util::*;

/// Bytes that must be readable from a line's start for the speculative walk: it never reads
/// further than this, whatever the bytes are (2 x (1 + 16 + 2) for the IDs, 28 + 23 for the
/// timestamps, 11 for the type, 32 for the tail = 132, and the 32-byte loads behind each).
pub const GUARD: usize = 320;

/// What must follow an ID's bytes: `,` (index 0) or `",` (index 1), as a little-endian
/// 16-bit word and the mask of the bytes that count.
static TERM_WANT: [u16; 2] = [0x002C, 0x2C22];
static TERM_MASK: [u16; 2] = [0x00FF, 0xFFFF];
/// Nonzero for an ID length that is not 1..=9.
static LEN_BAD: [u8; 17] = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1];
/// The same for 0..=9 (an empty user_id is handled here: the event is not a good one).
static LEN_BAD0: [u8; 17] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1];

/// Nonzero unless the bytes at `t` are `,` (q = 0) or `",` (q = 1).
#[inline(always)]
unsafe fn term(t: *const u8, q: usize) -> u64 {
    unsafe {
        let w = (t as *const u16).read_unaligned();
        ((w ^ *TERM_WANT.get_unchecked(q)) & *TERM_MASK.get_unchecked(q)) as u64
    }
}

#[inline(always)]
unsafe fn rd64(p: *const u8) -> u64 {
    unsafe { (p as *const u64).read_unaligned() }
}

// ---------------------------------------------------------------- templates

/// Timestamp templates, a field from its first byte (the opening quote if it has one) to the
/// comma after it: byte `i` must satisfy `b - lo[i] <= rng[i]` (unsigned, wrapping).
/// Index: form * 2 + quoted, forms F1, F2, F3 (13 digits); plus `NL` for the same field as a
/// line's last one (a newline after it instead of the comma).
#[repr(C, align(32))]
struct TsPat {
    lo: [[u8; 32]; 12],
    rng: [[u8; 32]; 12],
}
const NL: usize = 6;
const F1: usize = 0;
const F2: usize = 1;
const F3: usize = 2;

const fn ts_pat() -> TsPat {
    const D: (u8, u8) = (b'0', b'9');
    const fn x(c: u8) -> (u8, u8) {
        (c, c)
    }
    // "20YY-MM-DDTHH:MM:SS": the digits in their loosest ranges (the calendar and the hour
    // are checked on the values)
    let civil: [(u8, u8); 19] =
        [x(b'2'), x(b'0'), D, D, x(b'-'), (b'0', b'1'), D, x(b'-'), (b'0', b'3'), D, x(b'T'), (b'0', b'2'), D, x(b':'), (b'0', b'5'), D, x(b':'), (b'0', b'5'), D];
    // F2's sign may be `+`, `,` or `-` here; the caller excludes the comma
    let off: [(u8, u8); 6] = [(b'+', b'-'), (b'0', b'2'), D, x(b':'), (b'0', b'5'), D];
    let mut t = TsPat { lo: [[0; 32]; 12], rng: [[0xFF; 32]; 12] };
    let mut tf = 0;
    while tf < 6 {
        let (form, last) = (tf % 3, tf / 3);
        let mut q = 0;
        while q < 2 {
            let k = last * NL + form * 2 + q;
            let mut body = [(0u8, 0u8); 32];
            let mut n = 0;
            if q == 1 {
                body[n] = x(b'"');
                n += 1;
            }
            if form == F3 {
                let mut i = 0;
                while i < 13 {
                    body[n] = D;
                    n += 1;
                    i += 1;
                }
            } else {
                let mut i = 0;
                while i < 19 {
                    body[n] = civil[i];
                    n += 1;
                    i += 1;
                }
                if form == F1 {
                    body[n] = x(b'Z');
                    n += 1;
                } else {
                    let mut i = 0;
                    while i < 6 {
                        body[n] = off[i];
                        n += 1;
                        i += 1;
                    }
                }
            }
            if q == 1 {
                body[n] = x(b'"');
                n += 1;
            }
            body[n] = x(if last == 1 { b'\n' } else { b',' });
            n += 1;
            let mut i = 0;
            while i < n {
                t.lo[k][i] = body[i].0;
                t.rng[k][i] = body[i].1 - body[i].0;
                i += 1;
            }
            q += 1;
        }
        tf += 1;
    }
    t
}
static TS_PAT: TsPat = ts_pat();

/// Event type templates, as above but exact bytes under a mask. Index: `(c & 6) + quoted`
/// for the first letter `c` (`p` -> 0, `c` -> 2, `v` -> 6; 4 matches nothing). `adv`: bytes
/// from the field's first byte to the next field's first byte.
#[repr(C, align(16))]
struct TypePat {
    pat: [[u8; 16]; 8],
    mask: [[u8; 16]; 8],
    adv: [u8; 8],
}
const fn type_pat() -> TypePat {
    let mut t = TypePat { pat: [[0xFF; 16]; 8], mask: [[0xFF; 16]; 8], adv: [0; 8] };
    let words: [(&[u8], usize); 3] = [(b"purchase", 0), (b"cart", 2), (b"view", 6)];
    let mut w = 0;
    while w < 3 {
        let (name, base) = words[w];
        let mut q = 0;
        while q < 2 {
            let k = base + q;
            t.pat[k] = [0; 16];
            t.mask[k] = [0; 16];
            let mut n = 0;
            if q == 1 {
                t.pat[k][n] = b'"';
                n += 1;
            }
            let mut i = 0;
            while i < name.len() {
                t.pat[k][n] = name[i];
                n += 1;
                i += 1;
            }
            if q == 1 {
                t.pat[k][n] = b'"';
                n += 1;
            }
            t.pat[k][n] = b',';
            n += 1;
            let mut i = 0;
            while i < n {
                t.mask[k][i] = 0xFF;
                i += 1;
            }
            t.adv[k] = n as u8;
            q += 1;
        }
        w += 1;
    }
    t
}
static TYPE_PAT: TypePat = type_pat();

/// What `k::tod` adds to the seconds of the day: the digits are multiplied as ASCII.
const TOD_BIAS: i64 = 48 * (11 * 3600 + 10 * 60 + 60 + 11);

// ---------------------------------------------------------------- vector pieces

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
mod k {
    use super::{TS_PAT, TYPE_PAT};
    use crate::util::Id;
    use std::arch::x86_64::*;

    pub type V16 = __m128i;
    pub type Ids = __m256i;

    /// Byte masks of the first `len` of 16 bytes for `len` 0..=9; zero for 10..=16.
    static LEN_MASK: [[u8; 16]; 17] = {
        let mut t = [[0u8; 16]; 17];
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

    /// The 16 bytes at `a`, and how many of them come before the first byte outside
    /// `0x30..=0x7F` (16 when there is none).
    #[inline(always)]
    pub unsafe fn id_scan(a: *const u8) -> (V16, usize) {
        unsafe {
            let v = _mm_loadu_si128(a as *const __m128i);
            // signed compare: bytes from 0x80 on are negative, so they stop the ID as well
            let m = _mm_movemask_epi8(_mm_cmpgt_epi8(_mm_set1_epi8(0x30), v)) as u32 | 0x1_0000;
            (v, m.trailing_zeros() as usize)
        }
    }

    /// How many of the bytes at `a` come before the first byte outside `0x30..=0x7F`:
    /// 0..=64, or 65 when there are more (64 bytes are read).
    #[inline(always)]
    pub unsafe fn id_len(a: *const u8) -> usize {
        unsafe {
            let lo = _mm256_set1_epi8(0x30);
            let m = _mm256_movemask_epi8(_mm256_cmpgt_epi8(lo, _mm256_loadu_si256(a as *const __m256i))) as u32;
            if m != 0 {
                return m.trailing_zeros() as usize;
            }
            let m = _mm256_movemask_epi8(_mm256_cmpgt_epi8(lo, _mm256_loadu_si256(a.add(32) as *const __m256i))) as u32;
            if m != 0 { 32 + m.trailing_zeros() as usize } else { 65 }
        }
    }

    /// `pack_id16` of two IDs of 0..=9 bytes, all in `0x30..=0x7F` (see `pack2_id16`);
    /// lengths up to 16 are allowed and give a meaningless value.
    #[inline(always)]
    pub unsafe fn pack2(v0: V16, l0: usize, v1: V16, l1: usize) -> Ids {
        unsafe {
            let m0 = _mm_load_si128(LEN_MASK.get_unchecked(l0).as_ptr() as *const __m128i);
            let m1 = _mm_load_si128(LEN_MASK.get_unchecked(l1).as_ptr() as *const __m128i);
            let d = _mm256_inserti128_si256(_mm256_castsi128_si256(_mm_and_si128(v0, m0)), _mm_and_si128(v1, m1), 1);
            let w = _mm256_maddubs_epi16(_mm256_set1_epi16(0x0180), d);
            let dd = _mm256_madd_epi16(w, _mm256_set1_epi32(0x0001_4000));
            let q = _mm256_or_si256(_mm256_mul_epu32(dd, _mm256_set1_epi64x(1 << 28)), _mm256_srli_epi64(dd, 32));
            _mm256_or_si256(_mm256_slli_epi64(q, 7), _mm256_srli_si256(d, 8))
        }
    }
    #[inline(always)]
    pub unsafe fn id0(ids: Ids) -> Id {
        unsafe { _mm_cvtsi128_si64(_mm256_castsi256_si128(ids)) as u64 }
    }
    #[inline(always)]
    pub unsafe fn id1(ids: Ids) -> Id {
        unsafe { _mm_cvtsi128_si64(_mm256_extracti128_si256(ids, 1)) as u64 }
    }

    /// True when the 32 bytes at `s` do not fit timestamp template `k`.
    #[inline(always)]
    pub unsafe fn ts_misfit(s: *const u8, k: usize) -> bool {
        unsafe {
            let v = _mm256_loadu_si256(s as *const __m256i);
            let x = _mm256_sub_epi8(v, _mm256_load_si256(TS_PAT.lo.get_unchecked(k).as_ptr() as *const __m256i));
            let over = _mm256_subs_epu8(x, _mm256_load_si256(TS_PAT.rng.get_unchecked(k).as_ptr() as *const __m256i));
            _mm256_testz_si256(over, over) == 0
        }
    }

    /// True when the 16 bytes at `s` do not fit type template `k`.
    #[inline(always)]
    pub unsafe fn type_misfit(s: *const u8, k: usize) -> bool {
        unsafe {
            let v = _mm_loadu_si128(s as *const __m128i);
            let x = _mm_xor_si128(v, _mm_load_si128(TYPE_PAT.pat.get_unchecked(k).as_ptr() as *const __m128i));
            _mm_testz_si128(x, _mm_load_si128(TYPE_PAT.mask.get_unchecked(k).as_ptr() as *const __m128i)) == 0
        }
    }

    /// `HH:MM:SS` at `a` (16 bytes readable), its bytes taken as they are:
    /// seconds of the day + `TOD_BIAS` when all six are digits.
    #[inline(always)]
    pub unsafe fn tod(a: *const u8) -> i64 {
        unsafe {
            let v = _mm_loadu_si128(a as *const __m128i);
            let w = _mm_maddubs_epi16(v, _mm_setr_epi8(10, 1, 0, 10, 1, 0, 10, 1, 0, 0, 0, 0, 0, 0, 0, 0));
            let d = _mm_madd_epi16(w, _mm_setr_epi16(3600, 60, 60, 1, 0, 0, 0, 0));
            let s = _mm_add_epi32(d, _mm_shuffle_epi32(d, 0x55));
            _mm_cvtsi128_si32(s) as u32 as i64
        }
    }

    /// 13 digits at `a` (16 bytes readable) -> their value.
    #[inline(always)]
    pub unsafe fn f3_13(a: *const u8) -> i64 {
        unsafe {
            let d = _mm_sub_epi8(_mm_loadu_si128(a as *const __m128i), _mm_set1_epi8(b'0' as i8));
            // three leading zeros, then the 13 digits: a 16-digit number
            let s = _mm_slli_si128(d, 3);
            let w = _mm_maddubs_epi16(s, _mm_set1_epi16(0x010A));
            let x = _mm_madd_epi16(w, _mm_set1_epi32(0x0001_0064));
            let y = _mm_packus_epi32(x, x);
            let z = _mm_madd_epi16(y, _mm_set1_epi32(0x0001_2710));
            let hi = _mm_cvtsi128_si32(z) as u32 as u64;
            let lo = _mm_extract_epi32(z, 1) as u32 as u64;
            (hi * 100_000_000 + lo) as i64
        }
    }

    /// Masks of `,`, `\n` and `"` in the 32 bytes at `s`.
    #[inline(always)]
    pub unsafe fn tail_masks(s: *const u8) -> (u32, u32, u32) {
        unsafe {
            let v = _mm256_loadu_si256(s as *const __m256i);
            let c = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b',' as i8))) as u32;
            let n = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8))) as u32;
            let q = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8))) as u32;
            (c, n, q)
        }
    }

    #[inline(always)]
    pub fn date_slot(key: u64) -> usize {
        unsafe { _pext_u64(key, 0x0F03_0000_0000_0000) as usize }
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
mod k {
    use super::{TS_PAT, TYPE_PAT};
    use crate::util::{Id, pack_id16};

    pub type V16 = *const u8;
    pub type Ids = (Id, Id);

    #[inline(always)]
    pub unsafe fn id_scan(a: *const u8) -> (V16, usize) {
        unsafe {
            let mut l = 0;
            while l < 16 && (0x30..=0x7F).contains(&*a.add(l)) {
                l += 1;
            }
            (a, l)
        }
    }

    #[inline(always)]
    pub unsafe fn id_len(a: *const u8) -> usize {
        unsafe {
            let mut l = 0;
            while l < 65 && (0x30..=0x7F).contains(&*a.add(l)) {
                l += 1;
            }
            l
        }
    }

    #[inline(always)]
    pub unsafe fn pack2(v0: V16, l0: usize, v1: V16, l1: usize) -> Ids {
        unsafe { (if l0 <= 9 { pack_id16(v0, l0) } else { 0 }, if l1 <= 9 { pack_id16(v1, l1) } else { 0 }) }
    }
    #[inline(always)]
    pub unsafe fn id0(ids: Ids) -> Id {
        ids.0
    }
    #[inline(always)]
    pub unsafe fn id1(ids: Ids) -> Id {
        ids.1
    }

    #[inline(always)]
    pub unsafe fn ts_misfit(s: *const u8, k: usize) -> bool {
        unsafe {
            let mut bad = false;
            for i in 0..32 {
                bad |= (*s.add(i)).wrapping_sub(TS_PAT.lo[k][i]) > TS_PAT.rng[k][i];
            }
            bad
        }
    }

    #[inline(always)]
    pub unsafe fn type_misfit(s: *const u8, k: usize) -> bool {
        unsafe {
            let mut bad = false;
            for i in 0..16 {
                bad |= (*s.add(i) ^ TYPE_PAT.pat[k][i]) & TYPE_PAT.mask[k][i] != 0;
            }
            bad
        }
    }

    #[inline(always)]
    pub unsafe fn tod(a: *const u8) -> i64 {
        unsafe {
            let b = |i: usize| *a.add(i) as i64;
            (b(0) * 10 + b(1)) * 3600 + (b(3) * 10 + b(4)) * 60 + b(6) * 10 + b(7)
        }
    }

    #[inline(always)]
    pub unsafe fn f3_13(a: *const u8) -> i64 {
        unsafe {
            let mut v = 0i64;
            for i in 0..13 {
                v = v * 10 + (*a.add(i) - b'0') as i64;
            }
            v
        }
    }

    #[inline(always)]
    pub unsafe fn tail_masks(s: *const u8) -> (u32, u32, u32) {
        unsafe {
            let (mut c, mut n, mut q) = (0u32, 0u32, 0u32);
            for i in 0..32 {
                let b = *s.add(i);
                c |= ((b == b',') as u32) << i;
                n |= ((b == b'\n') as u32) << i;
                q |= ((b == b'"') as u32) << i;
            }
            (c, n, q)
        }
    }

    #[inline(always)]
    pub fn date_slot(key: u64) -> usize {
        (((key >> 48) & 3) | ((key >> 54) & 0x3C)) as usize
    }
}

// ---------------------------------------------------------------- dates

/// Memo of the dates of the years 20YY: key = the eight bytes `YY-MM-DD`, value = epoch
/// seconds of that midnight less `TOD_BIAS`. Slot: the low bits of the two day digits.
pub struct HopDates {
    k: [u64; 64],
    v: [i64; 64],
}
impl HopDates {
    pub fn new() -> Self {
        HopDates { k: [0; 64], v: [0; 64] }
    }
}

/// The date `20YY-MM-DDT` at `a` is not in its slot: computes and stores it. NONE when it is
/// not a date (nothing is stored).
#[cold]
#[inline(never)]
unsafe fn date_miss(a: *const u8, dates: &mut HopDates) -> i64 {
    unsafe {
        let slot = k::date_slot(rd64(a.add(2)));
        let v = date_secs(a);
        if v == NONE || *a != b'2' || *a.add(1) != b'0' {
            return NONE;
        }
        dates.k[slot] = rd64(a.add(2));
        dates.v[slot] = v - TOD_BIAS;
        v - TOD_BIAS
    }
}

/// `20YY-MM-DDTHH:MM:SS` at `$a`, already compared with its template (so every digit is one,
/// and `20`, `-`, `T`, `:` are in place) -> epoch seconds. Leaves the row function with
/// `DATE_MISS` when the date is not in the memo, with `BAIL` for an impossible hour.
macro_rules! civil {
    ($a:expr, $dates:expr, $hot:expr) => {{
        let a = $a;
        let key = rd64(a.add(2));
        let slot = k::date_slot(key);
        if *$dates.k.get_unchecked(slot) != key {
            $hot.miss = a;
            return DATE_MISS;
        }
        let t = k::tod(a.add(11));
        if t >= TOD_BIAS + 86_400 {
            return BAIL;
        }
        *$dates.v.get_unchecked(slot) + t
    }};
}

// ---------------------------------------------------------------- the row

/// The purchase's `category * PRICE_MUL + price`, or 0 when the price is bad. Product: the
/// tail's first field (ends at bit `mcb`), price: its second (ends at `nl`); `open`: the
/// tail's opening quotes.
unsafe fn purchase_pc(ev: &EvRow, s5: *const u8, mcb: u32, nl: usize, open: u32) -> u32 {
    unsafe {
        let c = mcb.trailing_zeros() as usize;
        let qp = (open & 1) as usize;
        let qr = ((open >> (c + 1)) & 1) as usize;
        let (pp, pl) = (s5.add(c + 1 + qr), nl - c - 1 - 2 * qr);
        // (up to 8 digits at once: the bytes after the price are readable, see `GUARD`)
        let price = if pl - 1 < 8 {
            let w = (rd64(pp) << (8 * (8 - pl))) | ((0x3030_3030_3030_3030u64 >> 8) >> (8 * (pl - 1)));
            let ok = ((w & 0xF0F0_F0F0_F0F0_F0F0) == 0x3030_3030_3030_3030) & ((w.wrapping_add(0x0606_0606_0606_0606) & 0xF0F0_F0F0_F0F0_F0F0) == 0x3030_3030_3030_3030);
            let x = (w & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
            let x = (x & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
            let v = (x & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32;
            if ok && (1..=10_000_000).contains(&v) { v as u32 } else { 0 }
        } else {
            parse_price(pp, pl)
        };
        if price == 0 {
            return 0;
        }
        let mut cat = ev.ctx.products.get(pack_id(s5.add(qp), c - 2 * qp, ev.lim));
        if cat == u32::MAX {
            cat = ev.ctx.unknown_cat;
        }
        cat * PRICE_MUL + price
    }
}

/// What the row loop keeps in registers or on its stack instead of reading through `Ctx`,
/// and what a row hands to the loop when it leaves with a code.
struct Hot {
    ws: i64,
    /// `we - ws`
    span: u64,
    cbase: i64,
    ibase_s: i64,
    pshift: u32,
    dshift: u32,
    pbase: usize,
    /// The writers' raw parts (`Writers::raw`), or nulls when `RAW` is false.
    ph: *mut (*mut PRec, *mut PRec),
    pb: *mut PRec,
    dh: *mut (*mut DRec, *mut DRec),
    db: *mut DRec,
    /// All `PRec` indexes and all `DRec::ing` seen, or-ed: `escape` when one does not fit.
    /// (An `ing` does not fit exactly when a bit from 32 up is set. For the indexes the test
    /// `>= NOPREC` is exact from 2^32 up and never misses NOPREC itself; it could only be
    /// true without cause if indexes reached 2^31, which merely sends the run to the
    /// general path.)
    max_idx: usize,
    max_d: u64,
    /// `DATE_MISS`: the date that is not in the memo.
    miss: *const u8,
    /// `REFILL`: the partitions the row wrote to.
    ps: usize,
    ds: usize,
    /// `PURCHASE`: the row so far.
    pur: Purchase,
}
#[derive(Clone, Copy)]
struct Purchase {
    eid: Id,
    uid: Id,
    ts: i64,
    dd: u64,
    s5: *const u8,
    mcb: u32,
    open: u32,
    nl: usize,
}

/// What `hop_row` returns instead of the next line's start (which is never below `CODES`):
/// the line is not of the usual shape and nothing was done;
const BAIL: usize = 0;
/// nothing was done, `Hot::miss` is a date for the memo, then the line can be tried again;
const DATE_MISS: usize = 1;
/// the line is a purchase inside the window, to be finished by `purchase_finish`.
const PURCHASE: usize = 2;
/// nothing was done, the line has an ID of 10 bytes or more (or is not of the usual shape):
/// `long_row`.
const LONGROW: usize = 3;
const CODES: isize = 4;
/// Added to the next line's start: a block the row wrote to is full (`Writers::refill_full`
/// for `Hot::ps` and `Hot::ds`).
const REFILL: usize = 1 << 63;

/// `Writers::push_ptr` for the aligned write-combining form of 16-byte records (`heads`,
/// `buf` from `Writers::raw`), without the call that gets a new block: returns the record's
/// place, and `REFILL` in `full` when the block is full now.
#[inline(always)]
unsafe fn push16<T: Copy>(heads: *mut (*mut T, *mut T), buf: *mut T, s: usize, v: T, full: &mut usize) -> usize {
    unsafe {
        let so = s << 4;
        let h = (heads as *mut u8).add(so) as *mut (*mut T, *mut T);
        let p = (*h).0 as usize;
        // which of the line's four records
        let n = p & 0x30;
        let line = (buf as *mut u8).add(so * 4);
        (line.add(n) as *mut T).write(v);
        (*h).0 = (p + 16) as *mut T;
        if n == 0x30 {
            wc_copy((p - 0x30) as *mut u8, line, 64);
            if (*h).1 as usize == p + 16 {
                *full = REFILL;
            }
        }
        p
    }
}

/// The fields of an event line after the two IDs, from `$s2` (the first byte of `ts`):
/// evaluates to (ts in ms, ingest_ts in s, the type's template index, the tail's start, its
/// comma mask, its opening quotes, the newline's offset, a word that is nonzero when the tail
/// is not of the usual shape), or leaves the row function with `BAIL` or `DATE_MISS`.
macro_rules! row_rest {
    ($s2:expr, $dates:expr, $hot:expr) => {{
        // ts: F1 / F2 have a '-' after the year, a 13-digit F3 a digit
        let s2: *const u8 = $s2;
        let q2 = (*s2 == b'"') as usize;
        let a2 = s2.add(q2);
        let (ts, s3);
        if *a2.add(4) == b'-' {
            if !k::ts_misfit(s2, F1 * 2 + q2) {
                ts = civil!(a2, $dates, $hot) * 1000;
                s3 = a2.add(21 + q2);
            } else {
                // "+HH:MM" / "-HH:MM" (the template lets a comma pass for the sign)
                let z = *a2.add(19);
                if k::ts_misfit(s2, F2 * 2 + q2) | (z == b',') {
                    return BAIL;
                }
                let t = civil!(a2, $dates, $hot);
                let o = rd64(a2.add(19));
                let v = ((o >> 8) ^ 0x30_303A_3030) & 0xFF_FFFF_FFFF;
                let x = v.wrapping_mul(2561);
                let hh = (x >> 8) & 0xFF;
                let mm = (x >> 32) & 0xFF;
                if hh > 23 {
                    return BAIL;
                }
                let off = ((hh * 60 + mm) * 60_000) as i64;
                ts = if z == b'+' { t * 1000 - off } else { t * 1000 + off };
                s3 = a2.add(26 + q2);
            }
        } else {
            if k::ts_misfit(s2, F3 * 2 + q2) {
                return BAIL;
            }
            ts = k::f3_13(a2);
            s3 = a2.add(14 + q2);
        }
        // ingest_ts: F1
        let q3 = (*s3 == b'"') as usize;
        let a3 = s3.add(q3);
        if k::ts_misfit(s3, F1 * 2 + q3) {
            return BAIL;
        }
        let ing_s = civil!(a3, $dates, $hot);
        // event_type
        let s4 = a3.add(21 + q3);
        let q4 = (*s4 == b'"') as usize;
        let kt = (*s4.add(q4) as usize & 6) + q4;
        if k::type_misfit(s4, kt) {
            return BAIL;
        }
        // product_id , price_cents \n : one comma before the first newline; every quote opens
        // or closes a field, a field that opens closes, and no quote does both
        let s5 = s4.add(*TYPE_PAT.adv.get_unchecked(kt) as usize);
        let (mc, mn, mq) = k::tail_masks(s5);
        let nl = mn.trailing_zeros() as usize;
        let below = mn ^ mn.wrapping_sub(1);
        let mcb = mc & below;
        let d = mcb | (mn & mn.wrapping_neg());
        let mqb = mq & below;
        let open = mqb & ((d << 1) | 1);
        let close = mqb & (d >> 1);
        let opened = (!d).wrapping_add(open) & d;
        // no newline; no comma or several (`d` is then just the newline, or holds a comma
        // above the lowest one); quotes; an empty product (`,` first, or `""` first)
        let tb = (nl as u32 >> 5) | (mcb.wrapping_sub(1) & d) | (opened ^ (close << 1)) | (open & close) | (mqb ^ (open | close)) | ((mcb | ((mcb >> 2) & open)) & 1);
        (ts, ing_s, kt, s5, mcb, open, nl, tb)
    }};
}

/// One line at `p` (with `GUARD` readable bytes): handles it and returns the start of the next
/// line (plus `REFILL`), or one of the codes above. No calls in here: the loop around it
/// keeps its constants in registers.
#[inline(always)]
unsafe fn hop_row<const RAW: bool>(hot: &mut Hot, p: *const u8, dates: &HopDates, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>) -> usize {
    unsafe {
        // event_id
        let q0 = (*p == b'"') as usize;
        let a0 = p.add(q0);
        let (v0, l0) = k::id_scan(a0);
        let t0 = a0.add(l0);
        let mut bad = term(t0, q0);
        // user_id
        let s1 = t0.add(q0 + 1);
        let q1 = (*s1 == b'"') as usize;
        let a1 = s1.add(q1);
        let (v1, l1) = k::id_scan(a1);
        // an ID of 10 bytes or more: `long_row`
        if (l0 > 9) | (l1 > 9) {
            return LONGROW;
        }
        let t1 = a1.add(l1);
        bad |= term(t1, q1);
        // the event_id not empty (an empty user_id packs to 0)
        bad |= (*LEN_BAD.get_unchecked(l0) | *LEN_BAD0.get_unchecked(l1)) as u64;
        let ids = k::pack2(v0, l0, v1, l1);
        let (ts, ing_s, kt, s5, mcb, open, nl, tb) = row_rest!(t1.add(q1 + 1), dates, hot);
        bad |= tb as u64;
        if bad != 0 {
            return BAIL;
        }
        let mut next = s5 as usize + nl + 1;
        let dd = (ing_s - hot.ibase_s) as u64;
        let eid = k::id0(ids);
        let mut idx = NOPREC as usize;
        // (`ts` is not negative and the window is ordered: one compare for both ends)
        if ((ts.wrapping_sub(hot.ws) as u64) < hot.span) && (ing_s * 1000).wrapping_sub(ts) <= DAY {
            let uid = k::id1(ids);
            if kt < 2 {
                hot.pur = Purchase { eid, uid, ts, dd, s5, mcb, open, nl };
                return PURCHASE;
            }
            if uid != 0 {
                let ps = ((id_hash(uid) >> 1) >> hot.pshift) as usize;
                let rec = PRec { k: uid, ts: (ts - hot.cbase) as u32, pc: 0 };
                let prec = if RAW {
                    let mut full = 0;
                    let prec = push16(hot.ph, hot.pb, ps, rec, &mut full);
                    if full != 0 {
                        hot.ps = ps;
                        next |= REFILL;
                    }
                    prec
                } else {
                    pw.push_ptr(ps, rec) as usize
                };
                idx = prec.wrapping_sub(hot.pbase) / std::mem::size_of::<PRec>();
                hot.max_idx |= idx;
            }
        }
        hot.max_d |= dd;
        let ds = ((id_hash(eid) >> 1) >> hot.dshift) as usize;
        let rec = DRec { eid, ing: dd as u32, prec: idx as u32 };
        if RAW {
            let mut full = 0;
            push16(hot.dh, hot.db, ds, rec, &mut full);
            if full != 0 {
                hot.ds = ds;
                next |= REFILL;
            }
        } else {
            dw.push(ds, rec);
        }
        next
    }
}

/// `hop_row` for a line whose event_id or user_id is long: the same walk with ID fields of up
/// to 64 bytes (all in `0x30..=0x7F`), out of line. A user_id of 10 bytes or more is
/// interned, as `pack_id` does; an event_id of 10 bytes or more gets an `LRec`
/// (`push_dedup`). Returns the next line's start, `BAIL` or `DATE_MISS`.
#[inline(never)]
unsafe fn long_row(ev: &EvRow, hot: &mut Hot, p: *const u8, dates: &HopDates, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, lw: &mut Writers<LRec>, escape: &mut bool) -> usize {
    unsafe {
        let q0 = (*p == b'"') as usize;
        let a0 = p.add(q0);
        let l0 = k::id_len(a0);
        if l0.wrapping_sub(1) > 63 {
            return BAIL;
        }
        let t0 = a0.add(l0);
        let mut bad = term(t0, q0);
        let s1 = t0.add(q0 + 1);
        let q1 = (*s1 == b'"') as usize;
        let a1 = s1.add(q1);
        let l1 = k::id_len(a1);
        if l1 > 64 {
            return BAIL;
        }
        let t1 = a1.add(l1);
        bad |= term(t1, q1);
        let (ts, ing_s, kt, s5, mcb, open, nl, tb) = row_rest!(t1.add(q1 + 1), dates, hot);
        if bad | tb as u64 != 0 {
            return BAIL;
        }
        let dd = (ing_s - hot.ibase_s) as u64;
        hot.max_d |= dd;
        let mut prec = NOPREC;
        if ((ts.wrapping_sub(hot.ws) as u64) < hot.span) && (ing_s * 1000).wrapping_sub(ts) <= DAY && l1 != 0 {
            let pc = if kt < 2 { purchase_pc(ev, s5, mcb, nl, open) } else { 0 };
            if kt >= 2 || pc != 0 {
                let uid = if l1 <= 9 { pack_id16(a1, l1) } else { intern_long(std::slice::from_raw_parts(a1, l1)) };
                prec = push_prec(ev.ctx, pw, escape, uid, (ts - hot.cbase) as u32, pc);
            }
        }
        push_dedup(ev.ctx, dw, lw, a0, l0, ev.lim, ev.fix, dd as u32, prec);
        s5 as usize + nl + 1
    }
}

/// The rest of a row that left `hop_row` with `PURCHASE`; returns the next line's start.
#[cold]
#[inline(never)]
unsafe fn purchase_finish(ev: &EvRow, hot: &mut Hot, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>) -> *const u8 {
    unsafe {
        let Purchase { eid, uid, ts, dd, s5, mcb, open, nl } = hot.pur;
        let pc = purchase_pc(ev, s5, mcb, nl, open);
        let mut idx = NOPREC as usize;
        if pc != 0 && uid != 0 {
            let prec = pw.push_ptr(((id_hash(uid) >> 1) >> hot.pshift) as usize, PRec { k: uid, ts: (ts - hot.cbase) as u32, pc });
            idx = (prec as usize).wrapping_sub(hot.pbase) / std::mem::size_of::<PRec>();
            hot.max_idx |= idx;
        }
        hot.max_d |= dd;
        dw.push(((id_hash(eid) >> 1) >> hot.dshift) as usize, DRec { eid, ing: dd as u32, prec: idx as u32 });
        s5.add(nl + 1)
    }
}

/// The line at `p` (ending before `end`) by the general rules; returns the next line's start.
/// One pass that finds the newline and splits as `split_row` does: a field has no quote, or
/// exactly two, as its first and last byte.
#[inline(never)]
unsafe fn slow_row_bytes(ev: &EvRow, p: *const u8, end: *const u8, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, lw: &mut Writers<LRec>, escape: &mut bool) -> *const u8 {
    unsafe {
        let mut f: [Field; 7] = [(p, 0); 7];
        let (mut k, mut well, mut st, mut nq, mut i) = (0usize, true, p, 0usize, p);
        loop {
            let c = if i == end { b'\n' } else { *i };
            if c == b',' || c == b'\n' {
                let len = i as usize - st as usize;
                if k < 7 {
                    if nq == 0 {
                        f[k] = (st, len);
                    } else if nq == 2 && len >= 2 && *st == b'"' && *i.sub(1) == b'"' {
                        f[k] = (st.add(1), len - 2);
                    } else {
                        well = false;
                    }
                }
                k += 1;
                if c == b'\n' {
                    break;
                }
                st = i.add(1);
                nq = 0;
            } else if c == b'"' {
                nq += 1;
            }
            i = i.add(1);
        }
        if well && k == 7 {
            event_general(ev.ctx, &f, ev.lim, ev.fix, cache, pw, dw, lw, escape);
        }
        i.add(1)
    }
}

/// `slow_row_bytes` with the delimiters and quotes found 32 bytes at a time; a line that
/// comes within 32 bytes of the end of the readable bytes is handed to `slow_row_bytes`.
#[cold]
#[inline(never)]
unsafe fn slow_row(ev: &EvRow, p: *const u8, end: *const u8, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, lw: &mut Writers<LRec>, escape: &mut bool) -> *const u8 {
    unsafe {
        let mut f: [Field; 7] = [(p, 0); 7];
        let (mut k, mut well, mut st, mut nq, mut b) = (0usize, true, p, 0u32, p);
        loop {
            if (ev.lim as usize - b as usize) < 32 {
                return slow_row_bytes(ev, p, end, cache, pw, dw, lw, escape);
            }
            let (mc, mn, mq) = k::tail_masks(b);
            let mut dm = mc | mn;
            // quotes of the block not yet counted: those from bit `from` on
            let mut from = 0u32;
            while dm != 0 {
                let i = dm.trailing_zeros();
                nq += (((mq as u64) >> from) << from & ((1u64 << i) - 1)).count_ones();
                let e = b.add(i as usize);
                let len = e as usize - st as usize;
                if k < 7 {
                    if nq == 0 {
                        f[k] = (st, len);
                    } else if nq == 2 && len >= 2 && *st == b'"' && *e.sub(1) == b'"' {
                        f[k] = (st.add(1), len - 2);
                    } else {
                        well = false;
                    }
                }
                k += 1;
                if (mn >> i) & 1 != 0 {
                    if well && k == 7 {
                        event_general(ev.ctx, &f, ev.lim, ev.fix, cache, pw, dw, lw, escape);
                    }
                    return e.add(1);
                }
                st = e.add(1);
                nq = 0;
                from = i + 1;
                dm &= dm - 1;
            }
            nq += ((mq as u64) >> from).count_ones();
            b = b.add(32);
        }
    }
}

#[inline(always)]
unsafe fn hop_loop<const RAW: bool>(ev: &EvRow, base: *const u8, len: usize, dates: &mut HopDates, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, lw: &mut Writers<LRec>, escape: &mut bool) -> u64 {
    unsafe {
        let ctx = ev.ctx;
        let (ph, pb) = pw.raw().unwrap_or((std::ptr::null_mut(), std::ptr::null_mut()));
        let (dh, db) = dw.raw().unwrap_or((std::ptr::null_mut(), std::ptr::null_mut()));
        let mut hot = Hot {
            ws: ctx.ws,
            span: (ctx.we - ctx.ws) as u64,
            cbase: ctx.cbase,
            ibase_s: ev.ibase_s,
            pshift: 63 - ctx.pbits,
            dshift: 63 - ctx.dbits,
            pbase: ctx.pregion.base,
            ph,
            pb,
            dh,
            db,
            max_idx: 0,
            max_d: 0,
            miss: base,
            ps: 0,
            ds: 0,
            pur: Purchase { eid: 0, uid: 0, ts: 0, dd: 0, s5: base, mcb: 0, open: 0, nl: 0 },
        };
        let end = base.add(len);
        // lines that start before `fast_end` have `GUARD` readable bytes
        let room = ev.lim as usize - base as usize;
        let fast_end = if room >= GUARD { base.add((room - GUARD + 1).min(len)) } else { base };
        let mut p = base;
        let mut slow = 0u64;
        let (mut retry_p, mut retry_n) = (std::ptr::null::<u8>(), 0u32);
        loop {
            let mut code = BAIL;
            while p < fast_end {
                let r = hop_row::<RAW>(&mut hot, p, dates, pw, dw);
                if (r as isize) < CODES {
                    code = r;
                    break;
                }
                p = r as *const u8;
            }
            if code & REFILL != 0 {
                pw.refill_full(hot.ps);
                dw.refill_full(hot.ds);
                p = (code & !REFILL) as *const u8;
                continue;
            }
            if code == PURCHASE {
                p = purchase_finish(ev, &mut hot, pw, dw);
                continue;
            }
            if code == LONGROW {
                let r = long_row(ev, &mut hot, p, dates, pw, dw, lw, escape);
                if (r as isize) >= CODES {
                    p = r as *const u8;
                    continue;
                }
                code = r;
            }
            if code == DATE_MISS {
                // A line misses at most twice (its two dates) before it passes; one whose
                // dates share a memo slot would evict each other for ever, so it gets the
                // general path after that.
                if retry_p != p {
                    retry_p = p;
                    retry_n = 0;
                }
                retry_n += 1;
                if retry_n <= 2 && date_miss(hot.miss, dates) != NONE {
                    continue;
                }
            }
            if p >= end {
                break;
            }
            slow += 1;
            p = slow_row(ev, p, end, cache, pw, dw, lw, escape);
        }
        // (a `PRec` index of NOPREC or more: as in `emit_event`)
        *escape |= (hot.max_idx >= NOPREC as usize) | (hot.max_d > u32::MAX as u64);
        slow
    }
}

/// All event lines of `base[..len]` (whole lines; the last may lack its `\n` only when it
/// ends at `ev.lim`). Returns how many lines took the general path.
#[inline(always)]
pub unsafe fn hop_events(ev: &EvRow, base: *const u8, len: usize, dates: &mut HopDates, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, lw: &mut Writers<LRec>, escape: &mut bool) -> u64 {
    unsafe {
        if pw.raw().is_some() && dw.raw().is_some() && std::mem::size_of::<PRec>() == 16 && std::mem::size_of::<DRec>() == 16 {
            hop_loop::<true>(ev, base, len, dates, cache, pw, dw, lw, escape)
        } else {
            hop_loop::<false>(ev, base, len, dates, cache, pw, dw, lw, escape)
        }
    }
}

// ---------------------------------------------------------------- click rows

/// The same walk for click lines, `click_id , user_id , campaign_id , ts \n`: three IDs of the
/// usual shape (the click_id not empty) and a 13-digit F3, F1 or F2 `ts` that ends the line.
/// Every byte of an accepted line is checked as for events; every other line (and every line
/// within `GUARD` bytes of the end) takes `slow_click`, the general rules.
struct ClickHot {
    cbase: i64,
    /// `we - cbase`
    span: u64,
    fix: usize,
    miss: *const u8,
}

#[inline(always)]
unsafe fn click_row(ctx: &Ctx, hot: &mut ClickHot, p: *const u8, dates: &HopDates, cw: &mut Writers<CRec>) -> usize {
    unsafe {
        // click_id
        let q0 = (*p == b'"') as usize;
        let a0 = p.add(q0);
        let (_, l0) = k::id_scan(a0);
        let t0 = a0.add(l0);
        let mut bad = term(t0, q0);
        // user_id
        let s1 = t0.add(q0 + 1);
        let q1 = (*s1 == b'"') as usize;
        let a1 = s1.add(q1);
        let (v1, l1) = k::id_scan(a1);
        let t1 = a1.add(l1);
        bad |= term(t1, q1);
        // campaign_id
        let s2 = t1.add(q1 + 1);
        let q2 = (*s2 == b'"') as usize;
        let a2 = s2.add(q2);
        let (v2, l2) = k::id_scan(a2);
        let t2 = a2.add(l2);
        bad |= term(t2, q2);
        bad |= (*LEN_BAD.get_unchecked(l0) | *LEN_BAD0.get_unchecked(l1) | *LEN_BAD0.get_unchecked(l2)) as u64;
        let ids = k::pack2(v1, l1, v2, l2);
        // ts, then the newline
        let s3 = t2.add(q2 + 1);
        let q3 = (*s3 == b'"') as usize;
        let a3 = s3.add(q3);
        let (ts, next);
        if *a3.add(4) == b'-' {
            if !k::ts_misfit(s3, NL + F1 * 2 + q3) {
                ts = civil!(a3, dates, hot) * 1000;
                next = a3.add(21 + q3);
            } else {
                let z = *a3.add(19);
                if k::ts_misfit(s3, NL + F2 * 2 + q3) | (z == b',') {
                    return BAIL;
                }
                let t = civil!(a3, dates, hot);
                let o = rd64(a3.add(19));
                let v = ((o >> 8) ^ 0x30_303A_3030) & 0xFF_FFFF_FFFF;
                let x = v.wrapping_mul(2561);
                let hh = (x >> 8) & 0xFF;
                let mm = (x >> 32) & 0xFF;
                if hh > 23 {
                    return BAIL;
                }
                let off = ((hh * 60 + mm) * 60_000) as i64;
                ts = if z == b'+' { t * 1000 - off } else { t * 1000 + off };
                next = a3.add(26 + q3);
            }
        } else {
            if k::ts_misfit(s3, NL + F3 * 2 + q3) {
                return BAIL;
            }
            ts = k::f3_13(a3);
            next = a3.add(14 + q3);
        }
        if bad != 0 {
            return BAIL;
        }
        let (uid, cid) = (k::id0(ids), k::id1(ids));
        // in the click window, with a user and a known campaign (an empty ID packs to 0)
        if ((ts.wrapping_sub(hot.cbase) as u64) < hot.span) & (uid != 0) & (cid != 0) {
            let camp = ctx.campaigns.get(cid);
            if camp != u32::MAX {
                cw.push(part_of(id_hash(uid), ctx.pbits), CRec { k: uid, ts: (ts - hot.cbase) as u32, camp, ptr: (p as usize).wrapping_add(hot.fix) });
            }
        }
        next as usize
    }
}

/// The click line at `p` (ending before `end`) by the general rules; returns the next line's start.
#[cold]
#[inline(never)]
unsafe fn slow_click(ctx: &Ctx, p: *const u8, end: *const u8, lim: *const u8, fix: usize, cache: &mut DateCache, cw: &mut Writers<CRec>) -> *const u8 {
    unsafe {
        let mut e = p;
        while e < end && *e != b'\n' {
            e = e.add(1);
        }
        if let Some(f) = crate::scan::split_row::<4>(p, e as usize - p as usize) {
            click_general(ctx, &f, p, lim, fix, cache, cw);
        }
        e.add(1)
    }
}

/// All click lines of `base[..len]` (whole lines; the last may lack its `\n` only when it
/// ends at `lim`, the end of the readable bytes). Returns how many lines took the general path.
#[inline(always)]
pub unsafe fn hop_clicks(ctx: &Ctx, base: *const u8, len: usize, lim: *const u8, fix: usize, dates: &mut HopDates, cache: &mut DateCache, cw: &mut Writers<CRec>) -> u64 {
    unsafe {
        let mut hot = ClickHot { cbase: ctx.cbase, span: (ctx.we - ctx.cbase) as u64, fix, miss: base };
        let end = base.add(len);
        let room = lim as usize - base as usize;
        let fast_end = if room >= GUARD { base.add((room - GUARD + 1).min(len)) } else { base };
        let mut p = base;
        let mut slow = 0u64;
        let (mut retry_p, mut retry_n) = (std::ptr::null::<u8>(), 0u32);
        loop {
            let mut code = BAIL;
            while p < fast_end {
                let r = click_row(ctx, &mut hot, p, dates, cw);
                if (r as isize) < CODES {
                    code = r;
                    break;
                }
                p = r as *const u8;
            }
            if code == DATE_MISS {
                // as in `hop_loop`: at most one miss per line here, then the general path
                if retry_p != p {
                    retry_p = p;
                    retry_n = 0;
                }
                retry_n += 1;
                if retry_n <= 2 && date_miss(hot.miss, dates) != NONE {
                    continue;
                }
            }
            if p >= end {
                break;
            }
            slow += 1;
            p = slow_click(ctx, p, end, lim, fix, cache, cw);
        }
        slow
    }
}
