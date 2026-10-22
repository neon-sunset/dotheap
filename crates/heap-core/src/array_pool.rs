use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding, pool_bucket_length},
    type_names::{property_name, short},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

impl HeapSnapshot {
    pub fn array_pool_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "arraypool",
                "ArrayPool",
                "Shared ArrayPool per-core partitions and thread-local slots, checked for double \
                    returns, possible use after return and missed returns.",
            );
            let mut slots: BTreeMap<usize, Vec<String>> = BTreeMap::new();
            let mut storage = HashSet::new();
            let mut element_types = HashSet::new();
            for pool in self
                .objects(&self.type_ids(|s| s.starts_with("System.Buffers.SharedArrayPool`1<")))
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
            {
                let name = self.type_name(pool);
                if let Some((_, element)) = name.split_once('<')
                    && let Some(element) = element.strip_suffix('>')
                {
                    element_types.insert(format!("{element}[]"));
                }
                for (bucket, partitions) in self
                    .elements(self.target(pool, "_buckets"))
                    .into_iter()
                    .enumerate()
                {
                    for (partition_index, partition) in self
                        .elements(self.target(partitions, "_partitions"))
                        .into_iter()
                        .enumerate()
                    {
                        let Some(partition) = partition else { continue };
                        let Some(arrays) = self.target(partition, "_arrays") else {
                            continue;
                        };
                        storage.insert(arrays);
                        let count = self.integer(partition, "_count").unwrap_or(0).max(0) as usize;
                        let length = if bucket < 60 {
                            (16u64 << bucket).to_string()
                        } else {
                            "?".into()
                        };
                        for (index, array) in self
                            .elements(Some(arrays))
                            .into_iter()
                            .take(count)
                            .enumerate()
                        {
                            if let Some(array) = array {
                                slots.entry(array).or_default().push(format!("bucket {bucket} ({length}) partition {partition_index} slot {index}"));
                            }
                        }
                    }
                }
            }
            for local in self
                .objects(
                    &self.type_ids(|s| s == "System.Buffers.SharedArrayPoolThreadLocalArray[]"),
                )
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
            {
                storage.insert(local);
                let owner = match self.graph.sources_of.row(local).first() {
                    Some(&source) => self.describe_source(source as usize)?,
                    None => format!("thread-local slots at 0x{:x}", self.heap.addresses[local]),
                };
                for element in self.struct_elements(local, 64).unwrap_or_default() {
                    if let Some(array) = element
                        .fields
                        .iter()
                        .find(|f| f.field.name == "Array")
                        .and_then(|f| f.target)
                    {
                        slots
                            .entry(array)
                            .or_default()
                            .push(format!("{owner}, slot {}", element.index));
                    }
                }
            }
            let mut double = CheckSection::new(
                "Returned twice",
                "The same array is in more than one pool slot: two renters will get it.",
                &["length", "type", "slots"],
            );
            for (&array, list) in slots.iter().filter(|(_, list)| list.len() > 1) {
                double.rows.push(CheckRow::new(
                    vec![
                        Cell::int(self.component_count(array).unwrap_or(0)),
                        Cell::text(self.type_name(array)),
                        Cell::text(list.join("; ")),
                    ],
                    vec![array],
                ));
            }
            let mut used = CheckSection::new(
                "Still referenced after return",
                "Pooled arrays still referenced by live objects outside the pool. A later renter \
                    can receive the same array; check for use after return.",
                &["length", "type", "held by"],
            );
            for &array in slots.keys() {
                let Some(parent) = self
                    .graph
                    .referrer_rows
                    .row(array)
                    .iter()
                    .map(|&o| o as usize)
                    .find(|o| self.graph.live[*o] != 0 && !storage.contains(o))
                else {
                    continue;
                };
                let field = self
                    .reference_name(parent, array)?
                    .as_deref()
                    .map(property_name)
                    .unwrap_or_default();
                let holder = if field.starts_with("static ") {
                    field
                } else {
                    short(&self.type_name(parent)) + &field
                };
                used.rows.push(CheckRow::new(
                    vec![
                        Cell::int(self.component_count(array).unwrap_or(0)),
                        Cell::text(self.type_name(array)),
                        Cell::text(holder),
                    ],
                    vec![array, parent],
                ));
            }
            let mut dropped: BTreeMap<String, (usize, u64, Vec<usize>)> = BTreeMap::new();
            let mut other_dead: BTreeMap<String, (usize, u64)> = BTreeMap::new();
            let mut rented: BTreeMap<String, usize> = BTreeMap::new();
            let garbage = self.garbage()?;
            for array in self.objects(&self.type_ids(|s| element_types.contains(s))) {
                let length = self.component_count(array).unwrap_or(0);
                let name = self.type_name(array);
                if self.graph.live[array] != 0 {
                    if pool_bucket_length(length) && !slots.contains_key(&array) {
                        *rented.entry(name).or_default() += 1;
                    }
                    continue;
                }
                if !pool_bucket_length(length) {
                    let (count, bytes) = other_dead.entry(name).or_default();
                    *count += 1;
                    *bytes += self.heap.size(array);
                    continue;
                }
                if garbage.referrers.row(array).iter().any(|&o| {
                    self.type_name(o as usize) == "System.Buffers.SharedArrayPoolThreadLocalArray[]"
                }) {
                    continue;
                }
                let (count, bytes, samples) = dropped
                    .entry(format!("{name} length {length}"))
                    .or_default();
                *count += 1;
                *bytes += self.heap.size(array);
                if samples.len() < 50 {
                    samples.push(array);
                }
            }
            let mut drops = CheckSection::new(
                "Dead arrays of pool sizes",
                "Dead arrays of pooled element types with bucket lengths (16 × 2ⁿ), excluding \
                    arrays from exited thread slots. These may be missed returns; directly \
                    allocated arrays of those sizes look the same. Compare them with dead arrays of \
                    other lengths below.",
                &["dead", "bytes", "array"],
            );
            let mut dropped: Vec<_> = dropped.into_iter().collect();
            dropped.sort_by(|(a, av), (b, bv)| bv.1.cmp(&av.1).then_with(|| a.cmp(b)));
            drops.rows = dropped
                .into_iter()
                .map(|(name, (count, bytes, samples))| {
                    CheckRow::new(
                        vec![Cell::int(count), Cell::Bytes(bytes), Cell::text(name)],
                        samples,
                    )
                })
                .collect();
            let mut baseline = CheckSection::new(
                "For comparison",
                "Dead arrays of the same element types with other lengths, plus live arrays of \
                    bucket sizes outside the pool. Live arrays may be rented or directly allocated. \
                    Use these counts to assess missed-return candidates.",
                &[
                    "dead, other lengths",
                    "bytes",
                    "live of pool sizes, outside the pool",
                    "type",
                ],
            );
            let mut names: Vec<_> = other_dead
                .keys()
                .chain(rented.keys())
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            names.sort_by(|a, b| {
                other_dead
                    .get(b)
                    .map(|v| v.1)
                    .unwrap_or(0)
                    .cmp(&other_dead.get(a).map(|v| v.1).unwrap_or(0))
                    .then_with(|| a.cmp(b))
            });
            for name in names {
                let (dead, bytes) = other_dead.get(&name).copied().unwrap_or_default();
                baseline.rows.push(CheckRow::new(
                    vec![
                        Cell::int(dead),
                        Cell::Bytes(bytes),
                        Cell::int(rented.get(&name).copied().unwrap_or(0)),
                        Cell::text(name),
                    ],
                    Vec::new(),
                ));
            }
            if !double.rows.is_empty() {
                let count = double.rows.len();
                let text = if count == 1 {
                    "1 array is in two or more ArrayPool slots (returned twice)".into()
                } else {
                    format!("{count} arrays are in two or more ArrayPool slots (returned twice)")
                };
                report.findings.push(Finding { bytes: 0, text });
            }
            if !used.rows.is_empty() {
                let count = used.rows.len();
                let text = if count == 1 {
                    "1 array returned to the ArrayPool is still referenced by a live object".into()
                } else {
                    format!(
                        "{count} arrays returned to the ArrayPool are still referenced by live objects"
                    )
                };
                report.findings.push(Finding { bytes: 0, text });
            }
            report.sections = vec![double, used, drops, baseline];
            Ok(report)
        };
        self.saved("check arraypool", compute)
    }
}
