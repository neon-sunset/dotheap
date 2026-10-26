use crate::{
    Result,
    analysis::HeapSnapshot,
    bytes::ReadBytes,
    heap::{GRANULE, Gc},
    type_names::{self, FieldInfo},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TypeGroup {
    pub name: String,
    pub objects: usize,
    pub bytes: u64,
    pub samples: Vec<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandleGroup {
    pub handle_type: String,
    pub target_type: String,
    pub count: usize,
    pub live_targets: usize,
    pub target_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Word {
    pub offset: usize,
    pub value: u64,
    pub target: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldValue {
    pub path: String,
    pub field: FieldInfo,
    pub address: u64,
    pub display: String,
    pub target: Option<usize>,
    pub children: Vec<FieldValue>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructElement {
    pub index: usize,
    pub fields: Vec<FieldValue>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReferenceElement {
    pub index: usize,
    pub target: Option<usize>,
    pub value: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextPreview {
    pub kind: String,
    pub text: Option<String>,
    pub hex: Vec<String>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DominatorGroup {
    pub name: String,
    pub nodes: Vec<usize>,
    pub retained: u64,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(C)]
pub struct Granule {
    pub region: i32,
    pub live: u32,
    pub pending: u32,
    pub dead: u32,
    pub header: u32,
    pub used: u32,
    pub committed: u32,
    pub gc_storage: u32,
    pub is_region_start: bool,
}
impl Default for Granule {
    fn default() -> Self {
        Self {
            region: -1,
            live: 0,
            pending: 0,
            dead: 0,
            header: 0,
            used: 0,
            committed: 0,
            gc_storage: 0,
            is_region_start: false,
        }
    }
}
impl Granule {
    pub fn gaps(&self) -> u32 {
        self.used
            .saturating_sub(self.header + self.live + self.pending + self.dead + self.gc_storage)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegionStrip {
    pub live: Vec<f64>,
    pub pending: Vec<f64>,
    pub dead: Vec<f64>,
}

pub fn hex(v: u64) -> String {
    format!("0x{v:x}")
}
pub fn sort_groups(groups: impl IntoIterator<Item = TypeGroup>) -> Vec<TypeGroup> {
    let mut list: Vec<_> = groups.into_iter().collect();
    list.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
    list
}

impl HeapSnapshot {
    pub fn type_name(&self, i: usize) -> String {
        self.names.name(self.heap.method_table(i).address)
    }
    pub fn instances(&self, type_id: u32, limit: usize) -> Vec<usize> {
        let mut list: Vec<_> = (0..self.heap.count())
            .filter(|&i| self.heap.type_ids[i] == type_id)
            .collect();
        list.sort_by(|&a, &b| {
            self.graph.retained[b]
                .cmp(&self.graph.retained[a])
                .then(self.heap.size(b).cmp(&self.heap.size(a)))
                .then(a.cmp(&b))
        });
        list.truncate(limit);
        list
    }
    pub fn referrer_types(&self, type_id: u32) -> Result<Vec<TypeGroup>> {
        let mut groups = HashMap::<String, TypeGroup>::new();
        for i in (0..self.heap.count())
            .filter(|&i| self.heap.type_ids[i] == type_id && self.graph.live[i] != 0)
        {
            let mut seen = HashSet::new();
            for &r in self.graph.referrer_rows.row(i) {
                let name = self.type_name(r as usize);
                if !seen.insert(name.clone()) {
                    continue;
                }
                let g = groups.entry(name.clone()).or_insert_with(|| TypeGroup {
                    name,
                    ..Default::default()
                });
                g.objects += 1;
                g.bytes += self.heap.size(i);
                if g.samples.len() < 300 {
                    g.samples.push(r as usize);
                }
            }
            for &s in self.graph.sources_of.row(i) {
                let name = format!("root: {}", self.describe_source(s as usize)?);
                if !seen.insert(name.clone()) {
                    continue;
                }
                let g = groups.entry(name.clone()).or_insert_with(|| TypeGroup {
                    name,
                    ..Default::default()
                });
                g.objects += 1;
                g.bytes += self.heap.size(i);
            }
        }
        Ok(sort_groups(groups.into_values()))
    }
    pub fn dominator_types(&self, type_id: u32) -> Result<Vec<TypeGroup>> {
        let mut groups = HashMap::<String, TypeGroup>::new();
        for i in (0..self.heap.count())
            .filter(|&i| self.heap.type_ids[i] == type_id && self.graph.live[i] != 0)
        {
            let d = self.graph.idom[i];
            if d < 0 {
                continue;
            }
            let d = d as usize;
            let name = self.node_name(d)?;
            let g = groups.entry(name.clone()).or_insert_with(|| TypeGroup {
                name,
                ..Default::default()
            });
            g.objects += 1;
            g.bytes += self.graph.retained[i];
            if d < self.heap.count() && g.samples.len() < 300 && !g.samples.contains(&d) {
                g.samples.push(d);
            }
        }
        Ok(sort_groups(groups.into_values()))
    }
    pub fn handle_groups(&self) -> Vec<HandleGroup> {
        let mut groups = BTreeMap::new();
        for h in &self.graph.handles {
            let handle_type = crate::handles::type_name(h.kind);
            let target_type = h
                .target
                .map(|i| self.type_name(i))
                .unwrap_or("(not a heap object)".into());
            let key = format!("{handle_type}|{target_type}");
            let g = groups.entry(key).or_insert(HandleGroup {
                handle_type,
                target_type,
                count: 0,
                live_targets: 0,
                target_bytes: 0,
            });
            g.count += 1;
            if let Some(i) = h.target {
                g.target_bytes += self.heap.size(i);
                g.live_targets += usize::from(self.graph.live[i] != 0);
            }
        }
        let mut list: Vec<_> = groups.into_iter().collect();
        list.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(&b.0)));
        list.into_iter().map(|(_, g)| g).collect()
    }
    pub fn words(&self, i: usize, limit: usize) -> Vec<Word> {
        (0..limit.min((self.heap.size(i) / 8) as usize))
            .map(|k| {
                let value = self
                    .dump
                    .u64(self.heap.addresses[i] + k as u64 * 8)
                    .unwrap_or(0);
                let target = if k > 0 && value & 7 == 0 {
                    self.heap.object_at(value)
                } else {
                    None
                };
                Word {
                    offset: k * 8,
                    value,
                    target,
                }
            })
            .collect()
    }
    pub fn field_values(&self, i: usize) -> Vec<FieldValue> {
        self.value_fields(
            self.heap.method_table(i).address,
            self.heap.addresses[i],
            true,
            self.heap.addresses[i] + self.heap.size(i),
            "",
            0,
        )
    }
    pub fn value_fields(
        &self,
        mt: u64,
        base: u64,
        boxed: bool,
        end: u64,
        path: &str,
        depth: usize,
    ) -> Vec<FieldValue> {
        let fields = self.names.fields(mt);
        let shift = if boxed { 0 } else { -8 };
        fields
            .iter()
            .enumerate()
            .filter_map(|(k, f)| {
                let a = base.checked_add_signed(f.offset as i64 + shift)?;
                let p = format!("{path}.{}", f.name);
                let (display, target, children) = if a >= end {
                    ("(outside the object)".into(), None, vec![])
                } else if f.is_reference() {
                    match self
                        .dump
                        .u64(a)
                        .filter(|_| a.checked_add(8).is_some_and(|e| e <= end))
                    {
                        Some(0) => ("null".into(), None, vec![]),
                        Some(v) => {
                            let t = self.heap.object_at(v);
                            (
                                t.map(|i| type_names::short(&self.type_name(i)))
                                    .unwrap_or_else(|| hex(v)),
                                t,
                                vec![],
                            )
                        }
                        None => ("?".into(), None, vec![]),
                    }
                } else if let Some(size) = f.primitive_size() {
                    (
                        a.checked_add(size as u64)
                            .filter(|&e| e <= end)
                            .and_then(|_| self.dump.bytes(a, size))
                            .map(|b| primitive(f.element_type, &b))
                            .unwrap_or("?".into()),
                        None,
                        vec![],
                    )
                } else if let Some(vt) = f.value_type.filter(|_| depth < 6) {
                    (
                        type_names::short(&self.names.name(vt)),
                        None,
                        self.value_fields(vt, a, false, end, &p, depth + 1),
                    )
                } else {
                    let next = fields
                        .get(k + 1)
                        .and_then(|f| base.checked_add_signed(f.offset as i64 + shift))
                        .unwrap_or(end)
                        .min(end);
                    let n = next.saturating_sub(a).min(64) as usize;
                    (
                        self.dump
                            .bytes(a, n)
                            .map(|b| {
                                b.iter()
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or("?".into()),
                        None,
                        vec![],
                    )
                };
                Some(FieldValue {
                    path: p,
                    field: f.clone(),
                    address: a,
                    display,
                    target,
                    children,
                })
            })
            .collect()
    }
    pub fn struct_elements(&self, i: usize, limit: usize) -> Option<Vec<StructElement>> {
        let mt = self.heap.method_table(i);
        if mt.flags & 0xf0000 != 0xa0000 || mt.component_size() == 0 {
            return None;
        }
        let element = self.dump.u64(mt.address + 48)?;
        if element & 1 != 0
            || !self.names.is_struct(element)
            || self.names.fields(element).is_empty()
        {
            return None;
        }
        let stride = mt.component_size() as u64;
        let data = self.heap.addresses[i] + 16;
        let count = limit
            .min(self.heap.lengths[i] as usize)
            .min(self.heap.size(i).saturating_sub(16).checked_div(stride)? as usize);
        Some(
            (0..count)
                .map(|index| {
                    let base = data + index as u64 * stride;
                    StructElement {
                        index,
                        fields: self.value_fields(
                            element,
                            base,
                            false,
                            base + stride,
                            &format!("[{index}]"),
                            1,
                        ),
                    }
                })
                .collect(),
        )
    }
    pub fn reference_elements(&self, i: usize, limit: usize) -> Option<Vec<ReferenceElement>> {
        let mt = self.heap.method_table(i);
        if mt.flags & 0xf0000 != 0xa0000 || mt.component_size() != 8 {
            return None;
        }
        let element = self.dump.u64(mt.address + 48)?;
        if element & 1 != 0 {
            return None;
        }
        let category = self.dump.u32(element)? & 0xf0000;
        if (0x40000..=0x70000).contains(&category) {
            return None;
        }
        let count = limit
            .min(self.heap.lengths[i] as usize)
            .min((self.heap.size(i).saturating_sub(16) / 8) as usize);
        Some(
            (0..count)
                .map(|index| {
                    let value = self
                        .dump
                        .u64(self.heap.addresses[i] + 16 + index as u64 * 8)
                        .unwrap_or(0);
                    ReferenceElement {
                        index,
                        value,
                        target: self.heap.object_at(value),
                    }
                })
                .collect(),
        )
    }
    pub fn static_field_slots(&self) -> Result<Arc<HashMap<u64, String>>> {
        self.memo("staticFieldSlots", || {
            Ok(self
                .names
                .static_field_slots(self.runtime.as_ref().and_then(|r| r.descriptor.as_ref())))
        })
    }
    pub fn statics_arrays(&self) -> Result<Arc<HashSet<usize>>> {
        self.memo("staticsArrays", || {
            Ok(self
                .static_field_slots()?
                .keys()
                .filter_map(|&a| self.heap.object_containing(a))
                .collect())
        })
    }
    pub fn statics_arrays_with_progress(
        &self,
        progress: impl Fn(f64) + Sync,
    ) -> Result<Arc<HashSet<usize>>> {
        let update = |_: &str, fraction| progress(fraction);
        let progress = crate::progress::Progress::new(&update);
        progress.report(0.0);
        self.memo("staticFieldSlots", || {
            Ok(self.names.static_field_slots_with_progress(
                self.runtime.as_ref().and_then(|r| r.descriptor.as_ref()),
                progress,
            ))
        })?;
        let arrays = self.statics_arrays()?;
        progress.report(1.0);
        Ok(arrays)
    }
    pub fn reference_name(&self, parent: usize, child: usize) -> Result<Option<String>> {
        let start = self.heap.addresses[parent];
        let size = self.heap.size(parent);
        let target = self.heap.addresses[child];
        let mut cursor = crate::core_dump::Cursor::new(&self.dump, true);
        let slot = (8..size.saturating_sub(7))
            .step_by(8)
            .find(|&offset| cursor.u64(start + offset) == Some(target))
            .map(|o| start + o);
        let Some(slot) = slot else {
            return Ok(None);
        };
        if let Some(name) = self.static_field_slots()?.get(&slot) {
            return Ok(Some(format!("static {name}")));
        }
        fn find(values: &[FieldValue], slot: u64, end: u64) -> Option<String> {
            for (i, v) in values.iter().enumerate() {
                if v.field.is_reference() && v.address == slot {
                    return Some(v.path.clone());
                }
                if let Some(p) = find(&v.children, slot, end) {
                    return Some(p);
                }
                if v.children.is_empty()
                    && !v.field.is_reference()
                    && v.field.primitive_size().is_none()
                    && slot >= v.address
                {
                    let next = values[i + 1..]
                        .iter()
                        .map(|v| v.address)
                        .filter(|&a| a > v.address)
                        .min()
                        .unwrap_or(end);
                    if slot < next {
                        return Some(if slot == v.address {
                            v.path.clone()
                        } else {
                            format!("{} +{}", v.path, slot - v.address)
                        });
                    }
                }
            }
            None
        }
        let mt = self.heap.method_table(parent);
        if mt.flags & 0xf0000 != 0xa0000 || mt.component_size() == 0 {
            return Ok(find(&self.field_values(parent), slot, start + size));
        }
        let data = start + 16;
        let stride = mt.component_size() as u64;
        if slot < data {
            return Ok(None);
        }
        let k = (slot - data) / stride;
        let path = format!("[{k}]");
        let Some(element) = self
            .dump
            .u64(mt.address + 48)
            .filter(|&e| e & 1 == 0 && self.names.is_struct(e))
        else {
            return Ok(Some(path));
        };
        let base = data + k * stride;
        Ok(Some(
            find(
                &self.value_fields(element, base, false, base + stride, &path, 1),
                slot,
                start + size,
            )
            .unwrap_or(path),
        ))
    }
    pub fn string_value(&self, i: usize, limit: usize) -> Option<String> {
        if self.type_name(i) != "System.String" {
            return None;
        }
        let length = self.dump.u32(self.heap.addresses[i] + 8)? as usize;
        let n = length
            .min(limit)
            .min((self.heap.size(i).saturating_sub(12) / 2) as usize);
        let bytes = self.dump.bytes(self.heap.addresses[i] + 12, n * 2)?;
        let mut value = utf16(&bytes);
        if n < length {
            value.push('…');
        }
        Some(value)
    }
    pub fn component_count(&self, i: usize) -> Option<usize> {
        (self.heap.method_table(i).component_size() > 0).then_some(self.heap.lengths[i] as usize)
    }
    pub fn text_preview(&self, i: usize, limit: usize) -> Option<TextPreview> {
        let name = self.type_name(i);
        if !["System.String", "System.Char[]", "System.Byte[]"].contains(&name.as_str()) {
            return None;
        }
        let length = self.dump.u32(self.heap.addresses[i] + 8)? as usize;
        let offset = if name == "System.String" { 12 } else { 16 };
        let stride = if name == "System.Byte[]" { 1 } else { 2 };
        let n = length
            .min(limit)
            .min((self.heap.size(i).saturating_sub(offset) / stride) as usize);
        let bytes = self
            .dump
            .bytes(self.heap.addresses[i] + offset, n * stride as usize)?;
        let truncated = n < length;
        if stride == 2 {
            return Some(TextPreview {
                kind: if name == "System.String" {
                    "string"
                } else {
                    "UTF-16 characters"
                }
                .into(),
                text: Some(utf16(&bytes)),
                hex: vec![],
                truncated,
            });
        }
        if let Some(kind) = signature(&bytes) {
            return Some(TextPreview {
                kind: kind.into(),
                text: None,
                hex: hex_dump(&bytes, 32),
                truncated,
            });
        }
        let (text, valid, ascii) = decode_leniently(&bytes);
        if !bytes.is_empty() && valid as f64 / bytes.len() as f64 >= 0.9 {
            return Some(TextPreview {
                kind: if ascii == bytes.len() {
                    "ASCII text".into()
                } else {
                    format!("UTF-8 text ({}% valid)", 100 * valid / bytes.len())
                },
                text: Some(text),
                hex: vec![],
                truncated,
            });
        }
        Some(TextPreview {
            kind: if bytes.iter().all(|&b| b == 0) {
                "all zero".into()
            } else {
                format!("binary ({}% text bytes)", 100 * valid / bytes.len().max(1))
            },
            text: None,
            hex: hex_dump(&bytes, 32),
            truncated,
        })
    }
    pub fn identifying_text(&self, i: usize) -> Option<String> {
        if let Some(text) = self.string_value(i, 80) {
            return Some(text);
        }
        let fields = self.field_values(i);
        let strings: Vec<_> = fields
            .iter()
            .filter(|f| {
                f.target
                    .is_some_and(|t| self.type_name(t) == "System.String")
            })
            .collect();
        let pick = strings
            .iter()
            .find(|f| {
                ["pattern", "name", "key", "id", "path", "host", "uri", "url"].contains(
                    &f.field
                        .name
                        .trim_matches(['_', '<', '>'])
                        .to_lowercase()
                        .as_str(),
                )
            })
            .or_else(|| strings.first())?;
        let text = self.string_value(pick.target?, 80)?;
        (!text.is_empty()).then(|| text.replace('\n', " "))
    }
    pub fn node_name(&self, node: usize) -> Result<String> {
        if node == self.graph.root_node() {
            return Ok("roots".into());
        }
        if node >= self.heap.count() {
            return self.describe_source(node - self.heap.count());
        }
        let name = self.type_name(node);
        Ok(if self.statics_arrays()?.contains(&node) {
            format!("static fields ({name})")
        } else {
            name
        })
    }
    pub fn dominated_children(&self, node: usize) -> Result<Arc<Vec<usize>>> {
        self.memo(format!("dominator children {node}"), || {
            Ok(self.graph.dominated_children(node))
        })
    }
    pub fn dominated_groups(&self, node: usize) -> Result<Arc<Vec<DominatorGroup>>> {
        self.memo(format!("dominator groups {node}"), || {
            let mut groups = BTreeMap::<String, Vec<usize>>::new();
            for &n in self.dominated_children(node)?.iter() {
                groups.entry(self.node_name(n)?).or_default().push(n);
            }
            let mut list: Vec<_> = groups
                .into_iter()
                .map(|(name, nodes)| DominatorGroup {
                    name,
                    retained: nodes.iter().map(|&i| self.graph.retained[i]).sum(),
                    size: nodes
                        .iter()
                        .filter(|&&i| i < self.heap.count())
                        .map(|&i| self.heap.size(i))
                        .sum(),
                    nodes,
                })
                .collect();
            list.sort_by(|a, b| b.retained.cmp(&a.retained).then(a.name.cmp(&b.name)));
            Ok(list)
        })
    }
    pub fn granule_map(&self) -> Vec<Vec<Granule>> {
        let mut pages: Vec<Vec<Granule>> = self
            .heap
            .pages
            .iter()
            .map(|p| vec![Granule::default(); ((p.end - p.start) / GRANULE) as usize])
            .collect();
        let clip = |lo: u64, hi: u64, a: u64, b: u64| hi.min(b).saturating_sub(lo.max(a)) as u32;
        for r in &self.heap.regions {
            let Some(p) = self
                .heap
                .pages
                .iter()
                .position(|p| r.start >= p.start && r.start < p.end)
            else {
                continue;
            };
            let base = self.heap.pages[p].start;
            let first = ((r.start - base) / GRANULE) as usize;
            let end = ((r.end - base).div_ceil(GRANULE) as usize).min(pages[p].len());
            for (k, granule) in pages[p].iter_mut().enumerate().take(end).skip(first) {
                let lo = r.start.max(base + k as u64 * GRANULE);
                let hi = base + (k + 1) as u64 * GRANULE;
                *granule = Granule {
                    region: r.index as i32,
                    is_region_start: k == first,
                    header: if k == first {
                        r.header_bytes().min(GRANULE) as u32
                    } else {
                        0
                    },
                    used: clip(lo, hi, r.start, r.used.min(r.end)),
                    committed: clip(lo, hi, r.start, r.committed.min(r.end)),
                    gc_storage: if r.gc_storage.is_some() {
                        clip(lo, hi, r.object_start, r.committed.min(r.end))
                    } else {
                        0
                    },
                    ..Default::default()
                };
            }
            for i in r.first_object..r.end_object {
                let mut lo = self.heap.addresses[i];
                let end = (lo + self.heap.size(i)).min(r.end);
                while lo < end {
                    let k = ((lo - base) / GRANULE) as usize;
                    let hi = end.min(base + (k + 1) as u64 * GRANULE);
                    if let Some(cell) = pages[p].get_mut(k) {
                        let amount = (hi - lo) as u32;
                        if self.graph.live[i] == 0 {
                            cell.dead += amount;
                        } else if self.graph.pending[i] != 0 {
                            cell.pending += amount;
                        } else {
                            cell.live += amount;
                        }
                    }
                    lo = hi;
                }
            }
        }
        pages
    }
    pub fn region_strip(&self, region: usize, buckets: usize) -> RegionStrip {
        let r = &self.heap.regions[region];
        let n = buckets.max(1);
        let mut out = RegionStrip {
            live: vec![0.0; n],
            pending: vec![0.0; n],
            dead: vec![0.0; n],
        };
        let edge = |b: usize| (r.size() as u128 * b as u128 / n as u128) as u64;
        for i in r.first_object..r.end_object {
            let mut lo = self.heap.addresses[i] - r.start;
            let end = (lo + self.heap.size(i)).min(r.size());
            let mut b = (lo as u128 * n as u128 / r.size() as u128) as usize;
            while lo < end {
                while edge(b + 1) <= lo {
                    b += 1;
                }
                let hi = end.min(edge(b + 1));
                let list = if self.graph.live[i] == 0 {
                    &mut out.dead
                } else if self.graph.pending[i] != 0 {
                    &mut out.pending
                } else {
                    &mut out.live
                };
                list[b] += (hi - lo) as f64;
                lo = hi;
            }
        }
        for b in 0..n {
            let width = edge(b + 1) - edge(b);
            if width > 0 {
                out.live[b] /= width as f64;
                out.pending[b] /= width as f64;
                out.dead[b] /= width as f64;
            }
        }
        out
    }
    pub fn region_types(&self, region: usize) -> Vec<TypeGroup> {
        let r = &self.heap.regions[region];
        let mut groups = BTreeMap::new();
        for i in r.first_object..r.end_object {
            let g = groups
                .entry(self.heap.type_ids[i])
                .or_insert_with(|| TypeGroup {
                    name: self.type_name(i),
                    ..Default::default()
                });
            g.objects += 1;
            g.bytes += self.heap.size(i);
        }
        sort_groups(groups.into_values())
    }
    pub fn satori_region_kinds(&self) -> Result<Arc<Vec<String>>> {
        self.memo("satori region kinds", || {
            if self.heap.kind.gc != Gc::Satori {
                return Ok(vec![]);
            }
            Ok(self
                .heap
                .regions
                .iter()
                .map(|r| {
                    if let Some(storage) = &r.gc_storage {
                        return storage.clone();
                    }
                    if r.generation < 0 && r.queue == "Allocator" {
                        "free"
                    } else if r.generation < 0 {
                        "unassigned"
                    } else if r.size() > GRANULE {
                        "huge"
                    } else if (r.first_object..r.end_object).any(|i| {
                        self.heap.size(i) >= 32 * 1024
                            && !self.names.is_free(self.heap.method_table(i).address)
                    }) {
                        "large objects"
                    } else {
                        "regular"
                    }
                    .into()
                })
                .collect())
        })
    }
}

pub fn utf16(bytes: &[u8]) -> String {
    String::from_utf16_lossy(
        &bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>(),
    )
}
pub fn primitive(kind: u8, bytes: &[u8]) -> String {
    match kind {
        2 => if bytes[0] != 0 { "true" } else { "false" }.into(),
        3 => {
            let v = bytes.u16(0).unwrap();
            format!("'{}' ({v})", char::from_u32(v as u32).unwrap_or('?'))
        }
        4 => (bytes[0] as i8).to_string(),
        5 => bytes[0].to_string(),
        6 => (bytes.u16(0).unwrap() as i16).to_string(),
        7 => bytes.u16(0).unwrap().to_string(),
        8 => (bytes.u32(0).unwrap() as i32).to_string(),
        9 => bytes.u32(0).unwrap().to_string(),
        10 => (bytes.u64(0).unwrap() as i64).to_string(),
        11 => bytes.u64(0).unwrap().to_string(),
        12 => format!("{:?}", f32::from_bits(bytes.u32(0).unwrap())),
        13 => format!("{:?}", f64::from_bits(bytes.u64(0).unwrap())),
        _ => hex(bytes
            .iter()
            .take(8)
            .enumerate()
            .fold(0, |v, (i, &b)| v | ((b as u64) << (i * 8)))),
    }
}
pub fn signature(bytes: &[u8]) -> Option<&'static str> {
    let known: &[(&[u8], &str)] = &[
        (b"\x1f\x8b", "gzip data"),
        (b"\x28\xb5\x2f\xfd", "zstd frame"),
        (b"\x89PNG", "PNG image"),
        (b"\xff\xd8\xff", "JPEG image"),
        (b"PK\x03\x04", "zip archive"),
        (b"GIF8", "GIF image"),
        (b"RIFF", "RIFF (WebP, WAV)"),
        (b"\x16\x03", "TLS record"),
        (b"\x30\x82", "DER (certificate or key)"),
    ];
    known
        .iter()
        .find(|(p, _)| bytes.starts_with(p))
        .map(|(_, s)| *s)
}
pub fn decode_leniently(bytes: &[u8]) -> (String, usize, usize) {
    let (mut text, mut valid, mut ascii, mut k) = (String::new(), 0, 0, 0);
    while k < bytes.len() {
        let b = bytes[k];
        if b < 0x80 {
            if b >= 0x20 || [9, 10, 13].contains(&b) {
                valid += 1;
                ascii += 1;
                text.push(b as char);
            } else {
                text.push('·');
            }
            k += 1;
            continue;
        }
        let n = if b >= 0xf0 {
            4
        } else if b >= 0xe0 {
            3
        } else if b >= 0xc0 {
            2
        } else {
            0
        };
        if n > 0
            && let Some(s) = bytes
                .get(k..k + n)
                .and_then(|b| std::str::from_utf8(b).ok())
        {
            text.push_str(s);
            valid += n;
            k += n;
        } else {
            text.push('�');
            k += 1;
        }
    }
    (text, valid, ascii)
}
pub fn hex_dump(bytes: &[u8], lines: usize) -> Vec<String> {
    bytes
        .chunks(16)
        .take(lines)
        .enumerate()
        .map(|(i, row)| {
            let hex = row
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let text: String = row
                .iter()
                .map(|&b| {
                    if (0x20..0x7f).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:06x}  {hex:48}  {text}", i * 16)
        })
        .collect()
}
