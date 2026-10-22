use crate::{Result, analysis::HeapSnapshot, core_dump::Cursor};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateString {
    pub sample: usize,
    pub preview: String,
    pub count: usize,
    pub bytes_each: u64,
    pub samples: Vec<usize>,
}
impl DuplicateString {
    pub fn wasted(&self) -> u64 {
        (self.count - 1) as u64 * self.bytes_each
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionCapacity {
    #[serde(rename = "type")]
    pub kind: String,
    pub instances: usize,
    pub empty: usize,
    pub slots: usize,
    pub used: usize,
    pub storage_bytes: u64,
    pub unused_bytes: u64,
    pub samples: Vec<usize>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZeroArrays {
    pub type_id: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub arrays: usize,
    pub bytes: u64,
    pub zero_bytes: u64,
    pub samples: Vec<usize>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeCount {
    pub type_id: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub live: usize,
    pub dead: usize,
    pub bytes: u64,
    pub samples: Vec<usize>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finalizable {
    pub type_id: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub live_registered: usize,
    pub live_suppressed: usize,
    pub resurrected: usize,
    pub pending: usize,
    pub dead: usize,
    pub finalizer_can_re_register: bool,
    pub bytes: u64,
    pub samples: Vec<usize>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GarbageGroup {
    pub type_id: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub unreferenced: usize,
    pub retained: u64,
    pub samples: Vec<usize>,
}

impl HeapSnapshot {
    pub fn duplicate_strings(&self, limit: usize) -> Vec<DuplicateString> {
        let Some(kind) = self
            .heap
            .types
            .iter()
            .position(|t| self.names.name(t.address) == "System.String")
        else {
            return vec![];
        };
        let mut groups = HashMap::<u64, Vec<(usize, Vec<usize>)>>::new();
        let cursor = Cursor::new(&self.dump, true);
        for i in 0..self.heap.count() {
            if self.heap.type_ids[i] as usize != kind || self.graph.live[i] == 0 {
                continue;
            }
            let length = self.heap.lengths[i];
            let start = self.heap.addresses[i] + 12;
            let size = length as usize * 2;
            if !cursor.holds(start, size) {
                continue;
            }
            let mut h = 1_469_598_103_934_665_603u64 ^ length as u64;
            self.dump.scan(start, start + size as u64, |_, bytes| {
                for &b in bytes {
                    h = (h ^ b as u64).wrapping_mul(1_099_511_628_211);
                }
            });
            let bucket = groups.entry(h).or_default();
            let same = bucket.iter().position(|(_, samples)| {
                let other = samples[0];
                if self.heap.lengths[other] != length {
                    return false;
                }
                let other_start = self.heap.addresses[other] + 12;
                let mut equal = true;
                self.dump.scan(start, start + size as u64, |a, bytes| {
                    if equal {
                        equal = self
                            .dump
                            .bytes(other_start + a - start, bytes.len())
                            .is_some_and(|b| &*b == bytes);
                    }
                });
                equal
            });
            if let Some(k) = same {
                bucket[k].0 += 1;
                if bucket[k].1.len() < 200 {
                    bucket[k].1.push(i);
                }
            } else {
                bucket.push((1, vec![i]));
            }
        }
        let mut list: Vec<_> = groups
            .into_values()
            .flatten()
            .filter(|(count, _)| *count > 1)
            .map(|(count, samples)| {
                let sample = samples[0];
                DuplicateString {
                    sample,
                    preview: self.string_value(sample, 200).unwrap_or_default(),
                    count,
                    bytes_each: self.heap.size(sample),
                    samples,
                }
            })
            .collect();
        list.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.sample.cmp(&b.sample)));
        list.truncate(limit);
        list
    }
    pub fn collection_capacity(&self) -> Vec<CollectionCapacity> {
        let kinds = [
            (
                "System.Collections.Generic.List`1<",
                "_items",
                "_size",
                None,
            ),
            (
                "System.Collections.Generic.Dictionary`2<",
                "_entries",
                "_count",
                Some("_freeCount"),
            ),
            (
                "System.Collections.Generic.HashSet`1<",
                "_entries",
                "_count",
                Some("_freeCount"),
            ),
            (
                "System.Collections.Generic.Queue`1<",
                "_array",
                "_size",
                None,
            ),
            (
                "System.Collections.Generic.Stack`1<",
                "_array",
                "_size",
                None,
            ),
        ];
        let mut result = BTreeMap::<String, CollectionCapacity>::new();
        let mut unused = BTreeMap::<String, Vec<(usize, u64)>>::new();
        for (t, info) in self.heap.types.iter().enumerate() {
            let name = self.names.name(info.address);
            let Some(kind) = kinds.iter().find(|k| name.starts_with(k.0)) else {
                continue;
            };
            let fields = self.names.fields(info.address);
            let (Some(array), Some(count)) = (
                fields.iter().find(|f| f.name == kind.1),
                fields.iter().find(|f| f.name == kind.2),
            ) else {
                continue;
            };
            let free = kind.3.and_then(|n| fields.iter().find(|f| f.name == n));
            let label = name
                .split('<')
                .next()
                .unwrap_or(&name)
                .replace("System.Collections.Generic.", "");
            let stats = result
                .entry(label.clone())
                .or_insert_with(|| CollectionCapacity {
                    kind: label.clone(),
                    ..Default::default()
                });
            let mut cursor = Cursor::new(&self.dump, true);
            for i in 0..self.heap.count() {
                if self.heap.type_ids[i] as usize != t || self.graph.live[i] == 0 {
                    continue;
                }
                let a = self.heap.addresses[i];
                stats.instances += 1;
                let count = (cursor.u32(a + count.offset as u64).unwrap_or(0) as i32 as i64
                    - free
                        .and_then(|f| cursor.u32(a + f.offset as u64))
                        .unwrap_or(0) as i32 as i64)
                    .max(0) as usize;
                let Some(j) = cursor
                    .u64(a + array.offset as u64)
                    .and_then(|a| self.heap.object_at(a))
                else {
                    stats.empty += 1;
                    continue;
                };
                let slots = self.component_count(j).unwrap_or(0);
                let waste = slots.saturating_sub(count) as u64
                    * self.heap.method_table(j).component_size() as u64;
                stats.slots += slots;
                stats.used += count;
                stats.storage_bytes += self.heap.size(j);
                stats.unused_bytes += waste;
                if count == 0 {
                    stats.empty += 1;
                }
                unused.entry(label.clone()).or_default().push((i, waste));
            }
        }
        for (label, mut members) in unused {
            members.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            result.get_mut(&label).unwrap().samples =
                members.into_iter().take(300).map(|p| p.0).collect();
        }
        let mut list: Vec<_> = result.into_values().filter(|s| s.instances > 0).collect();
        list.sort_by(|a, b| {
            b.unused_bytes
                .cmp(&a.unused_bytes)
                .then(a.kind.cmp(&b.kind))
        });
        list
    }
    pub fn mostly_zero_arrays(&self) -> Vec<ZeroArrays> {
        let mut result = BTreeMap::new();
        let cursor = Cursor::new(&self.dump, true);
        for i in 0..self.heap.count() {
            if self.graph.live[i] == 0 {
                continue;
            }
            let mt = self.heap.method_table(i);
            if mt.component_size() == 0 || mt.contains_pointers() || self.heap.size(i) < 1024 {
                continue;
            }
            let bytes = self.heap.lengths[i] as u64 * mt.component_size() as u64;
            let offset = if mt.flags & 0xf0000 == 0xa0000 {
                16
            } else {
                12
            };
            let start = self.heap.addresses[i] + offset;
            if bytes == 0
                || bytes > self.heap.size(i).saturating_sub(offset)
                || !cursor.holds(start, bytes as usize)
            {
                continue;
            }
            let mut zero = 0u64;
            self.dump.scan(start, start + bytes, |_, b| {
                zero += b.iter().filter(|&&v| v == 0).count() as u64
            });
            if (zero as u128) * 10 < (bytes as u128) * 9 {
                continue;
            }
            let t = self.heap.type_ids[i];
            let e = result.entry(t).or_insert_with(|| ZeroArrays {
                type_id: t,
                kind: self.type_name(i),
                ..Default::default()
            });
            e.arrays += 1;
            e.bytes += self.heap.size(i);
            e.zero_bytes += zero;
            e.samples.push(i);
        }
        let mut list: Vec<ZeroArrays> = result.into_values().collect();
        for e in &mut list {
            e.samples
                .sort_by(|&a, &b| self.heap.size(b).cmp(&self.heap.size(a)).then(a.cmp(&b)));
            e.samples.truncate(300);
        }
        list.sort_by(|a, b| b.zero_bytes.cmp(&a.zero_bytes).then(a.kind.cmp(&b.kind)));
        list
    }
    pub fn boxed_values(&self) -> Vec<TypeCount> {
        let selected: HashSet<_> = self
            .heap
            .types
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.component_size() == 0 && [0x60000, 0x70000].contains(&(t.flags & 0xf0000))
            })
            .map(|(i, _)| i as u32)
            .collect();
        let mut result = BTreeMap::new();
        for i in 0..self.heap.count() {
            let t = self.heap.type_ids[i];
            if !selected.contains(&t) {
                continue;
            }
            let e = result.entry(t).or_insert_with(|| TypeCount {
                type_id: t,
                kind: self.type_name(i),
                ..Default::default()
            });
            if self.graph.live[i] != 0 {
                e.live += 1;
            } else {
                e.dead += 1;
            }
            e.bytes += self.heap.size(i);
            if e.samples.len() < 300 {
                e.samples.push(i);
            }
        }
        let mut list: Vec<TypeCount> = result.into_values().collect();
        list.sort_by(|a, b| {
            (b.live + b.dead)
                .cmp(&(a.live + a.dead))
                .then(a.kind.cmp(&b.kind))
        });
        list
    }
    pub fn finalizable_objects(&self) -> Vec<Finalizable> {
        let selected: HashSet<_> = self
            .heap
            .types
            .iter()
            .enumerate()
            .filter(|(_, t)| t.component_size() == 0 && t.flags & 0x0010_0000 != 0)
            .map(|(i, _)| i as u32)
            .collect();
        let mut registered = HashSet::new();
        for q in &self.heap.finalization {
            for slot in q.registered_slots().chain(q.ready_slots()) {
                if let Some(i) = self.dump.u64(slot).and_then(|a| self.heap.object_at(a)) {
                    registered.insert(i);
                }
            }
        }
        let mut result = BTreeMap::new();
        let mut samples = BTreeMap::<u32, [Vec<usize>; 3]>::new();
        let mut cursor = Cursor::new(&self.dump, true);
        for i in 0..self.heap.count() {
            let t = self.heap.type_ids[i];
            if !selected.contains(&t) {
                continue;
            }
            let e = result.entry(t).or_insert_with(|| Finalizable {
                type_id: t,
                kind: self.type_name(i),
                finalizer_can_re_register: self
                    .names
                    .finalizer_re_registers(self.heap.types[t as usize].address),
                ..Default::default()
            });
            if self.graph.live[i] == 0 {
                e.dead += 1;
            } else if self.graph.pending[i] != 0 {
                e.pending += 1;
                samples.entry(t).or_default()[0].push(i);
            } else {
                let suppressed = cursor
                    .u32(self.heap.addresses[i].saturating_sub(4))
                    .unwrap_or(0)
                    & 0x4000_0000
                    != 0;
                if registered.contains(&i) && !suppressed {
                    e.live_registered += 1;
                    samples.entry(t).or_default()[2].push(i);
                } else if suppressed || self.heap.regions[self.heap.region_of(i)].immortal {
                    e.live_suppressed += 1;
                } else {
                    e.resurrected += 1;
                    samples.entry(t).or_default()[1].push(i);
                }
            }
            e.bytes += self.heap.size(i);
        }
        for (t, groups) in samples {
            result.get_mut(&t).unwrap().samples = groups.into_iter().flatten().take(300).collect();
        }
        let mut list: Vec<Finalizable> = result.into_values().collect();
        list.sort_by(|a, b| {
            b.pending
                .cmp(&a.pending)
                .then(b.resurrected.cmp(&a.resurrected))
                .then(b.live_registered.cmp(&a.live_registered))
                .then(a.kind.cmp(&b.kind))
                .then(a.type_id.cmp(&b.type_id))
        });
        list
    }
    pub fn garbage_by_type(&self) -> Result<Vec<GarbageGroup>> {
        let g = self.garbage()?;
        let mut result = BTreeMap::new();
        for &i in &g.unreferenced {
            let i = i as usize;
            let t = self.heap.type_ids[i];
            let e = result.entry(t).or_insert_with(|| GarbageGroup {
                type_id: t,
                kind: self.type_name(i),
                ..Default::default()
            });
            e.unreferenced += 1;
            e.retained += g.retained[i];
            e.samples.push(i);
        }
        let mut list: Vec<GarbageGroup> = result.into_values().collect();
        for e in &mut list {
            e.samples
                .sort_by(|&a, &b| g.retained[b].cmp(&g.retained[a]).then(a.cmp(&b)));
            e.samples.truncate(300);
        }
        list.sort_by(|a, b| {
            b.retained
                .cmp(&a.retained)
                .then(a.kind.cmp(&b.kind))
                .then(a.type_id.cmp(&b.type_id))
        });
        Ok(list)
    }
}
