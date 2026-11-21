#!/usr/bin/env python3
"""
Writes fixtures_extra/<case>/{input,expected}. Every expected row below was
derived BY HAND from rules.md (see the comment next to each case); nothing
here runs the oracle. Re-run to regenerate: python oracle/build_extra_fixtures.py
"""
import json
import os
import shutil

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures_extra")

EV_H = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
CL_H = b"click_id,user_id,campaign_id,ts"
LN_H = b"user_id_a,user_id_b"
UD_H = "user_id,day,sessions,events,revenue_cents,top_category"
CD_H = "campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions"
OCT = ("2026-10-01T00:00:00Z", "2026-10-15T00:00:00Z")
PRODUCTS = b"product_id,category\np1,shoes\np2,books\n"
CAMPAIGNS = b"campaign_id,channel\nc_1,search\nc_2,social\n"

# epoch ms helpers (Oct 2026): 2026-10-01T00:00:00Z = 1790812800000
OCT1 = 1790812800000
H = 3_600_000
D = 86_400_000


def octms(day, h=0, m=0, s=0, ms=0):
    return str(OCT1 + (day - 1) * D + h * H + m * 60_000 + s * 1000 + ms)


def b(x):
    return x if isinstance(x, bytes) else x.encode("utf-8")


def lines(header, rows, final_newline=True):
    out = b"\n".join([header] + [b(r) for r in rows])
    return out + (b"\n" if final_newline else b"")


CASES = []


def case(name, catches, inputs, user_daily, campaign_daily, window=OCT, products=PRODUCTS, campaigns=CAMPAIGNS):
    """inputs: dict relpath -> bytes. events/clicks/links default to header-only files."""
    files = {
        "manifest.json": json.dumps({"window_start": window[0], "window_end": window[1]}, indent=2).encode() + b"\n",
        "products.csv": products,
        "campaigns.csv": campaigns,
    }
    for kind, h in (("events", EV_H), ("clicks", CL_H), ("links", LN_H)):
        if not any(k.startswith(kind + "/") for k in inputs):
            files[f"{kind}/part-00000.csv"] = h + b"\n"
    files.update(inputs)
    CASES.append((name, catches, files, user_daily, campaign_daily))


def ev(*f):
    return ",".join(f)


I12 = "2026-10-03T12:00:00Z"   # a safe ingest_ts for Oct 3 events

# ---------------------------------------------------------------- calendar

case("X01_leap_2028", "Feb 29 2028 is valid, Feb 30 is not; an F2 offset can move an event onto Feb 29",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2028-02-29T12:00:00Z", "2028-03-01T00:00:00Z", "view", "p1", ""),
         ev("e2", "u2", "2028-02-30T12:00:00Z", "2028-03-01T00:00:00Z", "view", "p1", ""),
         ev("e3", "u3", "2028-03-01T01:00:00+02:00", "2028-03-01T00:00:00Z", "view", "p1", ""),
     ])},
     # u1 kept (Feb 29). u2 bad (Feb 30). u3 = 2028-02-29T23:00Z -> day Feb 29.
     ["u1,2028-02-29,1,1,0,", "u3,2028-02-29,1,1,0,"],
     ["organic,organic,2028-02-29,2,0,0,0"],
     window=("2028-02-25T00:00:00Z", "2028-03-10T00:00:00Z"))

case("X02_nonleap_2027", "Feb 29 outside a leap year is bad; an F2 local date/hour is validated as written (no normalising through the offset)",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2027-02-29T12:00:00Z", "2027-03-01T00:00:00Z", "view", "p1", ""),
         ev("e2", "u2", "2027-02-28T12:00:00Z", "2027-03-01T00:00:00Z", "view", "p1", ""),
         ev("e3", "u3", "2027-03-01T00:00:00Z", "2027-03-01T00:00:00Z", "view", "p1", ""),
         # F2 local part is validated as written: local Feb 29 2027 is bad even though UTC would be Feb 28 23:00
         ev("e4", "u4", "2027-02-29T01:00:00+02:00", "2027-03-01T00:00:00Z", "view", "p1", ""),
         # local Feb 28 23:00 at -02:00 is Mar 1 01:00Z: valid, day Mar 1
         ev("e5", "u5", "2027-02-28T23:00:00-02:00", "2027-03-01T02:00:00Z", "view", "p1", ""),
         # local hour 24 is bad even though 24:00+01:00 would be a valid UTC instant
         ev("e6", "u6", "2027-02-28T24:00:00+01:00", "2027-03-01T00:00:00Z", "view", "p1", ""),
     ])},
     ["u2,2027-02-28,1,1,0,", "u3,2027-03-01,1,1,0,", "u5,2027-03-01,1,1,0,"],
     ["organic,organic,2027-02-28,1,0,0,0", "organic,organic,2027-03-01,2,0,0,0"],
     window=("2027-02-25T00:00:00Z", "2027-03-11T00:00:00Z"))

