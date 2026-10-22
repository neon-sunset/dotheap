use crate::{bytes::ReadBytes, core_dump::CoreDump};
use memmap2::{Mmap, MmapOptions};
use std::{
    collections::HashMap,
    fs::File,
    ops::Range,
    path::Path,
    sync::{Arc, OnceLock},
};

enum Image {
    Dump {
        dump: Arc<CoreDump>,
        start: u64,
        length: usize,
    },
    Mapped(Mmap),
    Owned(Vec<u8>),
}
impl Image {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Dump {
                dump,
                start,
                length,
            } => dump.slice(*start, *length).expect("validated image range"),
            Self::Mapped(m) => m,
            Self::Owned(b) => b,
        }
    }
}

#[derive(Clone, Copy)]
enum Column {
    Fixed(usize),
    String,
    Guid,
    Blob,
    Table(usize),
    Coded(usize),
}
use Column::{Blob as B, Coded as C, Fixed as F, Guid as G, String as S, Table as T};
const SCHEMA: &[&[Column]] = &[
    &[F(2), S, G, G, G],
    &[C(11), S, S],
    &[F(4), S, S, C(0), T(4), T(6)],
    &[T(4)],
    &[F(2), S, B],
    &[T(6)],
    &[F(4), F(2), F(2), S, B, T(8)],
    &[T(8)],
    &[F(2), F(2), S],
    &[T(2), C(0)],
    &[C(5), S, B],
    &[F(2), C(1), B],
    &[C(2), C(10), B],
    &[C(3), B],
    &[F(2), C(4), B],
    &[F(2), F(4), T(2)],
    &[F(4), T(4)],
    &[B],
    &[T(2), T(0x14)],
    &[T(0x14)],
    &[F(2), S, C(0)],
    &[T(2), T(0x17)],
    &[T(0x17)],
    &[F(2), S, B],
    &[F(2), T(6), C(6)],
    &[T(2), C(7), C(7)],
    &[S],
    &[B],
    &[F(2), C(8), S, T(0x1a)],
    &[F(4), T(4)],
    &[F(4), F(4)],
    &[F(4)],
    &[F(4), F(2), F(2), F(2), F(2), F(4), B, S, S],
    &[F(4)],
    &[F(4), F(4), F(4)],
    &[F(2), F(2), F(2), F(2), F(4), B, S, S, B],
    &[F(4), T(0x23)],
    &[F(4), F(4), F(4), T(0x23)],
    &[F(4), S, B],
    &[F(4), F(4), S, S, C(9)],
    &[F(4), F(4), S, C(9)],
    &[T(2), T(2)],
    &[F(2), F(2), C(12), S],
    &[C(7), B],
    &[T(0x2a), C(0)],
];
const CODED: &[(usize, &[usize])] = &[
    (2, &[2, 1, 0x1b]),
    (2, &[4, 8, 0x17]),
    (
        5,
        &[
            6, 4, 1, 2, 8, 9, 0xa, 0, 0xe, 0x17, 0x14, 0x11, 0x1a, 0x1b, 0x20, 0x23, 0x26, 0x27,
            0x28, 0x2a, 0x2c, 0x2b,
        ],
    ),
    (1, &[4, 8]),
    (2, &[2, 6, 0x20]),
    (3, &[2, 1, 0x1a, 6, 0x1b]),
    (1, &[0x14, 0x17]),
    (1, &[6, 0xa]),
    (1, &[4, 6]),
    (2, &[0x26, 0x23, 0x27]),
    (3, &[6, 0xa]),
    (2, &[0, 0x1a, 0x23, 1]),
    (1, &[2, 6]),
];

#[derive(Default)]
struct Table {
    start: usize,
    size: usize,
    count: usize,
    columns: Vec<(usize, usize)>,
}

pub struct AssemblyMetadata {
    image: Arc<Image>,
    loaded: bool,
    strings: Range<usize>,
    blobs: Range<usize>,
    tables: Vec<Table>,
    enclosing: HashMap<usize, usize>,
    definitions: OnceLock<HashMap<String, usize>>,
}

