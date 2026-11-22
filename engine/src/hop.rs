//! Event rows by "row hopping": no stream of delimiters is built. Each line is walked from
//! left to right, speculating that it has the usual shape
//!
//!   id , id , ts , ingest_ts , type , product , price \n
//!
//! where each `id` is 1 to 9 bytes that are all in `0x30..=0x7F`, `ts` is a 13-digit F3, an
//! F1 or an F2, `ingest_ts` is an F1, `type` is `view`, `cart` or `purchase`, the product is
//! not empty, and product, price and the newline fit in 32 bytes. Any of the first five fields
//! may be wrapped in quotes; quotes in the last two are checked with masks.
//!
//! Exactness: *every* byte of an accepted line is checked against that shape (the content of
//! each of the first five fields can hold no comma, quote or newline because of what it is
//! compared to; the bytes after each are exactly `,` or `",`; the tail has exactly one comma
//! before the first newline and its quotes wrap whole fields). Therefore an accepted line is
//! well-formed, its field boundaries are the ones `split_row` finds, and it is evaluated with
//! the same arithmetic as `event_row`. Every other line (and every line within `GUARD` bytes
//! of the end of the readable bytes) takes `split_row` + `event_general`, the plain statement
//! of the rules.

use crate::fast::*;
use crate::links::part_of;
use crate::scan::split_row;
use crate::ts::*;
use crate::util::*;

/// Bytes that must be readable from a line's start for the speculative walk: it never reads
/// further than this, whatever the bytes are (2 x (1 + 16 + 2) for the IDs, 28 + 23 for the
/// timestamps, 11 for the type, 32 for the tail = 132, and the 16-byte loads behind them).
pub const GUARD: usize = 192;

const VIEW: u32 = u32::from_le_bytes(*b"view");
const CART: u32 = u32::from_le_bytes(*b"cart");
const PURCHASE: u64 = u64::from_le_bytes(*b"purchase");

/// What must follow a field's content: `,` (index 0) or `",` (index 1), as a little-endian
/// 16-bit word and the mask of the bytes that count.
static TERM_WANT: [u16; 2] = [0x002C, 0x2C22];
static TERM_MASK: [u16; 2] = [0x00FF, 0xFFFF];

/// Nonzero unless the bytes at `t` are `,` (q = 0) or `",` (q = 1).
#[inline(always)]
unsafe fn term(t: *const u8, q: usize) -> u32 {
    unsafe {
        let w = (t as *const u16).read_unaligned();
        ((w ^ *TERM_WANT.get_unchecked(q)) & *TERM_MASK.get_unchecked(q)) as u32
    }
}

