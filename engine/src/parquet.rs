//! Minimal Parquet writer: PLAIN, uncompressed, data page v1, thrift compact footer.

use std::fs::File;
use std::io::Write;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Str,
    StrOpt,
    Date,
    I64,
}

/// One column's buffered values for the open row group.
pub struct Col {
    pub kind: Kind,
    /// PLAIN-encoded values (non-null ones only).
    pub data: Vec<u8>,
    /// For `StrOpt`: definition levels, one bit per row, LSB first.
    pub defs: Vec<u8>,
}

impl Col {
    pub fn new(kind: Kind) -> Self {
        Col { kind, data: Vec::new(), defs: Vec::new() }
    }
    #[inline(always)]
    pub fn push_i64(&mut self, v: i64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
    #[inline(always)]
    pub fn push_i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
    #[inline(always)]
    pub fn push_str(&mut self, s: &[u8]) {
        self.data.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.data.extend_from_slice(s);
    }
    /// Sets row `row`'s definition bit (rows must be visited in order).
    #[inline(always)]
    pub fn push_def(&mut self, row: usize, present: bool) {
        if row & 7 == 0 {
            self.defs.push(0);
        }
        if present {
            *self.defs.last_mut().unwrap() |= 1 << (row & 7);
        }
    }
}

struct Th {
    b: Vec<u8>,
    last: i16,
    stack: Vec<i16>,
}
impl Th {
    fn new() -> Self {
        Th { b: Vec::new(), last: 0, stack: Vec::new() }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    fn zig(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let d = id - self.last;
        if d > 0 && d <= 15 {
            self.b.push(((d as u8) << 4) | ty);
        } else {
            self.b.push(ty);
            self.zig(id as i64);
        }
        self.last = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.field(id, 5);
        self.zig(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.field(id, 6);
        self.zig(v);
    }
    fn bin(&mut self, s: &[u8]) {
        self.varint(s.len() as u64);
        self.b.extend_from_slice(s);
    }
    fn binf(&mut self, id: i16, s: &[u8]) {
        self.field(id, 8);
        self.bin(s);
    }
    fn listf(&mut self, id: i16, ety: u8, n: usize) {
        self.field(id, 9);
        if n < 15 {
            self.b.push(((n as u8) << 4) | ety);
        } else {
            self.b.push(0xF0 | ety);
            self.varint(n as u64);
        }
    }
    fn begin(&mut self) {
        self.stack.push(self.last);
        self.last = 0;
    }
    fn structf(&mut self, id: i16) {
        self.field(id, 12);
        self.begin();
    }
    fn end(&mut self) {
        self.b.push(0);
        self.last = self.stack.pop().unwrap();
    }
}

struct ChunkMeta {
    offset: u64,
    size: u64,
    nvalues: u64,
}

pub struct PqFile {
    f: File,
    pos: u64,
    schema: Vec<(&'static str, Kind)>,
    rgs: Vec<(Vec<ChunkMeta>, u64)>,
    nrows: u64,
}

impl PqFile {
    pub fn create(path: &std::path::Path, schema: &[(&'static str, Kind)]) -> std::io::Result<Self> {
        let mut f = File::create(path)?;
        f.write_all(b"PAR1")?;
        Ok(PqFile { f, pos: 4, schema: schema.to_vec(), rgs: Vec::new(), nrows: 0 })
    }

    /// Writes the buffered columns as one row group and clears them.
    pub fn write_group(&mut self, cols: &mut [Col], nrows: usize) -> std::io::Result<()> {
        if nrows == 0 {
            return Ok(());
        }
        let mut metas = Vec::with_capacity(cols.len());
        for c in cols.iter_mut() {
            let mut lev: Vec<u8> = Vec::new();
            if c.kind == Kind::StrOpt {
                let groups = nrows.div_ceil(8);
                let mut t = Th::new();
                t.varint(((groups as u64) << 1) | 1);
                lev.extend_from_slice(&((t.b.len() + groups) as u32).to_le_bytes());
                lev.extend_from_slice(&t.b);
                debug_assert_eq!(c.defs.len(), groups);
            }
            let body = lev.len() + c.defs.len() + c.data.len();
            let mut h = Th::new();
            h.i32f(1, 0); // DATA_PAGE
            h.i32f(2, body as i32);
            h.i32f(3, body as i32);
            h.structf(5);
            h.i32f(1, nrows as i32);
            h.i32f(2, 0); // PLAIN
            h.i32f(3, 3); // RLE
            h.i32f(4, 3); // RLE
            h.end();
            h.b.push(0);
            self.f.write_all(&h.b)?;
            if !lev.is_empty() {
                self.f.write_all(&lev)?;
                self.f.write_all(&c.defs)?;
            }
            self.f.write_all(&c.data)?;
            let size = (h.b.len() + body) as u64;
            metas.push(ChunkMeta { offset: self.pos, size, nvalues: nrows as u64 });
            self.pos += size;
            c.data.clear();
            c.defs.clear();
        }
        self.rgs.push((metas, nrows as u64));
        self.nrows += nrows as u64;
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        let mut t = Th::new();
        t.i32f(1, 1);
        t.listf(2, 12, self.schema.len() + 1);
        t.begin();
        t.binf(4, b"schema");
        t.i32f(5, self.schema.len() as i32);
        t.end();
        for &(name, kind) in &self.schema {
            t.begin();
            let (ty, rep, conv) = match kind {
                Kind::Str => (6, 0, Some(0)),
                Kind::StrOpt => (6, 1, Some(0)),
                Kind::Date => (1, 0, Some(6)),
                Kind::I64 => (2, 0, None),
            };
            t.i32f(1, ty);
            t.i32f(3, rep);
            t.binf(4, name.as_bytes());
            if let Some(c) = conv {
                t.i32f(6, c);
                // LogicalType union: 1 = STRING, 6 = DATE (empty structs)
                t.structf(10);
                t.structf(if c == 0 { 1 } else { 6 });
                t.end();
                t.end();
            }
            t.end();
        }
        t.i64f(3, self.nrows as i64);
        t.listf(4, 12, self.rgs.len());
        for (metas, nrows) in &self.rgs {
            t.begin();
            t.listf(1, 12, metas.len());
            let mut total = 0u64;
            for (m, &(name, kind)) in metas.iter().zip(&self.schema) {
                total += m.size;
                t.begin();
                t.i64f(2, m.offset as i64);
                t.structf(3);
                t.i32f(1, match kind {
                    Kind::Str | Kind::StrOpt => 6,
                    Kind::Date => 1,
                    Kind::I64 => 2,
                });
                t.listf(2, 5, 2);
                t.zig(0);
                t.zig(3);
                t.listf(3, 8, 1);
                t.bin(name.as_bytes());
                t.i32f(4, 0);
                t.i64f(5, m.nvalues as i64);
                t.i64f(6, m.size as i64);
                t.i64f(7, m.size as i64);
                t.i64f(9, m.offset as i64);
                t.end();
                t.end();
            }
            t.i64f(2, total as i64);
            t.i64f(3, *nrows as i64);
            t.end();
        }
        t.binf(6, b"shopping-engine");
        t.b.push(0);
        let n = t.b.len() as u32;
        t.b.extend_from_slice(&n.to_le_bytes());
        t.b.extend_from_slice(b"PAR1");
        self.f.write_all(&t.b)?;
        Ok(())
    }
}