impl AssemblyMetadata {
    pub fn from_file(path: &Path) -> Option<Self> {
        let file = File::open(path).ok()?;
        // The image remains mapped for the metadata's lifetime and is only read.
        let map = unsafe { MmapOptions::new().map(&file).ok()? };
        Self::parse(Arc::new(Image::Mapped(map)), false)
    }
    pub fn from_dump(dump: Arc<CoreDump>, start: u64, length: usize) -> Option<Self> {
        let image = if dump.slice(start, length).is_some() {
            Image::Dump {
                dump,
                start,
                length,
            }
        } else {
            Image::Owned(dump.bytes(start, length)?.into_owned())
        };
        let image = Arc::new(image);
        Self::parse(image.clone(), false).or_else(|| Self::parse(image, true))
    }
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        let image = Arc::new(Image::Owned(bytes));
        Self::parse(image.clone(), false).or_else(|| Self::parse(image, true))
    }
    pub fn image_length(&self) -> Option<usize> {
        image_length(self.image.bytes(), self.loaded)
    }
    fn parse(image: Arc<Image>, loaded: bool) -> Option<Self> {
        let b = image.bytes();
        let (rva, _) = directory(b, 14)?;
        if rva == 0 {
            return None;
        }
        let cli = image_offset(b, rva, loaded)?;
        let md = image_offset(b, b.u32(cli.checked_add(8)?)? as usize, loaded)?;
        let streams = read_metadata_streams(b, md, true)?;
        let tables = streams.get("#~").or_else(|| streams.get("#-"))?.clone();
        let strings = streams.get("#Strings")?.clone();
        let blobs = streams.get("#Blob").cloned().unwrap_or(0..0);
        let heap_sizes = b.u8(tables.start.checked_add(6)?)?;
        let valid = b.u64(tables.start.checked_add(8)?)?;
        let mut counts = [0usize; 64];
        let mut at = tables.start.checked_add(24)?;
        for (i, count) in counts.iter_mut().enumerate() {
            if valid & (1u64 << i) != 0 {
                *count = b.u32(at)? as usize;
                at += 4;
            }
        }
        if heap_sizes & 0x40 != 0 {
            at += 4;
        }
        let width = |c: Column| -> usize {
            match c {
                F(n) => n,
                S => {
                    if heap_sizes & 1 != 0 {
                        4
                    } else {
                        2
                    }
                }
                G => {
                    if heap_sizes & 2 != 0 {
                        4
                    } else {
                        2
                    }
                }
                B => {
                    if heap_sizes & 4 != 0 {
                        4
                    } else {
                        2
                    }
                }
                T(t) => {
                    if counts[t] < 65536 {
                        2
                    } else {
                        4
                    }
                }
                C(c) => {
                    let (bits, tables) = CODED[c];
                    if tables.iter().map(|&t| counts[t]).max().unwrap_or(0) < 1 << (16 - bits) {
                        2
                    } else {
                        4
                    }
                }
            }
        };
        let mut parsed = Vec::new();
        for (i, schema) in SCHEMA.iter().enumerate() {
            let mut size = 0;
            let columns = schema
                .iter()
                .map(|&c| {
                    let n = width(c);
                    let offset = size;
                    size += n;
                    (offset, n)
                })
                .collect();
            let end = at.checked_add(counts[i].checked_mul(size)?)?;
            if end > tables.end {
                return None;
            }
            parsed.push(Table {
                start: at,
                size,
                count: counts[i],
                columns,
            });
            at = end;
        }
        if parsed[2].count == 0 {
            return None;
        }
        let mut result = Self {
            image,
            loaded,
            strings,
            blobs,
            tables: parsed,
            enclosing: HashMap::new(),
            definitions: OnceLock::new(),
        };
        for rid in 1..=result.tables[0x29].count {
            result.enclosing.insert(
                result.cell(0x29, rid, 0)? as usize,
                result.cell(0x29, rid, 1)? as usize,
            );
        }
        Some(result)
    }
    pub fn type_count(&self) -> usize {
        self.tables[2].count
    }
    pub fn cell(&self, table: usize, rid: usize, column: usize) -> Option<u32> {
        let t = self.tables.get(table)?;
        if rid == 0 || rid > t.count {
            return None;
        }
        let (offset, size) = *t.columns.get(column)?;
        let at = t.start + (rid - 1) * t.size + offset;
        if size == 2 {
            self.image.bytes().u16(at).map(u32::from)
        } else {
            self.image.bytes().u32(at)
        }
    }
    fn string(&self, index: u32) -> String {
        let bytes = self.image.bytes();
        let Some(start) = self
            .strings
            .start
            .checked_add(index as usize)
            .filter(|&s| s < self.strings.end)
        else {
            return "?".into();
        };
        let text = &bytes[start..self.strings.end];
        String::from_utf8_lossy(&text[..memchr::memchr(0, text).unwrap_or(text.len())]).into_owned()
    }
    fn blob(&self, index: u32) -> Option<&[u8]> {
        let heap = &self.image.bytes()[self.blobs.clone()];
        let mut at = index as usize;
        let size = compressed_integer(heap, &mut at)? as usize;
        heap.get(at..at.checked_add(size)?)
    }
    pub fn field_name(&self, rid: usize) -> Option<String> {
        Some(self.string(self.cell(4, rid, 1)?))
    }
    pub fn field_signature(&self, rid: usize) -> Option<&[u8]> {
        self.blob(self.cell(4, rid, 2)?)
    }
    pub fn method_name(&self, rid: usize) -> Option<String> {
        Some(self.string(self.cell(6, rid, 3)?))
    }
    pub fn assembly_name(&self) -> Option<String> {
        Some(self.string(self.cell(0x20, 1, 7)?))
    }
    pub fn assembly_reference_name(&self, rid: usize) -> Option<String> {
        Some(self.string(self.cell(0x23, rid, 6)?))
    }
    pub fn type_reference(&self, rid: usize) -> Option<TypeReference> {
        let scope = self.cell(1, rid, 0)? as usize;
        Some(TypeReference {
            namespace: self.string(self.cell(1, rid, 2)?),
            name: self.string(self.cell(1, rid, 1)?),
            scope_table: [0, 0x1a, 0x23, 1][scope & 3],
            scope_rid: scope >> 2,
        })
    }
    pub fn type_name(&self, rid: usize) -> Option<String> {
        self.type_name_at_depth(rid, 0)
    }
    fn type_name_at_depth(&self, rid: usize, depth: usize) -> Option<String> {
        if depth >= 64 {
            return None;
        }
        let name = self.string(self.cell(2, rid, 1)?);
        if let Some(&outer) = self.enclosing.get(&rid).filter(|&&v| v != rid) {
            return Some(format!(
                "{}+{name}",
                self.type_name_at_depth(outer, depth + 1)?
            ));
        }
        let ns = self.string(self.cell(2, rid, 2)?);
        Some(if ns.is_empty() {
            name
        } else {
            format!("{ns}.{name}")
        })
    }
    pub fn type_definition(&self, name: &str) -> Option<usize> {
        self.definitions
            .get_or_init(|| {
                (1..=self.type_count())
                    .filter_map(|rid| self.type_name(rid).map(|name| (name, rid)))
                    .collect()
            })
            .get(name)
            .copied()
    }
    pub fn methods(&self, rid: usize) -> Range<usize> {
        let Some(first) = self.cell(2, rid, 5).map(|v| v as usize) else {
            return 0..0;
        };
        let end = self
            .cell(2, rid + 1, 5)
            .map(|v| v as usize)
            .unwrap_or(self.tables[6].count + 1);
        first..end.min(self.tables[6].count + 1).max(first)
    }
    pub fn declaring_type(&self, method: usize) -> Option<usize> {
        let (mut lo, mut hi) = (1, self.type_count());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.methods(mid).start <= method {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        self.methods(lo).contains(&method).then_some(lo)
    }
    pub fn method_body(&self, rid: usize) -> Option<&[u8]> {
        let b = self.image.bytes();
        let rva = self.cell(6, rid, 0)? as usize;
        if rva == 0 {
            return None;
        }
        let at = image_offset(b, rva, self.loaded)?;
        let first = b.u8(at)?;
        let (start, size) = match first & 3 {
            2 => (at + 1, (first >> 2) as usize),
            3 => {
                let header = (b.u16(at)? >> 12) as usize * 4;
                if header < 12 {
                    return None;
                }
                (at.checked_add(header)?, b.u32(at.checked_add(4)?)? as usize)
            }
            _ => return None,
        };
        b.get(start..start.checked_add(size)?)
    }
    pub fn calls(&self, rid: usize, kind: &str, method: &str) -> bool {
        let Some(body) = self.method_body(rid) else {
            return false;
        };
        let mut at = 0;
        while let Some(instruction) = instruction(body, &mut at) {
            if instruction.opcode != 0x28 && instruction.opcode != 0x6f {
                continue;
            }
            let token = instruction.operand.u32(0).unwrap();
            let row = (token & 0xffffff) as usize;
            match token >> 24 {
                6 => {
                    if self.method_name(row).as_deref() == Some(method)
                        && self
                            .declaring_type(row)
                            .and_then(|t| self.type_name(t))
                            .as_deref()
                            == Some(kind)
                    {
                        return true;
                    }
                }
                0xa => {
                    if self.cell(0xa, row, 1).map(|n| self.string(n)).as_deref() == Some(method)
                        && self.member_parent_name(row).as_deref() == Some(kind)
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
    fn member_parent_name(&self, rid: usize) -> Option<String> {
        let parent = self.cell(0xa, rid, 0)? as usize;
        match parent & 7 {
            0 => self.type_name(parent >> 3),
            1 => {
                let r = self.type_reference(parent >> 3)?;
                Some(if r.namespace.is_empty() {
                    r.name
                } else {
                    format!("{}.{}", r.namespace, r.name)
                })
            }
            _ => None,
        }
    }
    pub fn informational_version(&self) -> Option<String> {
        self.assembly_name()?;
        for rid in 1..=self.tables[0xc].count {
            let parent = self.cell(0xc, rid, 0)?;
            if parent & 31 != 14 || parent >> 5 != 1 {
                continue;
            }
            let ctor = self.cell(0xc, rid, 1)? as usize;
            let name = match ctor & 7 {
                2 => self
                    .declaring_type(ctor >> 3)
                    .and_then(|t| self.type_name(t)),
                3 => self.member_parent_name(ctor >> 3),
                _ => None,
            };
            if name.as_deref() != Some("System.Reflection.AssemblyInformationalVersionAttribute") {
                continue;
            }
            let value = self.blob(self.cell(0xc, rid, 2)?)?;
            if value.u16(0) != Some(1) {
                continue;
            }
            let mut at = 2;
            let n = compressed_integer(value, &mut at)? as usize;
            return Some(String::from_utf8_lossy(value.get(at..at.checked_add(n)?)?).into_owned());
        }
        None
    }
    pub fn debug_entries(&self) -> Vec<(u32, &[u8])> {
        let b = self.image.bytes();
        let Some((rva, size)) = directory(b, 6) else {
            return vec![];
        };
        let Some(at) = image_offset(b, rva, self.loaded).filter(|_| rva != 0) else {
            return vec![];
        };
        (0..size / 28)
            .filter_map(|k| {
                let e = at.checked_add(k * 28)?;
                let size = b.u32(e.checked_add(16)?)? as usize;
                let offset = if self.loaded {
                    image_offset(b, b.u32(e.checked_add(20)?)? as usize, true)?
                } else {
                    b.u32(e.checked_add(24)?)? as usize
                };
                if offset == 0 {
                    return None;
                }
                Some((b.u32(e + 12)?, b.get(offset..offset.checked_add(size)?)?))
            })
            .collect()
    }
}

pub struct TypeReference {
    pub namespace: String,
    pub name: String,
    pub scope_table: usize,
    pub scope_rid: usize,
}

pub(crate) fn image_length(bytes: &[u8], loaded: bool) -> Option<usize> {
    if bytes.u16(0)? != 0x5a4d {
        return None;
    }
    let pe = bytes.u32(0x3c)? as usize;
    if bytes.u32(pe)? != 0x4550 {
        return None;
    }
    let optional = pe.checked_add(24)?;
    if loaded {
        return Some(bytes.u32(optional.checked_add(56)?)? as usize);
    }
    let mut length = bytes.u32(optional.checked_add(60)?)? as usize;
    let sections = bytes.u16(pe.checked_add(6)?)? as usize;
    let table = optional.checked_add(bytes.u16(pe.checked_add(20)?)? as usize)?;
    for index in 0..sections {
        let section = table.checked_add(index * 40)?;
        let size = bytes.u32(section.checked_add(16)?)? as usize;
        let offset = bytes.u32(section.checked_add(20)?)? as usize;
        if size != 0 {
            length = length.max(offset.checked_add(size)?);
        }
    }
    Some(length)
}

fn directory(b: &[u8], k: usize) -> Option<(usize, usize)> {
    if b.u16(0)? != 0x5a4d {
        return None;
    }
    let pe = b.u32(0x3c)? as usize;
    if b.u32(pe)? != 0x4550 {
        return None;
    }
    let optional = pe.checked_add(24)?;
    let (prefix, number) = match b.u16(optional)? {
        0x20b => (112, 108),
        0x10b => (96, 92),
        _ => return None,
    };
    if k >= b.u32(optional.checked_add(number)?)? as usize {
        return None;
    }
    let at = optional.checked_add(prefix + k * 8)?;
    Some((b.u32(at)? as usize, b.u32(at + 4)? as usize))
}

fn image_offset(b: &[u8], rva: usize, loaded: bool) -> Option<usize> {
    if loaded {
        return (rva < b.len()).then_some(rva);
    }
    let pe = b.u32(0x3c)? as usize;
    let sections = b.u16(pe.checked_add(6)?)? as usize;
    let table = pe.checked_add(24 + b.u16(pe.checked_add(20)?)? as usize)?;
    for s in 0..sections {
        let e = table.checked_add(s * 40)?;
        let va = b.u32(e + 12)? as usize;
        let raw_size = b.u32(e + 16)? as usize;
        let raw = b.u32(e + 20)? as usize;
        let size = (b.u32(e + 8)? as usize).max(raw_size);
        if rva >= va && rva - va < size {
            return raw.checked_add(rva - va).filter(|&o| o < b.len());
        }
    }
    None
}

pub fn metadata_streams(b: &[u8], md: usize) -> Option<HashMap<String, Range<usize>>> {
    read_metadata_streams(b, md, false)
}

fn read_metadata_streams(
    b: &[u8],
    md: usize,
    partial: bool,
) -> Option<HashMap<String, Range<usize>>> {
    if b.u32(md)? != 0x424a_5342 {
        return None;
    }
    let version = b.u32(md.checked_add(12)?)? as usize;
    let mut at = md.checked_add(16 + version + 2)?;
    let count = b.u16(at)?;
    at += 2;
    let mut streams = HashMap::new();
    for _ in 0..count {
        let offset = b.u32(at)? as usize;
        let size = b.u32(at + 4)? as usize;
        let name_start = at + 8;
        let end = name_start.checked_add(memchr::memchr(0, b.get(name_start..)?)?)?;
        let name = String::from_utf8_lossy(&b[name_start..end]).into_owned();
        let start = md.checked_add(offset)?;
        let stop = start.checked_add(size)?;
        if !partial && stop > b.len() {
            return None;
        }
        // A core can omit the last image pages while retaining its tables and names.
        let available = start.min(b.len())..stop.min(b.len());
        if streams.insert(name, available).is_some() {
            return None;
        }
        at = (end + 4) & !3;
    }
    Some(streams)
}

pub fn compressed_integer(b: &[u8], at: &mut usize) -> Option<u32> {
    let first = *b.get(*at)?;
    let (count, mut value) = if first & 0x80 == 0 {
        (1, first as u32)
    } else if first & 0xc0 == 0x80 {
        (2, (first & 0x3f) as u32)
    } else if first & 0xe0 == 0xc0 {
        (4, (first & 0x1f) as u32)
    } else {
        return None;
    };
    for i in 1..count {
        value = (value << 8) | *b.get(at.checked_add(i)?)? as u32;
    }
    *at += count;
    Some(value)
}

pub struct Instruction<'a> {
    pub offset: usize,
    pub opcode: u16,
    pub operand: &'a [u8],
}
pub fn instruction<'a>(b: &'a [u8], at: &mut usize) -> Option<Instruction<'a>> {
    let start = *at;
    let first = *b.get(start)?;
    let (opcode, prefix) = if first == 0xfe {
        (0xfe00 | *b.get(start + 1)? as u16, 2)
    } else {
        (first as u16, 1)
    };
    let operand = start + prefix;
    let size = match opcode {
        0x0e..=0x13 | 0x1f | 0x2b..=0x37 | 0xde | 0xfe12 | 0xfe19 => 1,
        0xfe09..=0xfe0e => 2,
        0x20
        | 0x22
        | 0x27..=0x29
        | 0x38..=0x44
        | 0x6f..=0x75
        | 0x79
        | 0x7b..=0x81
        | 0x8c..=0x8d
        | 0x8f
        | 0xa3..=0xa5
        | 0xc2
        | 0xc6
        | 0xd0
        | 0xdd
        | 0xfe06..=0xfe07
        | 0xfe15..=0xfe16
        | 0xfe1c => 4,
        0x21 | 0x23 => 8,
        0x45 => 4usize.checked_add((b.u32(operand)? as usize).checked_mul(4)?)?,
        0x00..=0x0d
        | 0x14..=0x1e
        | 0x25..=0x26
        | 0x2a
        | 0x46..=0x6e
        | 0x76
        | 0x7a
        | 0x82..=0x8b
        | 0x8e
        | 0x90..=0xa2
        | 0xb3..=0xba
        | 0xc3
        | 0xd1..=0xdc
        | 0xdf..=0xe0
        | 0xfe00..=0xfe05
        | 0xfe0f
        | 0xfe11
        | 0xfe13..=0xfe14
        | 0xfe17..=0xfe18
        | 0xfe1a
        | 0xfe1d..=0xfe1e => 0,
        _ => return None,
    };
    let end = operand.checked_add(size)?;
    let data = b.get(operand..end)?;
    *at = end;
    Some(Instruction {
        offset: start,
        opcode,
        operand: data,
    })
}