case("X03_century_2100", "2100 is not a leap year (divisible by 100, not 400)",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2100-02-29T12:00:00Z", "2100-03-01T00:00:00Z", "view", "p1", ""),
         ev("e2", "u2", "2100-03-01T00:00:00Z", "2100-03-01T00:00:00Z", "view", "p1", ""),
     ])},
     ["u2,2100-03-01,1,1,0,"],
     ["organic,organic,2100-03-01,1,0,0,0"],
     window=("2100-02-25T00:00:00Z", "2100-03-11T00:00:00Z"))

case("X04_leap_2000", "2000 is a leap year (divisible by 400); F3 for Feb 29 2000 is 951782400000",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2000-02-29T00:00:00Z", "2000-02-29T01:00:00Z", "view", "p1", ""),
         ev("e2", "u2", "0951782400000", "2000-02-29T01:00:00Z", "view", "p1", ""),
     ])},
     ["u1,2000-02-29,1,1,0,", "u2,2000-02-29,1,1,0,"],
     ["organic,organic,2000-02-29,2,0,0,0"],
     window=("2000-02-25T00:00:00Z", "2000-03-10T00:00:00Z"))

_bad_ts = [
    "2026-10-03T24:00:00Z", "2026-10-03T10:60:00Z", "2026-10-03T10:00:60Z", "2026-13-03T10:00:00Z",
    "2026-00-03T10:00:00Z", "2026-10-00T10:00:00Z", "2026-10-3T10:00:00Z", "2026-10-03T10:00:00z",
    "2026-10-03t10:00:00Z", "2026-10-03 10:00:00Z", "2026-10-03T10:00:00", "2026-10-03T10:00:00.000Z",
    "2026-10-03T10:00:00+02", "2026-10-03T10:00:00Z+00:00", "2026-10-03T10:00:00+00:00Z", "+1791021600000",
    " 1791021600000", "1791021600000 ", "2026-10-03T10:00:00 +02:00", "2026/10/03T10:00:00Z",
    "2026-10-03T1:00:00Z", "2026-10-03T10:00:00-24:00", "2026-10-03T10:00:00+2:00", "2026-10-03T10:00:00+02:0a",
    "2026-10-03T10:00:00−02:00", "20261003T100000Z", "2026-10-03T10:00Z", "2026-10-03T10:00:00UTC",
    "2026-10-03T10:00:00+-2:00", "2026-10-03T10:00:00+02:00:00", "2026-1O-03T10:00:00Z", "1791021600000Z",
]
_good_ts = ["2026-10-03T10:00:00Z", "2026-10-03T10:00:00+00:00", "2026-10-03T10:00:00-00:00",
            "2026-10-03T23:59:59Z", "2026-10-03T00:00:00Z"]
case("X05_ts_forms", "Strict F1/F2/F3 grammar: every malformed variant (bounds, case, separators, zone, sign, spaces) is bad",
     {"events/part-00000.csv": lines(EV_H,
         [ev(f"b{i}", f"b{i}", t, "2026-10-03T23:00:00Z", "view", "p1", "") for i, t in enumerate(_bad_ts)] +
         [ev(f"g{i}", f"g{i}", t, "2026-10-03T23:00:00Z", "view", "p1", "") for i, t in enumerate(_good_ts)])},
     [f"g{i},2026-10-03,1,1,0," for i in range(len(_good_ts))],
     [f"organic,organic,2026-10-03,{len(_good_ts)},0,0,0"])

case("X06_nov31", "Nov 31 is bad for events and for clicks (no rollover to Dec 1); Nov 30 is fine",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-11-31T10:00:00Z", "2026-11-30T11:00:00Z", "view", "p1", ""),
         ev("e2", "u2", "2026-11-30T10:00:00Z", "2026-11-30T11:00:00Z", "view", "p1", ""),
         ev("e3", "u3", "2026-12-01T11:00:00Z", "2026-12-01T12:00:00Z", "view", "p1", ""),
         ev("e4", "u4", "2026-12-01T11:00:00Z", "2026-12-01T12:00:00Z", "view", "p1", ""),
     ]), "clicks/part-00000.csv": lines(CL_H, [
         "k1,u3,c_1,2026-11-31T10:00:00Z",
         "k2,u4,c_2,2026-11-30T10:00:00Z",
     ])},
     ["u2,2026-11-30,1,1,0,", "u3,2026-12-01,1,1,0,", "u4,2026-12-01,1,1,0,"],
     ["organic,organic,2026-11-30,1,0,0,0", "organic,organic,2026-12-01,1,0,0,0",
      "c_2,social,2026-12-01,1,0,0,1"],
     window=("2026-11-25T00:00:00Z", "2026-12-09T00:00:00Z"))

