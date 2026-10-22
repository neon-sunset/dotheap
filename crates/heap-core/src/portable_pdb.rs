use crate::{
    bytes::ReadBytes,
    metadata::{compressed_integer, instruction, metadata_streams},
};
use std::{collections::BTreeMap, io::Read, ops::Range};

pub struct PortablePdb {
    bytes: Vec<u8>,
    pub id: Vec<u8>,
    blobs: Range<usize>,
    guids: Range<usize>,
    rows: [usize; 64],
    starts: [usize; 64],
    sizes: [usize; 64],
    blob_size: usize,
    guid_size: usize,
    document_size: usize,
    parent_size: usize,
}
pub struct SequencePoint {
    pub offset: usize,
    pub line: i32,
    pub document: usize,
}

impl PortablePdb {
    pub fn embedded(data: &[u8]) -> Option<Self> {
        if data.get(..4)? != b"MPDB" {
            return None;
        }
        let size = data.u32(4)? as usize;
        if size == 0 || size >= 1 << 30 {
            return None;
        }
        let mut decoder = flate2::read::DeflateDecoder::new(data.get(8..)?).take(size as u64 + 1);
        let mut bytes = Vec::new();
        decoder.read_to_end(&mut bytes).ok()?;
        if bytes.len() != size {
            return None;
        }
        Self::parse(bytes)
    }
    pub fn parse(bytes: Vec<u8>) -> Option<Self> {
        let streams = metadata_streams(&bytes, 0)?;
        let tables = streams.get("#~").or_else(|| streams.get("#-"))?;
        let pdb = streams.get("#Pdb")?;
        let id = bytes.get(pdb.start..pdb.start.checked_add(20)?)?.to_vec();
        let referenced = bytes.u64(pdb.start.checked_add(24)?)?;
        let mut rows = [0usize; 64];
        let mut at = pdb.start.checked_add(32)?;
        for (t, row) in rows.iter_mut().enumerate() {
            if referenced & (1u64 << t) != 0 {
                *row = bytes.u32(at)? as usize;
                at += 4;
            }
        }
        if at > pdb.end {
            return None;
        }
        let heap_sizes = bytes.u8(tables.start.checked_add(6)?)?;
        let (string_size, guid_size, blob_size) = (
            if heap_sizes & 1 == 0 { 2 } else { 4 },
            if heap_sizes & 2 == 0 { 2 } else { 4 },
            if heap_sizes & 4 == 0 { 2 } else { 4 },
        );
        let valid = bytes.u64(tables.start.checked_add(8)?)?;
        if valid & !(0xffu64 << 0x30) != 0 {
            return None;
        }
        at = tables.start.checked_add(24)?;
        for (t, row) in rows.iter_mut().enumerate() {
            if valid & (1u64 << t) != 0 {
                *row = bytes.u32(at)? as usize;
                at += 4;
            }
        }
        if heap_sizes & 0x40 != 0 {
            at += 4;
        }
        let table = |t: usize| if rows[t] < 65536 { 2 } else { 4 };
        let parents = [
            6, 4, 1, 2, 8, 9, 0xa, 0, 0xe, 0x17, 0x14, 0x11, 0x1a, 0x1b, 0x20, 0x23, 0x26, 0x27,
            0x28, 0x2a, 0x2c, 0x2b, 0x30, 0x32, 0x33, 0x34, 0x35,
        ];
        let parent_size = if parents.iter().map(|&t| rows[t]).max().unwrap_or(0) < 1 << 11 {
            2
        } else {
            4
        };
        let sizes_debug = [
            blob_size * 2 + guid_size * 2,
            table(0x30) + blob_size,
            table(6) + table(0x35) + table(0x33) + table(0x34) + 8,
            4 + string_size,
            string_size + blob_size,
            table(0x35) + blob_size,
            table(6) * 2,
            parent_size + guid_size + blob_size,
        ];
        let (mut starts, mut sizes) = ([0usize; 64], [0usize; 64]);
        for t in 0x30..=0x37 {
            starts[t] = at;
            sizes[t] = sizes_debug[t - 0x30];
            at = at.checked_add(rows[t].checked_mul(sizes[t])?)?;
            if at > tables.end {
                return None;
            }
        }
        let document_size = table(0x30);
        let blobs = streams.get("#Blob")?.clone();
        let guids = streams.get("#GUID")?.clone();
        Some(Self {
            bytes,
            id,
            blobs,
            guids,
            rows,
            starts,
            sizes,
            blob_size,
            guid_size,
            document_size,
            parent_size,
        })
    }
    fn index(&self, at: usize, size: usize) -> Option<usize> {
        if size == 2 {
            self.bytes.u16(at).map(|n| n as usize)
        } else {
            self.bytes.u32(at).map(|n| n as usize)
        }
    }
    fn blob(&self, index: usize) -> Option<&[u8]> {
        if index == 0 {
            return None;
        }
        let heap = &self.bytes[self.blobs.clone()];
        let mut at = index;
        let count = compressed_integer(heap, &mut at)? as usize;
        heap.get(at..at.checked_add(count)?)
    }
    fn guid(&self, index: usize) -> Option<&[u8]> {
        if index == 0 {
            return None;
        }
        let at = (index - 1).checked_mul(16)?;
        self.bytes[self.guids.clone()].get(at..at.checked_add(16)?)
    }
    fn row(&self, table: usize, rid: usize) -> Option<usize> {
        (rid > 0 && rid <= self.rows[table])
            .then(|| self.starts[table] + (rid - 1) * self.sizes[table])
    }
    pub fn document_name(&self, rid: usize) -> Option<String> {
        let row = self.row(0x30, rid)?;
        let data = self.blob(self.index(row, self.blob_size)?)?;
        let separator = *data.first()?;
        let mut at = 1;
        let mut parts = Vec::new();
        while at < data.len() {
            let index = compressed_integer(data, &mut at)? as usize;
            let part = if index == 0 { &[] } else { self.blob(index)? };
            parts.push(String::from_utf8_lossy(part).into_owned());
        }
        Some(parts.join(&if separator == 0 {
            String::new()
        } else {
            char::from(separator).to_string()
        }))
    }
    pub fn sequence_points(&self, rid: usize) -> Vec<SequencePoint> {
        let mut result = Vec::new();
        let _ = (|| {
            let row = self.row(0x31, rid)?;
            let mut document = self.index(row, self.document_size)?;
            let data = self.blob(self.index(row + self.document_size, self.blob_size)?)?;
            let mut at = 0;
            compressed_integer(data, &mut at)?;
            if document == 0 {
                document = compressed_integer(data, &mut at)? as usize;
            }
            let (mut offset, mut line, mut first) = (0usize, None::<i32>, true);
            while at < data.len() {
                let delta = compressed_integer(data, &mut at)? as usize;
                if delta == 0 && !first {
                    document = compressed_integer(data, &mut at)? as usize;
                    continue;
                }
                offset = offset.checked_add(delta)?;
                first = false;
                let lines = compressed_integer(data, &mut at)?;
                let columns = if lines == 0 {
                    compressed_integer(data, &mut at)? as i32
                } else {
                    signed_integer(data, &mut at)?
                };
                if lines == 0 && columns == 0 {
                    continue;
                }
                line = Some(if let Some(previous) = line {
                    let delta = signed_integer(data, &mut at)?;
                    signed_integer(data, &mut at)?;
                    previous.checked_add(delta)?
                } else {
                    let value = compressed_integer(data, &mut at)? as i32;
                    compressed_integer(data, &mut at)?;
                    value
                });
                result.push(SequencePoint {
                    offset,
                    line: line?,
                    document,
                });
            }
            Some(())
        })();
        result
    }
    pub fn yield_offsets(&self, method: usize) -> Vec<usize> {
        const ASYNC_STEPPING: &[u8] = &[
            0xc5, 0x2a, 0xfd, 0x54, 0x25, 0xe9, 0x1a, 0x40, 0x9c, 0x2a, 0xf9, 0x4f, 0x17, 0x10,
            0x72, 0xf8,
        ];
        for rid in 1..=self.rows[0x37] {
            let row = self.row(0x37, rid).unwrap();
            let Some(parent) = self.index(row, self.parent_size) else {
                continue;
            };
            if parent & 31 != 0 || parent >> 5 != method {
                continue;
            }
            if self
                .index(row + self.parent_size, self.guid_size)
                .and_then(|i| self.guid(i))
                != Some(ASYNC_STEPPING)
            {
                continue;
            }
            let Some(data) = self
                .index(row + self.parent_size + self.guid_size, self.blob_size)
                .and_then(|i| self.blob(i))
            else {
                continue;
            };
            let mut at = 4;
            let mut offsets = Vec::new();
            while data.len().saturating_sub(at) >= 8 {
                let offset = data.u32(at).unwrap() as usize;
                at += 8;
                if compressed_integer(data, &mut at).is_none() {
                    break;
                }
                offsets.push(offset);
            }
            return offsets;
        }
        vec![]
    }
    pub fn await_locations(
        &self,
        method: usize,
        il: Option<&[u8]>,
    ) -> BTreeMap<i32, (String, i32)> {
        let points = self.sequence_points(method);
        let mut result = BTreeMap::new();
        for (k, offset) in self.yield_offsets(method).into_iter().enumerate() {
            if let Some(point) = points.iter().rev().find(|p| p.offset <= offset)
                && let Some(name) = self.document_name(point.document)
            {
                result.insert(
                    il.and_then(|il| state_before(offset, il))
                        .unwrap_or(k as i32),
                    (name, point.line),
                );
            }
        }
        result
    }
}

