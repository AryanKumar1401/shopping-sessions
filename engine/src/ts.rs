//! Timestamp and number parsing. All functions work on raw bytes.

pub const NONE: i64 = i64::MIN;
pub const DAY: i64 = 86_400_000;

#[inline]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Clone, Copy)]
struct DateEnt {
    k8: u64,
    k3: u32,
    v: i64,
}

/// Memo of `YYYY-MM-DDT` -> epoch seconds of that midnight (or NONE), keyed by the 11 raw bytes.
pub struct DateCache {
    e: [DateEnt; 256],
}
impl DateCache {
    pub fn new() -> Self {
        DateCache { e: [DateEnt { k8: 0, k3: 0, v: NONE }; 256] }
    }
}

#[inline(always)]
unsafe fn rd64(p: *const u8) -> u64 {
    unsafe { (p as *const u64).read_unaligned() }
}

#[cold]
#[inline(never)]
unsafe fn date_slow(p: *const u8, cache: &mut DateCache, slot: usize) -> i64 {
    unsafe {
        let b = std::slice::from_raw_parts(p, 11);
        let dg = |i: usize| -> i64 {
            let v = b[i].wrapping_sub(b'0');
            if v > 9 { -100_000 } else { v as i64 }
        };
        let y = dg(0) * 1000 + dg(1) * 100 + dg(2) * 10 + dg(3);
        let m = dg(5) * 10 + dg(6);
        let d = dg(8) * 10 + dg(9);
        let mut v = NONE;
        if b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && y >= 0 && (1..=12).contains(&m) && d >= 1 {
            let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
            let dim = match m {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                _ => {
                    if leap { 29 } else { 28 }
                }
            };
            if d <= dim {
                v = days_from_civil(y, m, d) * (DAY / 1000);
            }
        }
        cache.e[slot] = DateEnt { k8: rd64(p), k3: (p.add(8) as *const u32).read_unaligned() & 0x00FF_FFFF, v };
        v
    }
}

/// `YYYY-MM-DDTHH:MM:SS` (19 bytes at `p`, of which 20 are readable) -> epoch seconds, or NONE.
#[inline(always)]
pub unsafe fn civil19(p: *const u8, cache: &mut DateCache) -> i64 {
    unsafe {
        let k8 = rd64(p);
        let k3 = (p.add(8) as *const u32).read_unaligned() & 0x00FF_FFFF;
        // the low digits of the day of the month
        let slot = ((k3 as usize & 15) << 4) | ((k3 as usize >> 8) & 15);
        let e = cache.e.get_unchecked(slot);
        let day = if (e.k8 == k8) & (e.k3 == k3) { e.v } else { date_slow(p, cache, slot) };
        // "HH:MM:SS": every byte minus its '0' or ':' must be at most 2 9 0 5 9 0 5 9
        let v = rd64(p.add(11)) ^ 0x3030_3A30_303A_3030;
        let bad = (v | v.wrapping_add(0x767A_7F76_7A7F_767D)) & 0x8080_8080_8080_8080;
        // tens * 10 + ones of each pair, in bytes 1, 4 and 7
        let x = v.wrapping_mul(2561);
        let h = (x >> 8) & 0xFF;
        let m = (x >> 32) & 0xFF;
        let s = x >> 56;
        if (bad != 0) | (h > 23) | (day == NONE) {
            return NONE;
        }
        day + (h * 3600 + m * 60 + s) as i64
    }
}

/// F1 (exactly 20 bytes at `p`) -> epoch seconds, or NONE when it is not a valid F1.
#[inline(always)]
pub unsafe fn parse_f1(p: *const u8, cache: &mut DateCache) -> i64 {
    unsafe {
        let t = civil19(p, cache);
        if *p.add(19) == b'Z' { t } else { NONE }
    }
}