case("X07_f2_window_edges", "F2 offsets moving ts across both window edges and across a date; a session cut by window_end",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-01T01:00:00+02:00", "2026-10-01T01:00:00Z", "view", "p1", ""),   # Sep 30 23:00Z bad
         ev("e2", "u2", "2026-09-30T20:00:00-04:00", "2026-10-01T01:00:00Z", "view", "p1", ""),   # Oct 1 00:00Z ok
         ev("e3", "u3", "2026-10-15T01:59:59+02:00", "2026-10-15T00:30:00Z", "view", "p1", ""),   # Oct 14 23:59:59 ok
         ev("e4", "u4", "2026-10-14T20:00:00-04:00", "2026-10-15T00:30:00Z", "view", "p1", ""),   # Oct 15 00:00Z bad
         ev("e5", "u5", "2026-10-03T22:00:00-10:00", "2026-10-04T09:00:00Z", "view", "p1", ""),   # Oct 4 08:00Z
         ev("e6", "u6", "2026-10-14T23:50:00Z", "2026-10-15T01:00:00Z", "view", "p1", ""),
         ev("e7", "u6", "2026-10-15T00:10:00Z", "2026-10-15T01:00:00Z", "purchase", "p1", "100"),  # out of window
         ev("e8", "u7", "2026-09-30T23:50:00Z", "2026-10-01T01:00:00Z", "view", "p1", ""),        # out
         ev("e9", "u7", "2026-10-01T00:10:00Z", "2026-10-01T01:00:00Z", "view", "p1", ""),
     ])},
     ["u2,2026-10-01,1,1,0,", "u3,2026-10-14,1,1,0,", "u5,2026-10-04,1,1,0,", "u6,2026-10-14,1,1,0,",
      "u7,2026-10-01,1,1,0,"],
     ["organic,organic,2026-10-01,2,0,0,0", "organic,organic,2026-10-14,2,0,0,0",
      "organic,organic,2026-10-04,1,0,0,0"])

# ---------------------------------------------------------------- CSV dialect

case("X08_quoting", "Quote rules: whole-field only; `\"a\"b`, mid-field, lone, triple and unbalanced quotes, extra/missing field all malformed (even in an ignored price); `\"\"` is null",
     {"events/part-00000.csv": lines(EV_H, [
         '"e1","u1","2026-10-03T10:00:00Z","2026-10-03T12:00:00Z","view","p1",""',     # kept
         'e2,"u2"x,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,',
         'e3,u"3,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,',
         'e4,u4,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,"',
         'e5,u5,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,,',
         'e6,u6,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1',
         'e7,u7,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,""',                  # kept
         'e8,u8,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,"""',
         'e10,"u10,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,',
         'e12,u12,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,"purchase","p1","0250"',     # kept, 250
         'e13,u13,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,"purchase",p1," 100"',       # bad price
         'e14,u14,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,a"b"',
         'e15,u15,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,"a""b"',
         '"e16",u16,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1, ""',
     ])},
     ["u1,2026-10-03,1,1,0,", "u7,2026-10-03,1,1,0,", "u12,2026-10-03,1,1,250,shoes"],
     ["organic,organic,2026-10-03,3,1,250,0"])

case("X09_dedup_earliest_invalid", "Dedup keeps the smallest ingest_ts even when that copy is late, mistyped, userless, productless or badly priced; a malformed earliest copy never counts; a view's junk price is fine",
     {"events/part-00000.csv": lines(EV_H, [
         # e1: earliest copy is late (ts Oct 1 10:00, ingest Oct 2 10:00:01)
         ev("e1", "u1", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""),
         ev("e1", "u1", "2026-10-01T10:00:00Z", "2026-10-02T10:00:01Z", "view", "p1", ""),
         ev("e2", "u2", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""),
         ev("e2", "u2", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "Purchase", "p1", "100"),
         ev("e3", '""', "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "view", "p1", ""),
         ev("e3", "u3", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""),
         ev("e4", "u4", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "view", "", ""),
         ev("e4", "u4", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""),
         ev("e5", "u5", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "purchase", "p1", "0"),
         ev("e5", "u5", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "purchase", "p1", "100"),
         "e6,u6,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1",                     # malformed (6 fields)
         ev("e6", "u6", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""),
         ev("e10", "u10", "2026-10-03T10:00:00Z", "2026-10-04T00:00:00Z", "view", "p1", ""),
         ev("e10", "u10", "2026-10-03 10:00:00Z", "2026-10-03T23:59:59Z", "view", "p1", ""),
         ev("e11", "u11", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "view", "p1", "abc"),
         ev("e11", "u11", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "purchase", "p1", "500"),
     ]), "events/part-00001.csv": lines(EV_H, [
         # e8: later copy (u9) in the first file, earliest copy (u8) here; e12 identical copies in two files
         ev("e8", "u8", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "view", "p1", ""),
         ev("e12", "u12", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "purchase", "p2", "300"),
         '"e12","u12","2026-10-03T10:00:00Z","2026-10-03T10:01:00Z","purchase","p2","300"',
     ]), "events/part-00000b.csv": lines(EV_H, [
         ev("e8", "u9", "2026-10-03T10:00:00Z", "2026-10-03T10:05:00Z", "view", "p1", ""),
         ev("e12", "u12", "2026-10-03T10:00:00Z", "2026-10-03T10:01:00Z", "purchase", "p2", "300"),
     ])},
     ["u6,2026-10-03,1,1,0,", "u8,2026-10-03,1,1,0,", "u11,2026-10-03,1,1,0,",
      "u12,2026-10-03,1,1,300,books"],
     ["organic,organic,2026-10-03,4,1,300,0"])

case("X10_click_dedup_empty_fields", "A click copied with different quoting credits and assists once; empty campaign_id, empty user_id and empty click_id drop the click",
     {"events/part-00000.csv": lines(EV_H, [ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "view", "p1", "")]),
      "clicks/part-00000.csv": lines(CL_H, [
          "k1,u1,c_1,2026-10-03T09:00:00Z",
          '"k1","u1","c_1","2026-10-03T09:00:00Z"',
          'k2,u1,"",2026-10-03T09:30:00Z',
          "k3,,c_2,2026-10-03T09:40:00Z",
          '"",u1,c_2,2026-10-03T09:45:00Z',
          ",u1,c_2,2026-10-03T09:50:00Z",
      ]), "clicks/part-00001.csv": lines(CL_H, ['k1,"u1",c_1,"2026-10-03T09:00:00Z"'])},
     ["u1,2026-10-03,1,1,0,"],
     ["c_1,search,2026-10-03,1,0,0,1"])

