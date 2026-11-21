//! The general path: a plain single-threaded implementation of rules.md with no limits on the
//! window length or the number of categories. Used when the fast path's compact encodings do
//! not fit (window + 7 d >= 2^32 ms, more than 429 categories) or with SE_SLOW=1.

use crate::fast::Inputs;
use crate::parquet::{Col, PqFile};
use crate::person::{CD_SCHEMA, UD_SCHEMA};
use crate::scan::scan_scalar;
use crate::ts::{DAY, DateCache, NONE, parse_price, parse_ts};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

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

pub fn run(inp: &Inputs, out: &Path) {
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
