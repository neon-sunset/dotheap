use crate::{
    bytes::{ReadBytes, align_up},
    core_dump::{CoreDump, Cursor},
};
use std::collections::{BTreeSet, HashSet};

pub const SEGMENT_SIZE: u64 = 0x10000;
pub const TYPE_NAMES: &[&str] = &[
    "weak short",
    "weak long",
    "strong",
    "pinned",
    "variable",
    "ref counted",
    "dependent",
    "async pinned",
    "sized ref",
    "weak native COM",
    "weak interior pointer",
    "cross reference",
];
pub fn type_name(kind: u8) -> String {
    TYPE_NAMES
        .get(kind as usize)
        .map(|s| (*s).to_owned())
        .unwrap_or_else(|| format!("type {kind}"))
}
pub fn is_strong(kind: u8) -> bool {
    [2, 3, 4, 5, 7, 8, 11].contains(&kind)
}

pub struct Handle {
    pub slot: u64,
    pub kind: u8,
    pub target: u64,
    pub secondary: u64,
}
pub struct HandleTables {
    pub segments: Vec<u64>,
    pub handles: Vec<Handle>,
}

impl HandleTables {
    pub fn find(dump: &CoreDump, low: u64, high: u64) -> Self {
        let mut tables = BTreeSet::new();
        let mut cursor = Cursor::new(dump, true);
        for seg in dump
            .segments
            .iter()
            .filter(|s| s.writable && !(s.address < high && s.end() > low))
        {
            let Some(mut a) = align_up(seg.address, SEGMENT_SIZE) else {
                continue;
            };
            while a.checked_add(0x1000).is_some_and(|end| end <= seg.end()) {
                if let Some(table) = table_of_segment(dump, a, &mut cursor) {
                    tables.insert(table);
                }
                let Some(next) = a.checked_add(SEGMENT_SIZE) else {
                    break;
                };
                a = next;
            }
        }
        let (mut seen, mut result) = (
            HashSet::new(),
            Self {
                segments: vec![],
                handles: vec![],
            },
        );
        for table in tables {
            let mut s = table.checked_add(56).and_then(|p| dump.u64(p)).unwrap_or(0);
            while s != 0 && s & (SEGMENT_SIZE - 1) == 0 && seen.len() < 100_000 && seen.insert(s) {
                result.segments.push(s);
                result.read(dump, s);
                s = s.checked_add(1998).and_then(|p| dump.u64(p)).unwrap_or(0);
            }
        }
        result.segments.sort_unstable();
        result
    }
    pub fn contains(&self, address: u64) -> bool {
        self.segments
            .binary_search(&(address & !(SEGMENT_SIZE - 1)))
            .is_ok()
    }
    fn read(&mut self, dump: &CoreDump, address: u64) {
        let (Some(types), Some(data)) = (
            dump.bytes(address + 1560, 120),
            dump.bytes(address + 1680, 120),
        ) else {
            return;
        };
        let count = dump.u8(address + 2017).unwrap_or(0).min(120) as usize;
        for b in 0..count {
            if types[b] < 12 {
                let base = address + 0x1000 + (b * 64 * 8) as u64;
                let Some(slots) = dump.bytes(base, 64 * 8) else {
                    continue;
                };
                for k in 0..64 {
                    let value = slots.u64(k * 8).unwrap();
                    if value == 0 {
                        continue;
                    }
                    let secondary = if types[b] == 6 && data[b] < 120 {
                        dump.u64(address + 0x1000 + data[b] as u64 * 64 * 8 + k as u64 * 8)
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    self.handles.push(Handle {
                        slot: base + (k * 8) as u64,
                        kind: types[b],
                        target: value,
                        secondary,
                    });
                }
            }
        }
    }
}

fn table_of_segment(dump: &CoreDump, address: u64, cursor: &mut Cursor<'_>) -> Option<u64> {
    let table = cursor.u64(address + 2006)?;
    let commit = cursor.u8(address + 2017)?;
    if commit == 0 || commit > 120 || table == 0 || table & 7 != 0 {
        return None;
    }
    let types = cursor.slice(address + 1560, 120)?;
    if types.iter().any(|&t| t > 12 && t != 0xff) {
        return None;
    }
    let mut s = table.checked_add(56).and_then(|p| dump.u64(p)).unwrap_or(0);
    let mut seen = HashSet::new();
    while s != 0 && s & (SEGMENT_SIZE - 1) == 0 && seen.len() < 10_000 && seen.insert(s) {
        if s == address {
            return Some(table);
        }
        s = s.checked_add(1998).and_then(|p| dump.u64(p)).unwrap_or(0);
    }
    None
}