case("X11_links_chain_files", "Links chain across three files (one header-only, one without a final newline); the name X9 appears in no event or click; a click by another ID of the person credits",
     {"links/part-00000.csv": LN_H + b"\n",
      "links/part-00001.csv": lines(LN_H, ["u5,X9"]),
      "links/part-00002.csv": lines(LN_H, ["X9,u7", "u3,u7"], final_newline=False),
      "events/part-00000.csv": lines(EV_H, [
          ev("e1", "u5", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e2", "u3", "2026-10-03T10:20:00Z", I12, "view", "p1", ""),
          ev("e3", "u7", "2026-10-03T10:40:00Z", I12, "purchase", "p2", "900"),
      ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,u3,c_2,2026-10-02T10:00:00Z"])},
     ["X9,2026-10-03,1,3,900,books"],
     ["c_2,social,2026-10-03,1,1,900,1"])

case("X12_file_layout", "Header-only files (with and without newline), last line without newline, file ending in a blank line, a data row equal to the header",
     {"events/part-00000.csv": EV_H + b"\n",
      "events/part-00001.csv": EV_H,
      "events/part-00002.csv": lines(EV_H, [
          ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          EV_H.decode(),
          ev("e2", "u1", "2026-10-03T10:10:00Z", I12, "purchase", "p1", "700"),
      ], final_newline=False),
      "events/part-00003.csv": lines(EV_H, [ev("e3", "u1", "2026-10-03T10:20:00Z", I12, "view", "p2", "")]) + b"\n",
      "clicks/part-00000.csv": CL_H,
      "clicks/part-00001.csv": lines(CL_H, [CL_H.decode(), "k1,u1,c_1,2026-10-03T09:00:00Z"], final_newline=False)},
     ["u1,2026-10-03,1,3,700,shoes"],
     ["c_1,search,2026-10-03,1,1,700,1"])

case("X13_bytewise_names", "Names are the byte-wise (UTF-8, i.e. code point) smallest ID: 'B'<'a', 'Z'<'a'<'é'<'ｚ'<'😀' (UTF-16 order would put 😀 before ｚ)",
     {"links/part-00000.csv": lines(LN_H, ["é1,a1", "é2,Z2", "ｚ3,é3", "😀4,ｚ4", "a5,B5"]),
      "events/part-00000.csv": lines(EV_H, [
          ev("e1", "a1", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e2", "é1", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
          ev("e3", "é2", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e4", "Z2", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
          ev("e5", "é3", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e6", "ｚ3", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
          ev("e7", "😀4", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e8", "ｚ4", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
          ev("e9", "a5", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e10", "B5", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
      ])},
     ["a1,2026-10-03,1,2,0,", "Z2,2026-10-03,1,2,0,", "é3,2026-10-03,1,2,0,", "ｚ4,2026-10-03,1,2,0,",
      "B5,2026-10-03,1,2,0,"],
     ["organic,organic,2026-10-03,5,0,0,0"])

case("X14_upper_link_split", "An upper-cased link side is another ID: u2 and U2 are different, splitting one person into two; U2 names a person with no events",
     {"links/part-00000.csv": lines(LN_H, ["u1,u2", "U2,u3"]),
      "events/part-00000.csv": lines(EV_H, [
          ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e2", "u2", "2026-10-03T10:10:00Z", I12, "view", "p1", ""),
          ev("e3", "u3", "2026-10-03T10:20:00Z", I12, "view", "p1", ""),
      ])},
     ["u1,2026-10-03,1,2,0,", "U2,2026-10-03,1,1,0,"],
     ["organic,organic,2026-10-03,2,0,0,0"])

case("X15_interleaved_ids", "One person, three IDs interleaved: gaps measured across IDs, 30:01 splits; clicks from different IDs credit/assist",
     {"links/part-00000.csv": lines(LN_H, ["c,b", "b,a"]),
      "events/part-00000.csv": lines(EV_H, [
          ev("e1", "a", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e2", "b", "2026-10-03T10:20:00Z", I12, "view", "p1", ""),
          ev("e3", "c", "2026-10-03T10:45:00Z", I12, "purchase", "p2", "400"),
          ev("e4", "a", "2026-10-03T11:10:00Z", I12, "view", "p1", ""),
          ev("e5", "b", "2026-10-03T12:00:00Z", I12, "purchase", "p1", "100"),
          ev("e6", "c", "2026-10-03T12:29:59Z", I12, "view", "p1", ""),
          ev("e7", "a", "2026-10-03T13:00:00Z", I12, "view", "p1", ""),
      ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,c,c_1,2026-10-03T11:30:00Z", "k2,b,c_2,2026-10-03T12:45:00Z"])},
     # sessions: [10:00..11:10] organic (rev 400), [12:00,12:29:59] c_1 (rev 100), [13:00] c_2 (c_1+c_2 assist)
     ["a,2026-10-03,3,7,500,books"],
     ["organic,organic,2026-10-03,1,1,400,0", "c_1,search,2026-10-03,1,1,100,2",
      "c_2,social,2026-10-03,1,0,0,1"])

case("X16_midnight_both_tables", "A session crossing midnight counts all events and revenue on its start day in both tables",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T23:45:00Z", "2026-10-04T12:00:00Z", "view", "p1", ""),
         ev("e2", "u1", "2026-10-04T00:10:00Z", "2026-10-04T12:00:00Z", "purchase", "p1", "1000"),
         ev("e3", "u1", "2026-10-04T00:30:00Z", "2026-10-04T12:00:00Z", "purchase", "p2", "200"),
         ev("e4", "u1", "2026-10-04T09:00:00Z", "2026-10-04T12:00:00Z", "purchase", "p2", "5000"),
     ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,u1,c_1,2026-10-03T23:00:00Z"])},
     ["u1,2026-10-03,1,3,1200,shoes", "u1,2026-10-04,1,1,5000,books"],
     ["c_1,search,2026-10-03,1,1,1200,1", "c_1,search,2026-10-04,1,1,5000,1"])

case("X17_top_category", "top_category sums per day across sessions; ties byte-wise ('Zeta'<'books', 'shoes'<'unknown'); unknown can win; no valid purchase gives null",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "500"),
         ev("e2", "u1", "2026-10-03T10:01:00Z", I12, "purchase", "p9", "500"),
         ev("e3", "u2", "2026-10-03T10:00:00Z", I12, "purchase", "p9", "600"),
         ev("e4", "u2", "2026-10-03T10:01:00Z", I12, "purchase", "p1", "500"),
         ev("e5", "u3", "2026-10-03T10:00:00Z", I12, "purchase", "p3", "300"),
         ev("e6", "u3", "2026-10-03T10:01:00Z", I12, "purchase", "p2", "300"),
         ev("e7", "u4", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "400"),
         ev("e8", "u4", "2026-10-03T12:00:00Z", I12, "purchase", "p2", "300"),
         ev("e9", "u4", "2026-10-03T12:01:00Z", I12, "purchase", "p2", "200"),
         ev("e10", "u5", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "-5"),
         ev("e11", "u5", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
         ev("e12", "u6", "2026-10-03T10:00:00Z", I12, "purchase", "", "100"),
         ev("e13", "u6", "2026-10-03T10:01:00Z", I12, "view", "p1", ""),
         ev("e14", "u7", "2026-10-03T10:00:00Z", I12, "purchase", "P1", "100"),
     ])},
     ["u1,2026-10-03,1,2,1000,shoes", "u2,2026-10-03,1,2,1100,unknown", "u3,2026-10-03,1,2,600,Zeta",
      "u4,2026-10-03,2,3,900,books", "u5,2026-10-03,1,1,0,", "u6,2026-10-03,1,1,0,",
      "u7,2026-10-03,1,1,100,unknown"],
     ["organic,organic,2026-10-03,8,6,3700,0"],
     products=b"product_id,category\np1,shoes\np2,books\np3,Zeta\n")

case("X18_assists_only_rows", "A campaign that only assists gets a 0-credited row on every day it assisted; one campaign credits and assists",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-05T10:00:00Z", "2026-10-05T12:00:00Z", "purchase", "p1", "1000"),
         ev("e2", "u1", "2026-10-06T10:00:00Z", "2026-10-06T12:00:00Z", "view", "p1", ""),
     ]),
      "clicks/part-00000.csv": lines(CL_H, [
          "k1,u1,c_1,2026-10-05T09:00:00Z", "k2,u1,c_1,2026-10-05T09:10:00Z", "k3,u1,c_2,2026-10-05T09:30:00Z"])},
     ["u1,2026-10-05,1,1,1000,shoes", "u1,2026-10-06,1,1,0,"],
     ["c_1,search,2026-10-05,0,0,0,1", "c_1,search,2026-10-06,0,0,0,1",
      "c_2,social,2026-10-05,1,1,1000,1", "c_2,social,2026-10-06,1,0,0,1"])

case("X19_click_tie_bytewise", "Same-ms clicks in different forms (F1 vs F3) tie; smallest click_id byte-wise ('kB'<'ka', 'k10'<'k9'); same campaign twice assists once",
     {"events/part-00000.csv": lines(EV_H, [
         ev(f"e{i}", f"u{i}", "2026-10-03T10:00:00Z", I12, "view", "p1", "") for i in range(1, 5)]),
      "clicks/part-00000.csv": lines(CL_H, [
          "k_b,u1,c_1,2026-10-03T09:00:00Z", "k_a,u1,c_2," + octms(3, 9),
          "ka,u2,c_2,2026-10-03T09:00:00Z", "kB,u2,c_1,2026-10-03T11:00:00+02:00",
          "k9,u3,c_2,2026-10-03T09:00:00Z", "k10,u3,c_1,2026-10-03T09:00:00Z",
          "k1,u4,c_1,2026-10-03T09:00:00Z", "k2,u4,c_1,2026-10-03T09:00:00Z"])},
     ["u1,2026-10-03,1,1,0,", "u2,2026-10-03,1,1,0,", "u3,2026-10-03,1,1,0,", "u4,2026-10-03,1,1,0,"],
     ["c_1,search,2026-10-03,3,0,0,4", "c_2,social,2026-10-03,1,0,0,3"])

case("X20_click_7d_boundaries", "Exactly 7 d qualifies, 7 d + 1 ms does not, 1 ms after start does not; an F2 click exactly at the start credits",
     {"events/part-00000.csv": lines(EV_H, [
         ev(f"e{i}", f"u{i}", "2026-10-08T10:00:00Z", "2026-10-08T12:00:00Z", "view", "p1", "") for i in range(1, 5)]),
      "clicks/part-00000.csv": lines(CL_H, [
          "k1,u1,c_1," + octms(1, 10),
          "k2,u2,c_1," + octms(1, 9, 59, 59, 999), "k3,u2,c_2," + octms(8, 10, 0, 0, 1),
          "k4,u3,c_1," + octms(1, 10), "k5,u3,c_2," + octms(1, 10, 0, 0, 1),
          "k6,u4,c_1,2026-10-08T09:00:00Z", "k7,u4,c_2,2026-10-08T12:00:00+02:00"])},
     ["u1,2026-10-08,1,1,0,", "u2,2026-10-08,1,1,0,", "u3,2026-10-08,1,1,0,", "u4,2026-10-08,1,1,0,"],
     ["c_1,search,2026-10-08,1,0,0,3", "c_2,social,2026-10-08,2,0,0,2", "organic,organic,2026-10-08,1,0,0,0"])

case("X21_late_boundary", "Late cutoff in ms across forms: 24h kept, 24h+1ms dropped, F2 offset applied before the check, ingest days before ts kept",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T10:00:00Z", "2026-10-04T10:00:00Z", "view", "p1", ""),
         ev("e3", "u3", octms(3, 10, 0, 0, 1), "2026-10-04T10:00:00Z", "view", "p1", ""),
         ev("e4", "u4", octms(3, 9, 59, 59, 999), "2026-10-04T10:00:00Z", "view", "p1", ""),
         ev("e5", "u5", "2026-10-03T12:00:00+02:00", "2026-10-04T10:00:00Z", "view", "p1", ""),
         ev("e6", "u6", "2026-10-03T11:59:59+02:00", "2026-10-04T10:00:00Z", "view", "p1", ""),
         ev("e7", "u7", "2026-10-10T10:00:00Z", "2026-10-01T00:00:00Z", "view", "p1", ""),
         ev("e8", "u8", "2026-10-03T12:00:00-02:00", "2026-10-04T12:00:00Z", "view", "p1", ""),
     ])},
     # e8 ts = Oct 3 14:00Z, ingest Oct 4 12:00Z -> 22h kept (local-time reading would say 24h, still kept);
     ["u1,2026-10-03,1,1,0,", "u3,2026-10-03,1,1,0,", "u5,2026-10-03,1,1,0,", "u7,2026-10-10,1,1,0,",
      "u8,2026-10-03,1,1,0,"],
     ["organic,organic,2026-10-03,4,0,0,0", "organic,organic,2026-10-10,1,0,0,0"])

case("X22_price", "Price: 10000000 kept, 10000001 dropped, 30 leading zeros fine, 41- and 5000-digit values dropped, 0/00/+100/1e3/trailing space/empty dropped",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "10000000"),
         ev("e2", "u2", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "10000001"),
         ev("e3", "u3", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "0" * 30 + "100"),
         ev("e4", "u4", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "0" * 30 + "10000000"),
         ev("e5", "u5", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "1" + "0" * 40),
         ev("e6", "u6", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "0"),
         ev("e7", "u7", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "00"),
         ev("e8", "u8", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "+100"),
         ev("e9", "u9", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "1e3"),
         ev("e10", "u10", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "100 "),
         ev("e11", "u11", "2026-10-03T10:00:00Z", I12, "purchase", "p1", ""),
         ev("e12", "u12", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "9" * 5000),
         ev("e13", "u13", "2026-10-03T10:00:00Z", I12, "view", "p1", "abc"),
         ev("e14", "u14", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "18446744073709551716"),
         ev("e15", "u15", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "1_000"),
     ])},
     # e14 = 2^64 + 100: a wrapping u64 parse would give 100
     ["u1,2026-10-03,1,1,10000000,shoes", "u3,2026-10-03,1,1,100,shoes", "u4,2026-10-03,1,1,10000000,shoes",
      "u13,2026-10-03,1,1,0,"],
     ["organic,organic,2026-10-03,4,3,20000100,0"])