#[inline(always)]
fn parse8(x: u64) -> (u64, bool) {
    let ok = ((x & 0xF0F0_F0F0_F0F0_F0F0) == 0x3030_3030_3030_3030)
        & ((x.wrapping_add(0x0606_0606_0606_0606) & 0xF0F0_F0F0_F0F0_F0F0) == 0x3030_3030_3030_3030);
    let x = (x & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
    let x = (x & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
    let x = (x & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32;
    (x, ok)
}

/// One or more ASCII digits of any length -> value, or NONE (also for values >= 10^18,
/// which are outside every window and above every price).
#[inline(never)]
pub unsafe fn digits_slow(p: *const u8, len: usize) -> i64 {
    unsafe {
        if len == 0 {
            return NONE;
        }
        let b = std::slice::from_raw_parts(p, len);
        let mut v: i64 = 0;
        let mut sig = 0u32;
        for &c in b {
            let d = c.wrapping_sub(b'0');
            if d > 9 {
                return NONE;
            }
            if sig > 0 || d != 0 {
                sig += 1;
                if sig <= 18 {
                    v = v * 10 + d as i64;
                }
            }
        }
        if sig > 18 { NONE } else { v }
    }
}

/// F3: digits only.
#[inline(always)]
pub unsafe fn parse_f3(p: *const u8, len: usize) -> i64 {
    unsafe {
        if len > 8 && len <= 16 {
            let k = len - 8;
            let (lo, ok1) = parse8(rd64(p.add(k)));
            let hi = rd64(p);
            let hi = if k == 8 { hi } else { (hi << (8 * (8 - k))) | (0x3030_3030_3030_3030u64 >> (8 * k)) };
            let (hi, ok2) = parse8(hi);
            if ok1 & ok2 { (hi * 100_000_000 + lo) as i64 } else { NONE }
        } else {
            digits_slow(p, len)
        }
    }
}

/// F3 of exactly 13 digits (16 bytes readable at `p`) -> epoch ms, or NONE.
#[inline(always)]
pub unsafe fn parse_f3_13(p: *const u8) -> i64 {
    unsafe {
        let (hi, ok1) = parse8(rd64(p));
        // digits 8..12, behind three zeros in place of digits 5..7
        let (lo, ok2) = parse8((rd64(p.add(5)) & !0x00FF_FFFF) | 0x0030_3030);
        if ok1 & ok2 { (hi * 100_000 + lo) as i64 } else { NONE }
    }
}

/// Any `ts` form -> epoch ms, or NONE when it is not a valid timestamp
/// (or is an F3 value too large to be in any window).
#[inline(always)]
pub unsafe fn parse_ts(p: *const u8, len: usize, cache: &mut DateCache) -> i64 {
    unsafe {
        if len == 20 && *p.add(19) == b'Z' {
            let t = civil19(p, cache);
            if t == NONE { NONE } else { t * 1000 }
        } else if len == 25 && (*p.add(19) == b'+' || *p.add(19) == b'-') {
            let t = civil19(p, cache);
            let o = std::slice::from_raw_parts(p.add(20), 5);
            let d = |i: usize| o[i].wrapping_sub(b'0') as i64;
            let (a, b, c, e) = (d(0), d(1), d(3), d(4));
            if t == NONE || a > 9 || b > 9 || c > 9 || e > 9 || o[2] != b':' {
                return NONE;
            }
            let (hh, mm) = (a * 10 + b, c * 10 + e);
            if hh > 23 || mm > 59 {
                return NONE;
            }
            let off = (hh * 60 + mm) * 60_000;
            if *p.add(19) == b'+' { t * 1000 - off } else { t * 1000 + off }
        } else {
            parse_f3(p, len)
        }
    }
}

/// `parse_ts` out of line, for the rare forms.
#[inline(never)]
pub unsafe fn parse_ts_cold(p: *const u8, len: usize, cache: &mut DateCache) -> i64 {
    unsafe { parse_ts(p, len, cache) }
}

/// Purchase price: digits only, value 1..=10,000,000. Returns 0 when bad.
#[inline(always)]
pub unsafe fn parse_price(p: *const u8, len: usize) -> u32 {
    unsafe {
        let v = digits_slow(p, len);
        if (1..=10_000_000).contains(&v) { v as u32 } else { 0 }
    }
}

pub fn parse_ts_slice(s: &[u8]) -> i64 {
    let mut c = DateCache::new();
    unsafe { parse_ts(s.as_ptr(), s.len(), &mut c) }
}
