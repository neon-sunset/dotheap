use crate::{
    bytes::ReadBytes,
    core_dump::{CoreDump, Platform},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub start: u64,
    pub end: u64,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<ContractDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_dac_vars: Option<u64>,
}

impl RuntimeInfo {
    pub fn find(dump: &CoreDump) -> Option<Self> {
        let images = dump.library(if dump.platform == Platform::MacOSArm64 {
            "libcoreclr.dylib"
        } else {
            "/libcoreclr.so"
        });
        let first = *images.first()?;
        let start = first.start;
        let mut end = images.iter().map(|f| f.end).max()?;
        while let Some(i) = dump.segment_index(end) {
            if dump.file_mapping(end).is_some() || end - start >= 1 << 30 {
                break;
            }
            end = dump.segments[i].end();
        }
        let build_id = if dump.platform == Platform::MacOSArm64 {
            macho_uuid(dump, start)
        } else if first.file_offset == 0 {
            elf_build_id(dump, start)
        } else {
            None
        };
        let (mut version, mut descriptor, mut gc_dac_vars) = (None, None, None);
        for s in dump
            .segments
            .iter()
            .filter(|s| s.end() > start && s.address < end)
        {
            if version.is_none() {
                version = dump
                    .find_bytes(s.address, s.end(), b"@(#)Version ")
                    .and_then(|address| dump.c_string(address, 200));
            }
            dump.scan(s.address, s.end(), |a, bytes| {
                let first = (s.address.wrapping_sub(a) & 7) as usize;
                for i in (first..bytes.len()).step_by(8) {
                    let at = a + i as u64;
                    let word = |offset| {
                        bytes
                            .u64(i + offset)
                            .or_else(|| at.checked_add(offset as u64).and_then(|p| dump.u64(p)))
                    };
                    if descriptor.is_none() && word(0) == Some(0x0043_4144_4343_4e44) {
                        descriptor = ContractDescriptor::read(dump, at);
                    }
                    if gc_dac_vars.is_none()
                        && s.writable
                        && bytes.u8(i) == Some(2)
                        && bytes.u8(i + 1).or_else(|| dump.u8(at + 1)).unwrap_or(255) < 32
                        && word(16) == Some(5)
                        && word(8).is_some_and(|n| (64..=4096).contains(&n))
                        && word(24).is_some_and(|p| (start..end).contains(&p))
                        && word(32).is_some_and(|p| (start..end).contains(&p))
                    {
                        gc_dac_vars = Some(at);
                    }
                }
            });
        }
        Some(Self {
            start,
            end,
            path: first.path.clone(),
            build_id,
            version,
            descriptor,
            gc_dac_vars,
        })
    }