case("X23_f3_extremes", "F3 beyond 64 bits (incl. 2^64+T, which wraps into the window) is out of window; 100 leading zeros fine; 0 and signed values bad",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "18446745864731151616", I12, "view", "p1", ""),     # 2^64 + Oct 3 10:00
         ev("e2", "u2", "9223373827876375808", I12, "view", "p1", ""),      # 2^63 + Oct 3 10:00
         ev("e3", "u3", "0" * 50 + "1791021600000", I12, "view", "p1", ""),
         ev("e4", "u4", "0", I12, "view", "p1", ""),
         ev("e5", "u5", "-1791021600000", I12, "view", "p1", ""),
         ev("e6", "u6", "0" * 100 + "1791021600000", I12, "view", "p1", ""),
         ev("e7", "u7", "1" + "0" * 300, I12, "view", "p1", ""),
     ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,u3,c_1,18446745864731151616", "k2,u6,c_2," + "0" * 30 + octms(3, 9)])},
     ["u3,2026-10-03,1,1,0,", "u6,2026-10-03,1,1,0,"],
     ["organic,organic,2026-10-03,1,0,0,0", "c_2,social,2026-10-03,1,0,0,1"])

case("X24_chain_exact_30min", "A chain of exactly-30-minute gaps is one session that crosses midnight; 30:00.001 then splits on the new day",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T23:00:00Z", "2026-10-04T12:00:00Z", "view", "p1", ""),
         ev("e2", "u1", "2026-10-03T23:30:00Z", "2026-10-04T12:00:00Z", "view", "p1", ""),
         ev("e3", "u1", "2026-10-04T00:00:00Z", "2026-10-04T12:00:00Z", "view", "p1", ""),
         ev("e4", "u1", "2026-10-04T00:30:00Z", "2026-10-04T12:00:00Z", "view", "p1", ""),
         ev("e5", "u1", octms(4, 1, 0, 0, 1), "2026-10-04T12:00:00Z", "view", "p1", ""),
     ])},
     ["u1,2026-10-03,1,4,0,", "u1,2026-10-04,1,1,0,"],
     ["organic,organic,2026-10-03,1,0,0,0", "organic,organic,2026-10-04,1,0,0,0"])

