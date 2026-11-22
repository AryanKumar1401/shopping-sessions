//! The general path: rules.md with no limits on the window length, the number of categories
//! or campaigns, the ID bytes or the form of `ingest_ts`. Used when a fast-path precondition
//! fails (see `fast::run` returning false) or with `SE_SLOW=1`.
//!
//! Two implementations of the same thing:
//! * `run_simple`: single-threaded, standard maps. The reference (`SE_SLOW=2`).
//! * `run`: the same row logic (the same scalar line splitter, timestamp and price parsers,
//!   the same conditions in the same order) on `nthreads` threads:
//!   1. links, products, campaigns: single-threaded, as in `run_simple`;
//!   2. parse: the files are cut at newlines into chunks; each thread turns well-formed lines
//!      into records, events partitioned by the hash of `event_id`, clicks by the hash of
//!      the person;
//!   3. dedup: one hash map per event partition, smallest `ingest_ts` wins (ties: the copy
//!      read first, as in `run_simple`); surviving good events go to the person's partition;
//!   4. people: per person partition, events sorted by (person, ts), clicks by
//!      (person, ts, click_id); sessions, credit and assists per person; `user_daily` rows
//!      are written as they are produced, `campaign_daily` counters are merged at the end.
//!
//! All timestamps and sums are 64-bit. Nothing here depends on the fast path's encodings.

use crate::fast::Inputs;
use crate::parquet::{Col, PqFile};
use crate::person::{CD_SCHEMA, UD_SCHEMA};
use crate::scan::scan_scalar;
use crate::ts::{DAY, DateCache, NONE, parse_price, parse_ts};
use crate::util::{mum, par};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

type S = &'static [u8];

unsafe fn sl(f: (*const u8, usize)) -> S {
    unsafe { std::slice::from_raw_parts(f.0, f.1) }
}