    pub fn threads(&self, dump: &CoreDump) -> Vec<(u64, i32)> {
        self.descriptor
            .as_ref()
            .map(|d| d.threads(dump))
            .unwrap_or_default()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn macho_uuid(dump: &CoreDump, base: u64) -> Option<String> {
    let count = dump.u32(base.checked_add(16)?)?.min(1 << 20);
    let mut at = base.checked_add(32)?;
    for _ in 0..count {
        let kind = dump.u32(at)?;
        let size = dump.u32(at.checked_add(4)?)? as u64;
        if size < 8 {
            return None;
        }
        if kind == 0x1b && size >= 24 {
            return Some(hex(&dump.bytes(at.checked_add(8)?, 16)?));
        }
        at = at.checked_add(size)?;
    }
    None
}

pub fn elf_build_id(dump: &CoreDump, base: u64) -> Option<String> {
    if dump.u32(base)? != 0x464c457f {
        return None;
    }
    let offset = dump.u64(base.checked_add(0x20)?)?;
    let size = dump.u16(base.checked_add(0x36)?)? as u64;
    let count = dump.u16(base.checked_add(0x38)?)? as u64;
    if size < 56 {
        return None;
    }
    for i in 0..count {
        let ph = base.checked_add(offset)?.checked_add(i * size)?;
        if dump.u32(ph) != Some(4) {
            continue;
        }
        let mut at = base.checked_add(dump.u64(ph.checked_add(16)?)?)?;
        let end = at.checked_add(dump.u64(ph.checked_add(32)?)?)?;
        while end.saturating_sub(at) >= 12 {
            let name_size = dump.u32(at)? as u64;
            let desc_size = dump.u32(at + 4)? as u64;
            let kind = dump.u32(at + 8)?;
            let desc = at.checked_add(12 + ((name_size + 3) & !3))?;
            let next = desc.checked_add((desc_size + 3) & !3)?;
            if next > end {
                break;
            }
            if kind == 3 && name_size >= 3 && dump.bytes(at + 12, 3).as_deref() == Some(b"GNU") {
                return Some(hex(&dump.bytes(desc, desc_size as usize)?));
            }
            at = next;
        }
    }
    None
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContractDescriptor {
    pub address: u64,
    pub types: BTreeMap<String, BTreeMap<String, u64>>,
    pub globals: BTreeMap<String, u64>,
    pub contracts: BTreeMap<String, u64>,
}

impl ContractDescriptor {
    pub fn read(dump: &CoreDump, address: u64) -> Option<Self> {
        let size = dump.u32(address.checked_add(12)?)? as usize;
        if size == 0 || size >= 1 << 24 {
            return None;
        }
        let json = dump.u64(address.checked_add(16)?)?;
        let count = dump.u32(address.checked_add(24)?)? as usize;
        let data = dump.u64(address.checked_add(32)?)?;
        if count > 1 << 20 {
            return None;
        }
        let text = dump.bytes(json, size)?;
        let text = &text[..memchr::memchr(0, &text).unwrap_or(text.len())];
        let root: Value = serde_json::from_slice(text).ok()?;
        let pointers: Vec<_> = (0..count)
            .map(|i| {
                data.checked_add(i as u64 * 8)
                    .and_then(|at| dump.u64(at))
                    .unwrap_or(0)
            })
            .collect();
        fn number(v: &Value, pointers: &[u64]) -> Option<u64> {
            match v {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => {
                    if let Some(hex) = s.strip_prefix("0x") {
                        u64::from_str_radix(hex, 16).ok()
                    } else {
                        s.parse().ok()
                    }
                }
                Value::Array(a) => number(a.first()?, &[])?
                    .try_into()
                    .ok()
                    .and_then(|i: usize| pointers.get(i).copied()),
                _ => None,
            }
        }
        let raw = |v: &Value| number(v.as_array().and_then(|a| a.first()).unwrap_or(v), &pointers);
        let mut types = BTreeMap::new();
        if let Some(values) = root.get("types").and_then(Value::as_object) {
            for (name, value) in values {
                let fields = value
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(key, v)| raw(v).map(|n| (key.clone(), n)))
                    .collect();
                types.insert(name.clone(), fields);
            }
        }
        let globals = root
            .get("globals")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| raw(v).map(|n| (k.clone(), n)))
            .collect();
        let contracts = root
            .get("contracts")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
            .collect();
        Some(Self {
            address,
            types,
            globals,
            contracts,
        })
    }
    pub fn offset(&self, kind: &str, field: &str) -> Option<u64> {
        self.types.get(kind)?.get(field).copied()
    }

    pub fn allocation_contexts(&self, dump: &CoreDump) -> Vec<(u64, i32)> {
        let Some(((locals, context), gc)) = self
            .offset("Thread", "RuntimeThreadLocals")
            .zip(self.offset("RuntimeThreadLocals", "AllocContext"))
            .zip(self.offset("EEAllocContext", "GCAllocationContext"))
        else {
            return Vec::new();
        };
        self.threads(dump)
            .into_iter()
            .filter_map(|(thread, tid)| {
                let locals = dump.u64(thread.checked_add(locals)?)?;
                if locals == 0 {
                    return None;
                }
                Some((locals.checked_add(context)?.checked_add(gc)?, tid))
            })
            .collect()
    }

    pub fn threads(&self, dump: &CoreDump) -> Vec<(u64, i32)> {
        let mut result = Vec::new();
        let Some((mut link, next_offset, id_offset)) = (|| {
            let store = dump.u64(*self.globals.get("ThreadStore")?)?;
            Some((
                dump.u64(store.checked_add(self.offset("ThreadStore", "FirstThreadLink")?)?)?,
                self.offset("Thread", "LinkNext")?,
                self.offset("Thread", "OSId")?,
            ))
        })() else {
            return result;
        };
        let mut seen = HashSet::new();
        while link != 0 && result.len() < 100_000 && seen.insert(link) {
            let Some(thread) = link.checked_sub(next_offset) else {
                break;
            };
            let id = thread
                .checked_add(id_offset)
                .and_then(|at| dump.u64(at))
                .unwrap_or(0);
            result.push((thread, i32::try_from(id).unwrap_or(0)));
            link = dump.u64(link).unwrap_or(0);
        }
        result
    }
}