case("X25_link_dirt", "Link sides: quoted-empty dropped, trailing space and trailing \\r are distinct IDs; name 'u1 ' (with space) for u2",
     {"links/part-00000.csv": lines(LN_H, ['"",u2', 'u1,""', "u3,u3", "u1 ,u2", "u4,u5\r", '"u6"," u7"']),
      "events/part-00000.csv": lines(EV_H, [
          ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e2", "u2", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e3", "u3", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e4", "u4", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e5", "u5", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e6", "u7", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e7", " u7", "2026-10-03T10:05:00Z", I12, "view", "p1", ""),
      ])},
     # " u7" (0x20) < "u6": person {u6, " u7"} named " u7"; plain u7 is separate.
     ["u1,2026-10-03,1,1,0,", "u1 ,2026-10-03,1,1,0,", "u3,2026-10-03,1,1,0,", "u4,2026-10-03,1,1,0,",
      "u5,2026-10-03,1,1,0,", "u7,2026-10-03,1,1,0,", " u7,2026-10-03,1,1,0,"],
     ["organic,organic,2026-10-03,7,0,0,0"])

case("X26_purchasing_sessions", "purchasing_sessions counts sessions, not purchases; sessions of two people credited to one campaign on one day add up",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "purchase", "p1", "100"),
         ev("e2", "u1", "2026-10-03T10:01:00Z", I12, "purchase", "p1", "100"),
         ev("e3", "u1", "2026-10-03T10:02:00Z", I12, "purchase", "p1", "100"),
         ev("e4", "u1", "2026-10-03T12:00:00Z", I12, "view", "p1", ""),
         ev("e5", "u2", "2026-10-03T10:00:00Z", I12, "purchase", "p2", "50"),
     ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,u1,c_1,2026-10-03T09:00:00Z", "k2,u2,c_1,2026-10-03T09:30:00Z"])},
     ["u1,2026-10-03,2,4,300,shoes", "u2,2026-10-03,1,1,50,books"],
     ["c_1,search,2026-10-03,3,2,350,3"])