/// The file without its header line.
fn body(data: S) -> S {
    match data.iter().position(|&c| c == b'\n') {
        Some(i) => &data[i + 1..],
        None => &[],
    }
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

struct Ev {
    ing: i64,
    /// (person, ts, price or 0, category) when the copy is a good event
    good: Option<(S, i64, u64, S)>,
}

/// The reference implementation: single-threaded, standard maps, nothing clever. `run` must
/// produce the same tables; it is also the last resort for an ID of 4 GiB or more.
/// Force it with `SE_SLOW=2`.
fn run_simple(inp: &Inputs, out: &Path) {
    let mut cache = DateCache::new();
    // links
    let mut ids: HashMap<S, usize> = HashMap::new();
    let mut names: Vec<S> = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    for &file in &inp.links {
        let b = body(file);
        unsafe {
            scan_scalar::<2>(b.as_ptr(), b.len(), |f, _| {
                let (a, c) = (sl(f[0]), sl(f[1]));
                if a.is_empty() || c.is_empty() {
                    return;
                }
                let mut node = |s: S| -> usize {
                    *ids.entry(s).or_insert_with(|| {
                        names.push(s);
                        parent.push(names.len() - 1);
                        names.len() - 1
                    })
                };
                let (x, y) = (node(a), node(c));
                let (rx, ry) = (find(&mut parent, x), find(&mut parent, y));
                if rx != ry {
                    parent[rx] = ry;
                }
            });
        }
    }
    let mut smallest: Vec<S> = names.clone();
    for i in 0..names.len() {
        let r = find(&mut parent, i);
        if names[i] < smallest[r] {
            smallest[r] = names[i];
        }
    }
    let mut person_of: HashMap<S, S> = HashMap::new();
    for i in 0..names.len() {
        let r = find(&mut parent, i);
        person_of.insert(names[i], smallest[r]);
    }
    let person = |id: S| -> S { *person_of.get(id).unwrap_or(&id) };

    let mut category: HashMap<S, S> = HashMap::new();
    let b = body(inp.products);
    unsafe {
        scan_scalar::<2>(b.as_ptr(), b.len(), |f, _| {
            category.entry(sl(f[0])).or_insert(sl(f[1]));
        });
    }
    let mut channel: HashMap<S, S> = HashMap::new();
    let b = body(inp.campaigns);
    unsafe {
        scan_scalar::<2>(b.as_ptr(), b.len(), |f, _| {
            channel.entry(sl(f[0])).or_insert(sl(f[1]));
        });
    }

    // events: dedup on event_id keeping the smallest ingest_ts
    let mut events: HashMap<S, Ev> = HashMap::new();
    for &file in &inp.events {
        let b = body(file);
        unsafe {
            scan_scalar::<7>(b.as_ptr(), b.len(), |f, _| {
                let eid = sl(f[0]);
                if eid.is_empty() {
                    return;
                }
                let ing = parse_ts(f[3].0, f[3].1, &mut cache);
                if ing == NONE {
                    return;
                }
                let (user, ty, product) = (sl(f[1]), sl(f[4]), sl(f[5]));
                let ts = parse_ts(f[2].0, f[2].1, &mut cache);
                let mut good = None;
                if !user.is_empty() && ts != NONE && ts >= inp.ws && ts < inp.we && ing - ts <= DAY && !product.is_empty() {
                    let cat: S = category.get(product).copied().unwrap_or(b"unknown");
                    if ty == b"view" || ty == b"cart" {
                        good = Some((person(user), ts, 0, cat));
                    } else if ty == b"purchase" {
                        let price = parse_price(f[6].0, f[6].1);
                        if price != 0 {
                            good = Some((person(user), ts, price as u64, cat));
                        }
                    }
                }
                match events.get_mut(eid) {
                    Some(e) => {
                        if ing < e.ing {
                            *e = Ev { ing, good };
                        }
                    }
                    None => {
                        events.insert(eid, Ev { ing, good });
                    }
                }
            });
        }
    }
    let mut by_person: BTreeMap<S, Vec<(i64, u64, S)>> = BTreeMap::new();
    for e in events.values() {
        if let Some((p, ts, price, cat)) = e.good {
            by_person.entry(p).or_default().push((ts, price, cat));
        }
    }
    drop(events);

    // clicks: (ts, click_id, campaign) per person
    let mut clicks: HashMap<S, Vec<(i64, S, S)>> = HashMap::new();
    for &file in &inp.clicks {
        let b = body(file);
        unsafe {
            scan_scalar::<4>(b.as_ptr(), b.len(), |f, _| {
                let (cid, user, camp) = (sl(f[0]), sl(f[1]), sl(f[2]));
                let ts = parse_ts(f[3].0, f[3].1, &mut cache);
                if cid.is_empty() || user.is_empty() || ts == NONE || ts < inp.ws - 7 * DAY || ts >= inp.we || camp.is_empty() || !channel.contains_key(camp) {
                    return;
                }
                clicks.entry(person(user)).or_default().push((ts, cid, camp));
            });
        }
    }

    let mut ud: BTreeMap<(S, i64), (u64, u64, u64, BTreeMap<S, u64>)> = BTreeMap::new();
    let mut cd: BTreeMap<(S, i64), [u64; 4]> = BTreeMap::new();
    for (name, evs) in by_person.iter_mut() {
        evs.sort_unstable_by_key(|e| e.0);
        let cl = clicks.get(name);
        let mut i = 0;
        while i < evs.len() {
            let mut j = i + 1;
            while j < evs.len() && evs[j].0 - evs[j - 1].0 <= 1_800_000 {
                j += 1;
            }
            let start = evs[i].0;
            let day = start.div_euclid(DAY);
            let row = ud.entry((name, day)).or_insert((0, 0, 0, BTreeMap::new()));
            let mut rev = 0u64;
            for e in &evs[i..j] {
                if e.1 > 0 {
                    rev += e.1;
                    *row.3.entry(e.2).or_default() += e.1;
                }
            }
            row.0 += 1;
            row.1 += (j - i) as u64;
            row.2 += rev;
            let mut credit: S = b"organic";
            if let Some(cl) = cl {
                let mut best: Option<(i64, S, S)> = None;
                let mut assists: BTreeSet<S> = BTreeSet::new();
                for &(ts, cid, camp) in cl {
                    if ts <= start && start - ts <= 7 * DAY {
                        assists.insert(camp);
                        if best.is_none_or(|b| ts > b.0 || (ts == b.0 && cid < b.1)) {
                            best = Some((ts, cid, camp));
                        }
                    }
                }
                if let Some(b) = best {
                    credit = b.2;
                }
                for a in assists {
                    cd.entry((a, day)).or_default()[3] += 1;
                }
            }
            let c = cd.entry((credit, day)).or_default();
            c[0] += 1;
            c[1] += (rev > 0) as u64;
            c[2] += rev;
            i = j;
        }
    }

    let mut cols: Vec<Col> = UD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect();
    let mut rows = 0;
    for ((name, day), (sessions, events, rev, cats)) in &ud {
        cols[0].push_str(name);
        cols[1].push_i32(*day as i32);
        cols[2].push_i64(*sessions as i64);
        cols[3].push_i64(*events as i64);
        cols[4].push_i64(*rev as i64);
        // highest sum; ties to the byte-wise smallest (BTreeMap iterates in that order)
        let mut top: Option<(S, u64)> = None;
        for (&c, &v) in cats {
            if top.is_none_or(|t| v > t.1) {
                top = Some((c, v));
            }
        }
        cols[5].push_def(rows, top.is_some());
        if let Some(t) = top {
            cols[5].push_str(t.0);
        }
        rows += 1;
    }
    let mut f = PqFile::create(&out.join("user_daily").join("part-000.parquet"), &UD_SCHEMA).expect("create user_daily");
    f.write_group(&mut cols, rows).expect("write user_daily");
    f.finish().expect("finish user_daily");

    let mut cols: Vec<Col> = CD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect();
    let mut rows = 0;
    for ((camp, day), v) in &cd {
        cols[0].push_str(camp);
        cols[1].push_str(if *camp == b"organic" { b"organic" } else { channel[camp] });
        cols[2].push_i32(*day as i32);
        for k in 0..4 {
            cols[3 + k].push_i64(v[k] as i64);
        }
        rows += 1;
    }
    let mut f = PqFile::create(&out.join("campaign_daily").join("part-000.parquet"), &CD_SCHEMA).expect("create campaign_daily");
    f.write_group(&mut cols, rows).expect("write campaign_daily");
    f.finish().expect("finish campaign_daily");
}

// ---------------------------------------------------------------------------------------------
// The parallel implementation.

/// Partitions of the dedup (by `event_id`) and of the people phase (by person).
const PARTS: usize = 256;
/// Records per block of a `Bucket`.
const BLOCK: usize = 1024;
/// Rows of `user_daily` buffered per thread before they are written as a row group.
const GROUP_ROWS: usize = 1 << 18;

fn hash(s: &[u8]) -> u64 {
    const K0: u64 = 0x9E37_79B9_7F4A_7C15;
    const K1: u64 = 0xD6E8_FEB8_6659_FD93;
    let mut h = K0 ^ (s.len() as u64).wrapping_mul(K1);
    let mut c = s.chunks_exact(8);
    for w in &mut c {
        h = mum(h ^ u64::from_le_bytes(w.try_into().unwrap()), K1).wrapping_add(K0);
    }
    let r = c.remainder();
    if !r.is_empty() {
        let mut t = [0u8; 8];
        t[..r.len()].copy_from_slice(r);
        h = mum(h ^ u64::from_le_bytes(t), K1).wrapping_add(K0);
    }
    mum(h, K0)
}

/// The partition of a key. Bits from the middle of the hash: the maps inside a partition use
/// the low bits (bucket) and the top seven (tag) of the hash of the same bytes.
fn part(s: &[u8]) -> usize {
    (hash(s) >> 40) as usize % PARTS
}

/// `hash` as a `std` hasher (the maps are keyed by byte strings and by small tuples).
#[derive(Clone, Copy, Default)]
struct Bh;
struct Hh(u64);
impl BuildHasher for Bh {
    type Hasher = Hh;
    fn build_hasher(&self) -> Hh {
        Hh(0)
    }
}
impl Hasher for Hh {
    fn write(&mut self, b: &[u8]) {
        self.0 = hash(b) ^ self.0.rotate_left(23);
    }
    fn write_usize(&mut self, _n: usize) {
        // the length prefix of a byte string: `hash` already mixes the length in
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
type Map<K, V> = HashMap<K, V, Bh>;

/// An append-only list in blocks of `BLOCK` records (no reallocation, no doubling).
struct Bucket<T> {
    blocks: Vec<Vec<T>>,
}
impl<T> Bucket<T> {
    fn new() -> Self {
        Bucket { blocks: Vec::new() }
    }
    fn push(&mut self, v: T) {
        match self.blocks.last_mut() {
            Some(b) if b.len() < BLOCK => b.push(v),
            _ => {
                let mut b = Vec::with_capacity(BLOCK);
                b.push(v);
                self.blocks.push(b);
            }
        }
    }
}
fn buckets<T>() -> Vec<Bucket<T>> {
    (0..PARTS).map(|_| Bucket::new()).collect()
}

/// A byte string of the input as (address, length).
fn bs(p: usize, n: u32) -> S {
    unsafe { std::slice::from_raw_parts(p as *const u8, n as usize) }
}

/// `cat` of a copy that is not a good event.
const BAD: u32 = u32::MAX;

/// One well-formed copy of an event with a non-empty `event_id` and a valid `ingest_ts`.
#[derive(Clone, Copy)]
struct Rec {
    eid: usize,
    /// `user_id` (not yet the person); the next four fields are set only when `cat != BAD`
    user: usize,
    ing: i64,
    ts: i64,
    eid_len: u32,
    user_len: u32,
    /// purchase price, 0 for a view or cart
    price: u32,
    /// index into `Tables::cat_names`, or `BAD`
    cat: u32,
}

/// A surviving good event.
#[derive(Clone, Copy)]
struct Pe {
    person: usize,
    ts: i64,
    person_len: u32,
    price: u32,
    cat: u32,
}

/// A valid click.
#[derive(Clone, Copy)]
struct Click {
    cid: usize,
    person: usize,
    ts: i64,
    cid_len: u32,
    person_len: u32,
    /// index into `Tables::camp_names`
    camp: u32,
}

/// Where a byte of the input sits in reading order: (file number, address). Only used to
/// break ties that rules.md says cannot happen (copies of an event with equal `ingest_ts`
/// that differ, copies of a click that differ) the way `run_simple` does: first read wins.
struct Order {
    /// (start address, file number), sorted
    files: Vec<(usize, usize)>,
}
impl Order {
    fn new(inp: &Inputs) -> Self {
        let mut files: Vec<(usize, usize)> =
            inp.events.iter().chain(&inp.clicks).enumerate().filter(|(_, f)| !f.is_empty()).map(|(i, f)| (f.as_ptr() as usize, i)).collect();
        files.sort_unstable();
        Order { files }
    }
    fn key(&self, p: usize) -> (usize, usize) {
        let i = self.files.partition_point(|f| f.0 <= p);
        (self.files[i - 1].1, p)
    }
}

struct Tables {
    /// linked ID -> the person's name
    person_of: Map<S, S>,
    /// product -> index into `cat_names`
    category: Map<S, u32>,
    cat_names: Vec<S>,
    /// index of the category `unknown`
    unknown: u32,
    /// campaign -> index into `camp_names` / `channels`
    camp: Map<S, u32>,
    camp_names: Vec<S>,
    channels: Vec<S>,
    /// index of the campaign named `organic` (the one of campaigns.csv if it has one)
    organic: u32,
}
impl Tables {
    fn person(&self, id: S) -> S {
        *self.person_of.get(id).unwrap_or(&id)
    }
}

/// Links, products and campaigns: the logic of `run_simple`, with names as indices.
fn tables(inp: &Inputs) -> Tables {
    let mut ids: Map<S, usize> = Map::default();
    let mut names: Vec<S> = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    for &file in &inp.links {
        let b = body(file);
        unsafe {
            scan_scalar::<2>(b.as_ptr(), b.len(), |f, _| {
                let (a, c) = (sl(f[0]), sl(f[1]));
                if a.is_empty() || c.is_empty() {
                    return;
                }
                let mut node = |s: S| -> usize {
                    *ids.entry(s).or_insert_with(|| {
                        names.push(s);
                        parent.push(names.len() - 1);
                        names.len() - 1
                    })
                };
                let (x, y) = (node(a), node(c));
                let (rx, ry) = (find(&mut parent, x), find(&mut parent, y));
                if rx != ry {
                    parent[rx] = ry;
                }
            });
        }
    }
    drop(ids);
    let mut smallest: Vec<S> = names.clone();
    for i in 0..names.len() {
        let r = find(&mut parent, i);
        if names[i] < smallest[r] {
            smallest[r] = names[i];
        }
    }
    let mut person_of: Map<S, S> = Map::with_capacity_and_hasher(names.len(), Bh);
    for i in 0..names.len() {
        let r = find(&mut parent, i);
        person_of.insert(names[i], smallest[r]);
    }

    // product -> category: the first line of a product wins; categories are numbered by name
    let mut cat_ids: Map<S, u32> = Map::default();
    let mut cat_names: Vec<S> = Vec::new();
    let mut cat_id = |name: S, cat_names: &mut Vec<S>| -> u32 {
        *cat_ids.entry(name).or_insert_with(|| {
            cat_names.push(name);
            (cat_names.len() - 1) as u32
        })
    };
    let mut category: Map<S, u32> = Map::default();
    let b = body(inp.products);
    unsafe {
        scan_scalar::<2>(b.as_ptr(), b.len(), |f, _| {
            if let Entry::Vacant(e) = category.entry(sl(f[0])) {
                e.insert(cat_id(sl(f[1]), &mut cat_names));
            }
        });
    }
    let unknown = cat_id(b"unknown", &mut cat_names);

    // campaign -> channel: the first line of a campaign wins
    let mut camp: Map<S, u32> = Map::default();
    let mut camp_names: Vec<S> = Vec::new();
    let mut channels: Vec<S> = Vec::new();
    let b = body(inp.campaigns);
    unsafe {
        scan_scalar::<2>(b.as_ptr(), b.len(), |f, _| {
            if let Entry::Vacant(e) = camp.entry(sl(f[0])) {
                e.insert(camp_names.len() as u32);
                camp_names.push(sl(f[0]));
                channels.push(sl(f[1]));
            }
        });
    }
    // `organic` is not a campaign a click can name unless campaigns.csv lists it; either way
    // there is one counter row per name.
    let organic = match camp.get(&b"organic"[..]) {
        Some(&i) => i,
        None => {
            camp_names.push(b"organic");
            channels.push(b"organic");
            (camp_names.len() - 1) as u32
        }
    };
    Tables { person_of, category, cat_names, unknown, camp, camp_names, channels, organic }
}

/// Cuts a file's lines into pieces of about `chunk` bytes, each ending after a `\n`
/// (or at the end of the file).
fn cut(file: S, chunk: usize, click: bool, tasks: &mut Vec<(bool, S)>) {
    let b = body(file);
    let mut s = 0;
    while s < b.len() {
        let mut e = s.saturating_add(chunk).min(b.len());
        if e < b.len() {
            e = match b[e..].iter().position(|&c| c == b'\n') {
                Some(i) => e + i + 1,
                None => b.len(),
            };
        }
        tasks.push((click, &b[s..e]));
        s = e;
    }
}

/// What one parse thread produced.
struct Parsed {
    ev: Vec<Bucket<Rec>>,
    cl: Vec<Bucket<Click>>,
    /// an ID of 4 GiB or more: does not fit a record
    big: bool,
}

/// The event and click lines of `b` (whole lines) as records: the row logic of `run_simple`.
fn parse(inp: &Inputs, t: &Tables, click: bool, b: S, cache: &mut DateCache, o: &mut Parsed) {
    let fits = |s: S| s.len() < u32::MAX as usize;
    if !click {
        unsafe {
            scan_scalar::<7>(b.as_ptr(), b.len(), |f, _| {
                let eid = sl(f[0]);
                if eid.is_empty() {
                    return;
                }
                let ing = parse_ts(f[3].0, f[3].1, cache);
                if ing == NONE {
                    return;
                }
                let (user, ty, product) = (sl(f[1]), sl(f[4]), sl(f[5]));
                let ts = parse_ts(f[2].0, f[2].1, cache);
                // (price or 0, category) when the copy is a good event
                let mut good: Option<(u32, u32)> = None;
                if !user.is_empty() && ts != NONE && ts >= inp.ws && ts < inp.we && ing - ts <= DAY && !product.is_empty() {
                    let cat = t.category.get(product).copied().unwrap_or(t.unknown);
                    if ty == b"view" || ty == b"cart" {
                        good = Some((0, cat));
                    } else if ty == b"purchase" {
                        let price = parse_price(f[6].0, f[6].1);
                        if price != 0 {
                            good = Some((price, cat));
                        }
                    }
                }
                o.big |= !fits(eid) || !fits(user);
                let mut r = Rec { eid: eid.as_ptr() as usize, user: 0, ing, ts: 0, eid_len: eid.len() as u32, user_len: 0, price: 0, cat: BAD };
                if let Some((price, cat)) = good {
                    r = Rec { user: user.as_ptr() as usize, ts, user_len: user.len() as u32, price, cat, ..r };
                }
                o.ev[part(eid)].push(r);
            });
        }
    } else {
        unsafe {
            scan_scalar::<4>(b.as_ptr(), b.len(), |f, _| {
                let (cid, user, camp) = (sl(f[0]), sl(f[1]), sl(f[2]));
                let ts = parse_ts(f[3].0, f[3].1, cache);
                if cid.is_empty() || user.is_empty() || ts == NONE || ts < inp.ws - 7 * DAY || ts >= inp.we || camp.is_empty() {
                    return;
                }
                let Some(&camp) = t.camp.get(camp) else { return };
                let person = t.person(user);
                o.big |= !fits(cid) || !fits(person);
                o.cl[part(person)].push(Click { cid: cid.as_ptr() as usize, person: person.as_ptr() as usize, ts, cid_len: cid.len() as u32, person_len: person.len() as u32, camp });
            });
        }
    }
}

/// The blocks of partition `p` from every thread's buckets, each behind a lock so that the
/// worker that takes a partition can free it when it is done.
fn regroup<T>(mut per_thread: Vec<Vec<Bucket<T>>>) -> Vec<Mutex<Vec<Vec<T>>>> {
    (0..PARTS)
        .map(|p| {
            let mut all = Vec::new();
            for t in per_thread.iter_mut() {
                all.append(&mut t[p].blocks);
            }
            Mutex::new(all)
        })
        .collect()
}

fn take<T>(m: &Mutex<Vec<T>>) -> Vec<T> {
    std::mem::take(&mut *m.lock().unwrap())
}

/// Dedup of one event partition: the copy with the smallest `ingest_ts` is kept (of equal
/// ones, the one read first); kept good events go to `out`, by person.
fn dedup(t: &Tables, order: &Order, blocks: Vec<Vec<Rec>>, map: &mut Map<S, Rec>, out: &mut [Bucket<Pe>], big: &mut bool) {
    map.clear();
    map.reserve(blocks.iter().map(|b| b.len()).sum());
    for r in blocks.iter().flatten() {
        match map.entry(bs(r.eid, r.eid_len)) {
            Entry::Vacant(e) => {
                e.insert(*r);
            }
            Entry::Occupied(mut e) => {
                let e = e.get_mut();
                if r.ing < e.ing || (r.ing == e.ing && order.key(r.eid) < order.key(e.eid)) {
                    *e = *r;
                }
            }
        }
    }
    for r in map.values() {
        if r.cat != BAD {
            let person = t.person(bs(r.user, r.user_len));
            *big |= person.len() >= u32::MAX as usize;
            out[part(person)].push(Pe { person: person.as_ptr() as usize, ts: r.ts, person_len: person.len() as u32, price: r.price, cat: r.cat });
        }
    }
}

/// One day of one person: sessions, events, revenue, revenue per category.
#[derive(Default)]
struct DayRow {
    sessions: u64,
    events: u64,
    rev: u64,
    cats: BTreeMap<S, u64>,
}

/// A click of the person being worked on.
#[derive(Clone, Copy)]
struct PClick {
    person: u32,
    camp: u32,
    ts: i64,
    cid: usize,
    cid_len: u32,
}

/// The state of one people-phase thread.
struct People<'a> {
    t: &'a Tables,
    order: &'a Order,
    file: &'a Mutex<PqFile>,
    /// buffered `user_daily` rows
    cols: Vec<Col>,
    rows: usize,
    /// (campaign, day) -> sessions, purchasing sessions, revenue, assisted sessions
    cd: Map<(u32, i64), [u64; 4]>,
    /// per campaign: the number of the last session it assisted (sessions count from 1)
    seen: Vec<u64>,
    nsessions: u64,
    // scratch of `partition`
    index: Map<S, u32>,
    names: Vec<S>,
    evs: Vec<(u32, i64, u32, u32)>,
    cls: Vec<PClick>,
}

impl<'a> People<'a> {
    fn new(t: &'a Tables, order: &'a Order, file: &'a Mutex<PqFile>) -> Self {
        People {
            t,
            order,
            file,
            cols: UD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect(),
            rows: 0,
            cd: Map::default(),
            seen: vec![0; t.camp_names.len()],
            nsessions: 0,
            index: Map::default(),
            names: Vec::new(),
            evs: Vec::new(),
            cls: Vec::new(),
        }
    }

    fn flush(&mut self) {
        self.file.lock().unwrap().write_group(&mut self.cols, self.rows).expect("write user_daily");
        self.rows = 0;
    }

    /// All people of one partition.
    fn partition(&mut self, events: Vec<Vec<Pe>>, clicks: Vec<Vec<Click>>) {
        self.index.clear();
        self.names.clear();
        self.evs.clear();
        self.cls.clear();
        // people are numbered in order of appearance; events as (person, ts, price, category)
        for e in events.iter().flatten() {
            let name = bs(e.person, e.person_len);
            let names = &mut self.names;
            let p = *self.index.entry(name).or_insert_with(|| {
                names.push(name);
                (names.len() - 1) as u32
            });
            self.evs.push((p, e.ts, e.price, e.cat));
        }
        assert!(self.names.len() < u32::MAX as usize);
        drop(events);
        // clicks of people without events credit nothing
        for c in clicks.iter().flatten() {
            if let Some(&p) = self.index.get(bs(c.person, c.person_len)) {
                self.cls.push(PClick { person: p, camp: c.camp, ts: c.ts, cid: c.cid, cid_len: c.cid_len });
            }
        }
        drop(clicks);
        self.evs.sort_unstable();
        let order = self.order;
        self.cls.sort_unstable_by(|a, b| {
            (a.person, a.ts)
                .cmp(&(b.person, b.ts))
                .then_with(|| bs(a.cid, a.cid_len).cmp(bs(b.cid, b.cid_len)))
                .then_with(|| order.key(a.cid).cmp(&order.key(b.cid)))
        });
        let evs = std::mem::take(&mut self.evs);
        let cls = std::mem::take(&mut self.cls);
        let (mut i, mut ci) = (0, 0);
        while i < evs.len() {
            let p = evs[i].0;
            let mut j = i;
            while j < evs.len() && evs[j].0 == p {
                j += 1;
            }
            while ci < cls.len() && cls[ci].person < p {
                ci += 1;
            }
            let mut cj = ci;
            while cj < cls.len() && cls[cj].person == p {
                cj += 1;
            }
            self.person(self.names[p as usize], &evs[i..j], &cls[ci..cj]);
            i = j;
            ci = cj;
        }
        self.evs = evs;
        self.cls = cls;
    }

    /// One person: `evs` sorted by ts, `cl` sorted by (ts, click_id, reading order).
    fn person(&mut self, name: S, evs: &[(u32, i64, u32, u32)], cl: &[PClick]) {
        let t = self.t;
        let mut days: BTreeMap<i64, DayRow> = BTreeMap::new();
        let mut i = 0;
        while i < evs.len() {
            let mut j = i + 1;
            while j < evs.len() && evs[j].1 - evs[j - 1].1 <= 1_800_000 {
                j += 1;
            }
            let start = evs[i].1;
            let day = start.div_euclid(DAY);
            let row = days.entry(day).or_default();
            let mut rev = 0u64;
            for e in &evs[i..j] {
                if e.2 > 0 {
                    rev += e.2 as u64;
                    *row.cats.entry(t.cat_names[e.3 as usize]).or_default() += e.2 as u64;
                }
            }
            row.sessions += 1;
            row.events += (j - i) as u64;
            row.rev += rev;
            // the clicks with start - 7 d <= ts <= start are cl[lo..hi]
            let lo = cl.partition_point(|c| start - c.ts > 7 * DAY);
            let hi = cl.partition_point(|c| c.ts <= start);
            let mut credit = t.organic;
            if lo < hi {
                // the latest ts; of those the smallest click_id; of those the one read first
                let last = cl[hi - 1].ts;
                credit = cl[cl.partition_point(|c| c.ts < last)].camp;
                self.nsessions += 1;
                for c in &cl[lo..hi] {
                    let seen = &mut self.seen[c.camp as usize];
                    if *seen != self.nsessions {
                        *seen = self.nsessions;
                        self.cd.entry((c.camp, day)).or_default()[3] += 1;
                    }
                }
            }
            let c = self.cd.entry((credit, day)).or_default();
            c[0] += 1;
            c[1] += (rev > 0) as u64;
            c[2] += rev;
            i = j;
        }
        for (day, r) in &days {
            self.cols[0].push_str(name);
            self.cols[1].push_i32(*day as i32);
            self.cols[2].push_i64(r.sessions as i64);
            self.cols[3].push_i64(r.events as i64);
            self.cols[4].push_i64(r.rev as i64);
            // highest sum; ties to the byte-wise smallest (BTreeMap iterates in that order)
            let mut top: Option<(S, u64)> = None;
            for (&c, &v) in &r.cats {
                if top.is_none_or(|t| v > t.1) {
                    top = Some((c, v));
                }
            }
            self.cols[5].push_def(self.rows, top.is_some());
            if let Some(t) = top {
                self.cols[5].push_str(t.0);
            }
            self.rows += 1;
        }
        if self.rows >= GROUP_ROWS || self.cols[0].data.len() >= (256 << 20) {
            self.flush();
        }
    }
}

pub fn run(inp: &Inputs, out: &Path, nthreads: usize, tm: &mut crate::util::Timer) {
    if std::env::var("SE_SLOW").as_deref() == Ok("2") {
        return run_simple(inp, out);
    }
    let nthreads = nthreads.max(1);
    let t = &tables(inp);
    if t.cat_names.len() >= BAD as usize || t.camp_names.len() >= u32::MAX as usize {
        return run_simple(inp, out);
    }
    let order = &Order::new(inp);
    tm.lap("general: links and tables");

    // parse
    let chunk = std::env::var("SE_CHUNK").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0).unwrap_or(4 << 20);
    let mut tasks: Vec<(bool, S)> = Vec::new();
    for &f in &inp.events {
        cut(f, chunk, false, &mut tasks);
    }
    for &f in &inp.clicks {
        cut(f, chunk, true, &mut tasks);
    }
    let next = AtomicUsize::new(0);
    let parsed = par((0..nthreads).collect(), |_, _| {
        let mut o = Parsed { ev: buckets(), cl: buckets(), big: false };
        let mut cache = DateCache::new();
        loop {
            let k = next.fetch_add(1, Relaxed);
            if k >= tasks.len() {
                break;
            }
            parse(inp, t, tasks[k].0, tasks[k].1, &mut cache, &mut o);
        }
        o
    });
    if parsed.iter().any(|p| p.big) {
        drop(parsed);
        return run_simple(inp, out);
    }
    let (mut ev, mut cl) = (Vec::new(), Vec::new());
    for p in parsed {
        ev.push(p.ev);
        cl.push(p.cl);
    }
    let (ev, cl) = (regroup(ev), regroup(cl));
    tm.lap("general: parse");

    // dedup
    let next = AtomicUsize::new(0);
    let deduped = par((0..nthreads).collect(), |_, _| {
        let mut out = buckets();
        let mut map: Map<S, Rec> = Map::default();
        let mut big = false;
        loop {
            let p = next.fetch_add(1, Relaxed);
            if p >= PARTS {
                break;
            }
            dedup(t, order, take(&ev[p]), &mut map, &mut out, &mut big);
        }
        (out, big)
    });
    drop(ev);
    if deduped.iter().any(|d| d.1) {
        drop(deduped);
        drop(cl);
        return run_simple(inp, out);
    }
    let pe = regroup(deduped.into_iter().map(|d| d.0).collect());
    tm.lap("general: dedup");

    // people
    let file = Mutex::new(PqFile::create(&out.join("user_daily").join("part-000.parquet"), &UD_SCHEMA).expect("create user_daily"));
    let next = AtomicUsize::new(0);
    let cds = par((0..nthreads).collect(), |_, _| {
        let mut st = People::new(t, order, &file);
        loop {
            let p = next.fetch_add(1, Relaxed);
            if p >= PARTS {
                break;
            }
            st.partition(take(&pe[p]), take(&cl[p]));
        }
        st.flush();
        st.cd
    });
    file.into_inner().unwrap().finish().expect("finish user_daily");
    tm.lap("general: people");

    // campaign_daily: one row per (campaign name, day), in that order
    let mut cd: BTreeMap<(S, i64), [u64; 4]> = BTreeMap::new();
    for m in &cds {
        for (&(camp, day), v) in m {
            let row = cd.entry((t.camp_names[camp as usize], day)).or_default();
            for k in 0..4 {
                row[k] += v[k];
            }
        }
    }
    let mut cols: Vec<Col> = CD_SCHEMA.iter().map(|&(_, k)| Col::new(k)).collect();
    let mut rows = 0;
    let mut f = PqFile::create(&out.join("campaign_daily").join("part-000.parquet"), &CD_SCHEMA).expect("create campaign_daily");
    for ((camp, day), v) in &cd {
        cols[0].push_str(camp);
        cols[1].push_str(if *camp == b"organic" { b"organic" } else { t.channels[t.camp[camp] as usize] });
        cols[2].push_i32(*day as i32);
        for k in 0..4 {
            cols[3 + k].push_i64(v[k] as i64);
        }
        rows += 1;
        if rows >= GROUP_ROWS {
            f.write_group(&mut cols, rows).expect("write campaign_daily");
            rows = 0;
        }
    }
    f.write_group(&mut cols, rows).expect("write campaign_daily");
    f.finish().expect("finish campaign_daily");
}