pub fn signed_integer(bytes: &[u8], at: &mut usize) -> Option<i32> {
    let first = *bytes.get(*at)?;
    let value = compressed_integer(bytes, at)? as i32;
    let bias = if first & 0x80 == 0 {
        0x40
    } else if first & 0xc0 == 0x80 {
        0x2000
    } else {
        0x1000_0000
    };
    Some(if value & 1 == 0 {
        value >> 1
    } else {
        (value >> 1) - bias
    })
}

fn state_before(offset: usize, il: &[u8]) -> Option<i32> {
    let mut at = 0;
    let mut last = std::collections::VecDeque::new();
    while at < offset {
        let i = instruction(il, &mut at)?;
        last.push_back(i);
        if last.len() > 4 {
            last.pop_front();
        }
    }
    if at != offset
        || last.len() != 4
        || last[3].opcode != 0x7d
        || !(0xa..=0xd).contains(&last[2].opcode) && last[2].opcode != 0x13
        || last[1].opcode != 0x25
    {
        return None;
    }
    match last[0].opcode {
        0x16..=0x1e => Some(last[0].opcode as i32 - 0x16),
        0x1f => Some(last[0].operand[0] as i8 as i32),
        0x20 => last[0].operand.u32(0).map(|n| n as i32),
        _ => None,
    }
}