case("X27_equal_ts_across_ids_files", "Equal-ts events from different IDs and files, read in reverse order, share a session; a gap is measured from the latest distinct ts",
     {"links/part-00000.csv": lines(LN_H, ["u2,u1"]),
      "events/part-00000.csv": lines(EV_H, [
          ev("e4", "u1", octms(3, 11, 0, 0, 1), I12, "view", "p1", ""),
          ev("e3", "u2", "2026-10-03T12:30:00+02:00", I12, "purchase", "p1", "100"),
      ]),
      "events/part-00001.csv": lines(EV_H, [
          ev("e2", "u1", octms(3, 10, 30), I12, "view", "p1", ""),
          ev("e1", "u2", "2026-10-03T10:00:00Z", I12, "view", "p1", ""),
          ev("e5", "u1", octms(3, 11, 30, 0, 1), I12, "purchase", "p2", "100"),
      ])},
     # sessions: [10:00, 10:30, 10:30] and [11:00:00.001, 11:30:00.001] -> books/shoes tie 100/100 -> books
     ["u1,2026-10-03,2,5,200,books"],
     ["organic,organic,2026-10-03,2,2,200,0"])

case("X28_click_window_vs_session", "A click 7 days before the window that is valid but older than 7 days before the session does nothing; a click 1 s before the click window is dropped",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-01T00:00:00Z", "2026-10-01T01:00:00Z", "view", "p1", ""),
         ev("e2", "u1", "2026-10-02T00:00:00Z", "2026-10-02T01:00:00Z", "view", "p1", ""),
     ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,u1,c_1,2026-09-24T00:00:00Z", "k2,u1,c_2,2026-09-23T23:59:59Z"])},
     # k1 in click window; credits Oct 1 00:00 (exactly 7d) but is 8 days before Oct 2 -> organic there.
     ["u1,2026-10-01,1,1,0,", "u1,2026-10-02,1,1,0,"],
     ["c_1,search,2026-10-01,1,0,0,1", "organic,organic,2026-10-02,1,0,0,0"])