#[inline(always)]
unsafe fn rd64(p: *const u8) -> u64 {
    unsafe { (p as *const u64).read_unaligned() }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
mod k {
    use crate::util::{Id, LEN_MASK};
    use std::arch::x86_64::*;

    pub type V16 = __m128i;

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

    /// `pack_id16` of two IDs of 1..=9 bytes, all in `0x30..=0x7F` (see `pack2_id16`).
    #[inline(always)]
    pub unsafe fn pack2(v0: V16, l0: usize, v1: V16, l1: usize) -> (Id, Id) {
        unsafe {
            let data = _mm256_inserti128_si256(_mm256_castsi128_si256(v0), v1, 1);
            let m0 = _mm_loadu_si128(LEN_MASK.get_unchecked(l0).as_ptr() as *const __m128i);
            let m1 = _mm_loadu_si128(LEN_MASK.get_unchecked(l1).as_ptr() as *const __m128i);
            let d = _mm256_and_si256(data, _mm256_inserti128_si256(_mm256_castsi128_si256(m0), m1, 1));
            let w = _mm256_maddubs_epi16(_mm256_set1_epi16(0x0180), d);
            let dd = _mm256_madd_epi16(w, _mm256_set1_epi32(0x0001_4000));
            let q = _mm256_or_si256(_mm256_mul_epu32(dd, _mm256_set1_epi64x(1 << 28)), _mm256_srli_epi64(dd, 32));
            let id = _mm256_or_si256(_mm256_slli_epi64(q, 7), _mm256_srli_si256(d, 8));
            (_mm_cvtsi128_si64(_mm256_castsi256_si128(id)) as u64, _mm_cvtsi128_si64(_mm256_extracti128_si256(id, 1)) as u64)
        }
    }

    /// 13 digits at `a` (16 bytes readable) -> (value, nonzero when one is not a digit).
    #[inline(always)]
    pub unsafe fn f3_13(a: *const u8) -> (i64, u64) {
        unsafe {
            let d = _mm_sub_epi8(_mm_loadu_si128(a as *const __m128i), _mm_set1_epi8(b'0' as i8));
            // three leading zeros, then the 13 digits: a 16-digit number
            let s = _mm_slli_si128(d, 3);
            let over = _mm_subs_epu8(s, _mm_set1_epi8(9));
            let bad = (_mm_testz_si128(over, over) == 0) as u64;
            let w = _mm_maddubs_epi16(s, _mm_set1_epi16(0x010A));
            let x = _mm_madd_epi16(w, _mm_set1_epi32(0x0001_0064));
            let y = _mm_packus_epi32(x, x);
            let z = _mm_madd_epi16(y, _mm_set1_epi32(0x0001_2710));
            let hi = _mm_cvtsi128_si32(z) as u32 as u64;
            let lo = _mm_extract_epi32(z, 1) as u32 as u64;
            ((hi * 100_000_000 + lo) as i64, bad)
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
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
mod k {
    use crate::util::{Id, pack_id16};

    pub type V16 = *const u8;

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
    pub unsafe fn pack2(v0: V16, l0: usize, v1: V16, l1: usize) -> (Id, Id) {
        unsafe { (pack_id16(v0, l0), pack_id16(v1, l1)) }
    }

    #[inline(always)]
    pub unsafe fn f3_13(a: *const u8) -> (i64, u64) {
        unsafe {
            let v = crate::ts::parse_f3_13(a);
            (v, (v == crate::ts::NONE) as u64)
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
}

/// The purchase's `category * PRICE_MUL + price`, or 0 when the price is bad. Product: the
/// tail's first field (ends at bit `mcb`), price: its second (ends at `nl`); `open`: the
/// tail's opening quotes.
#[cold]
#[inline(never)]
unsafe fn purchase_pc(ev: &EvRow, s5: *const u8, mcb: u32, nl: usize, open: u32) -> u32 {
    unsafe {
        let c = mcb.trailing_zeros() as usize;
        let qp = (open & 1) as usize;
        let qr = ((open >> (c + 1)) & 1) as usize;
        let price = parse_price(s5.add(c + 1 + qr), nl - c - 1 - 2 * qr);
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

/// One line at `p` (with `GUARD` readable bytes): handles it and returns the start of the next
/// line, or returns null having done nothing when the line is not of the usual shape.
#[inline(always)]
unsafe fn hop_row(ev: &EvRow, p: *const u8, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, escape: &mut bool) -> *const u8 {
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
        let t1 = a1.add(l1);
        bad |= term(t1, q1);
        // both 1..=9 bytes (an empty or a long ID is not the usual shape)
        bad |= ((l0.wrapping_sub(1) > 8) | (l1.wrapping_sub(1) > 8)) as u32;
        // ts: F1 / F2 have a '-' after the year, a 13-digit F3 a digit
        let s2 = t1.add(q1 + 1);
        let q2 = (*s2 == b'"') as usize;
        let a2 = s2.add(q2);
        let (ts, l2);
        if *a2.add(4) == b'-' {
            let (t, e) = civil19h(a2, cache);
            if e != 0 {
                return std::ptr::null();
            }
            let z = *a2.add(19);
            if z == b'Z' {
                ts = t * 1000;
                l2 = 20;
            } else {
                // "+HH:MM" / "-HH:MM": each digit at most 2 9 . 5 9, hours at most 23
                let o = rd64(a2.add(19));
                let v = ((o >> 8) ^ 0x30_303A_3030) & 0xFF_FFFF_FFFF;
                let e = (v | v.wrapping_add(0x76_7A7F_767D)) & 0x80_8080_8080;
                let x = v.wrapping_mul(2561);
                let hh = (x >> 8) & 0xFF;
                let mm = (x >> 32) & 0xFF;
                if (e != 0) | (hh > 23) | ((z != b'+') & (z != b'-')) {
                    return std::ptr::null();
                }
                let off = ((hh * 60 + mm) * 60_000) as i64;
                ts = if z == b'+' { t * 1000 - off } else { t * 1000 + off };
                l2 = 25;
            }
        } else {
            let (t, e) = k::f3_13(a2);
            if e != 0 {
                return std::ptr::null();
            }
            ts = t;
            l2 = 13;
        }
        let t2 = a2.add(l2);
        bad |= term(t2, q2);
        // ingest_ts: F1
        let s3 = t2.add(q2 + 1);
        let q3 = (*s3 == b'"') as usize;
        let a3 = s3.add(q3);
        let (ing_s, e3) = civil19h(a3, cache);
        bad |= (e3 != 0) as u32 | (*a3.add(19) ^ b'Z') as u32;
        let t3 = a3.add(20);
        bad |= term(t3, q3);
        // event_type
        let s4 = t3.add(q3 + 1);
        let q4 = (*s4 == b'"') as usize;
        let a4 = s4.add(q4);
        let w = rd64(a4);
        let purchase = w == PURCHASE;
        let l4 = if (w as u32 == VIEW) | (w as u32 == CART) {
            4
        } else if purchase {
            8
        } else {
            return std::ptr::null();
        };
        let t4 = a4.add(l4);
        bad |= term(t4, q4);
        // product_id , price_cents \n : one comma before the first newline; every quote opens
        // or closes a field, a field that opens closes, and no quote does both
        let s5 = t4.add(q4 + 1);
        let (mc, mn, mq) = k::tail_masks(s5);
        let below = mn ^ mn.wrapping_sub(1);
        let mcb = mc & below;
        let d = (mc | mn) & below;
        let mqb = mq & below;
        let open = mqb & ((d << 1) | 1);
        let close = mqb & (d >> 1);
        let opened = (!d).wrapping_add(open) & d;
        bad |= (mn == 0) as u32 | (mcb.count_ones() != 1) as u32;
        bad |= (opened ^ (close << 1)) | (open & close) | (mqb & !(open | close));
        // the product is empty when the comma is the first byte, or the third after `""`
        bad |= (mcb | ((mcb >> 2) & open)) & 1;
        if bad != 0 {
            return std::ptr::null();
        }
        let nl = mn.trailing_zeros() as usize;
        let ing = ing_s * 1000;
        let mut ok = (ts >= ev.ctx.ws) & (ts < ev.ctx.we) & (ing.wrapping_sub(ts) <= DAY);
        let mut pc = 0u32;
        if purchase && ok {
            pc = purchase_pc(ev, s5, mcb, nl, open);
            ok = pc != 0;
        }
        let (eid, uid) = k::pack2(v0, l0, v1, l1);
        let dd = ing_s - ev.ibase_s;
        if ok {
            let prec = pw.push_ptr(part_of(id_hash(uid), ev.ctx.pbits), PRec { k: uid, ts: (ts - ev.ctx.cbase) as u32, pc });
            let idx = (prec as usize).wrapping_sub(ev.ctx.pregion.base) / std::mem::size_of::<PRec>();
            *escape |= (idx >= NOPREC as usize) | (dd as u64 > u32::MAX as u64);
            dw.push(part_of(id_hash(eid), ev.ctx.dbits), DRec { eid, ing: dd as u32, prec: idx as u32 });
        } else {
            *escape |= dd as u64 > u32::MAX as u64;
            dw.push(part_of(id_hash(eid), ev.ctx.dbits), DRec { eid, ing: dd as u32, prec: NOPREC });
        }
        s5.add(nl + 1)
    }
}

/// The line at `p` (ending before `end`) by the general rules; returns the next line's start.
#[cold]
#[inline(never)]
unsafe fn slow_row(ev: &EvRow, p: *const u8, end: *const u8, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, escape: &mut bool) -> *const u8 {
    unsafe {
        let b = std::slice::from_raw_parts(p, end as usize - p as usize);
        let n = b.iter().position(|&c| c == b'\n').unwrap_or(b.len());
        if let Some(f) = split_row::<7>(p, n) {
            event_general(ev.ctx, &f, ev.lim, cache, pw, dw, escape);
        }
        p.add(n + 1)
    }
}

/// All event lines of `base[..len]` (whole lines; the last may lack its `\n` only when it
/// ends at `ev.lim`). Returns how many lines took the general path.
#[inline(always)]
pub unsafe fn hop_events(ev: &EvRow, base: *const u8, len: usize, cache: &mut DateCache, pw: &mut Writers<PRec>, dw: &mut Writers<DRec>, escape: &mut bool) -> u64 {
    unsafe {
        let end = base.add(len);
        let mut p = base;
        let mut slow = 0u64;
        while p < end {
            if ev.lim as usize - p as usize >= GUARD {
                let nx = hop_row(ev, p, cache, pw, dw, escape);
                if !nx.is_null() {
                    p = nx;
                    continue;
                }
            }
            slow += 1;
            p = slow_row(ev, p, end, cache, pw, dw, escape);
        }
        slow
    }
}