case("X29_unknown_product_case", "Product and campaign lookups are exact: 'P1' and 'p1 ' are unknown products; 'C_1' is an unknown campaign",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "purchase", "P1", "300"),
         ev("e2", "u1", "2026-10-03T10:01:00Z", I12, "purchase", "p2", "200"),
         ev("e3", "u1", "2026-10-03T10:02:00Z", I12, "purchase", "p1 ", "200"),
     ]),
      "clicks/part-00000.csv": lines(CL_H, ["k1,u1,C_1,2026-10-03T09:00:00Z"])},
     ["u1,2026-10-03,1,3,700,unknown"],
     ["organic,organic,2026-10-03,1,1,700,0"])

case("X30_header_only_everything", "Every events/clicks/links file holds only the header: both tables are empty (but must exist as Parquet)",
     {"events/part-00000.csv": EV_H + b"\n", "events/part-00001.csv": EV_H,
      "clicks/part-00000.csv": CL_H, "links/part-00000.csv": LN_H},
     [], [])

case("X31_event_type_and_ids", "event_type exact ('cart ' , 'CART', 'click' dropped); a view/cart with an empty price is fine; event_id case matters for dedup",
     {"events/part-00000.csv": lines(EV_H, [
         ev("e1", "u1", "2026-10-03T10:00:00Z", I12, "cart", "p1", ""),
         ev("e2", "u1", "2026-10-03T10:01:00Z", I12, "cart ", "p1", ""),
         ev("e3", "u1", "2026-10-03T10:02:00Z", I12, "CART", "p1", ""),
         ev("e4", "u1", "2026-10-03T10:03:00Z", I12, "click", "p1", ""),
         ev("E1", "u1", "2026-10-03T10:04:00Z", I12, "view", "p1", "12.5"),
         ev("e5", "u1", "2026-10-03T10:05:00Z", I12, "", "p1", ""),
     ])},
     ["u1,2026-10-03,1,2,0,"],
     ["organic,organic,2026-10-03,1,0,0,0"])


def main():
    if os.path.isdir(ROOT):
        for d in os.listdir(ROOT):
            if d.startswith("X") and os.path.isdir(os.path.join(ROOT, d)):
                shutil.rmtree(os.path.join(ROOT, d))
    for name, _, files, ud, cd in CASES:
        for rel, data in files.items():
            p = os.path.join(ROOT, name, "input", rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "wb") as f:
                f.write(data)
        e = os.path.join(ROOT, name, "expected")
        os.makedirs(e, exist_ok=True)
        with open(os.path.join(e, "user_daily.csv"), "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join([UD_H] + ud) + "\n")
        with open(os.path.join(e, "campaign_daily.csv"), "w", encoding="utf-8", newline="\n") as f:
            f.write("\n".join([CD_H] + cd) + "\n")
    print(f"wrote {len(CASES)} cases")
    for name, catches, *_ in CASES:
        print(f"| {name} | {catches} |")


if __name__ == "__main__":
    main()
