use crate::{
    Error, Result,
    bytes::ReadBytes,
    column::{Column, ColumnStore},
    core_dump::Cursor,
    gc_desc::GcDesc,
    handles::{self, HandleTables},
    heap::ManagedHeap,
    runtime::RuntimeInfo,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisOptions {
    pub native_memory_roots: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RootKind {
    Stack {
        tid: i32,
    },
    Registers {
        tid: i32,
    },
    Mapping {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Bss {
        path: String,
    },
    Handles {
        #[serde(rename = "type")]
        kind: String,
    },
    Immortal {},
    FinalizerQueue {},
    FinalizationRegistered {},
    ThreadStatics {
        tid: i32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootSource {
    pub kind: RootKind,
    pub start: u64,
    pub end: u64,
}
impl RootSource {
    pub fn description(&self) -> String {
        let (start, end) = (self.start, self.end);
        let filename = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
        match &self.kind {
            RootKind::Stack { tid } => format!("stack of thread {tid} (0x{start:x}-0x{end:x})"),
            RootKind::Registers { tid } => format!("registers of thread {tid}"),
            RootKind::Mapping { path: Some(path) } => {
                format!("{} (0x{start:x}-0x{end:x})", filename(path))
            }
            RootKind::Mapping { path: None } => format!(
                "{} 0x{start:x}-0x{end:x} (unverified)",
                if start & 0x3ff_ffff == 0 {
                    "malloc arena"
                } else {
                    "native memory"
                }
            ),
            RootKind::Bss { path } => format!("{} .bss (0x{start:x}-0x{end:x})", filename(path)),
            RootKind::Handles { kind } => format!("{kind} GC handles"),
            RootKind::Immortal {} => {
                "NonGC heap or Satori immortal regions (never collected)".into()
            }
            RootKind::FinalizerQueue {} => "finalizer queue".into(),
            RootKind::FinalizationRegistered {} => "registered for finalization".into(),
            RootKind::ThreadStatics { tid } => format!("thread statics of thread {tid}"),
        }
    }
}

pub struct CompressedRows {
    pub start: Column<u64>,
    pub items: Column<i32>,
}
impl CompressedRows {
    pub fn count(&self) -> usize {
        self.start.len() - 1
    }
    pub fn row(&self, row: usize) -> &[i32] {
        &self.items[self.start[row] as usize..self.start[row + 1] as usize]
    }
    pub fn empty(store: &ColumnStore) -> Result<Self> {
        Ok(Self {
            start: store.make(1, None)?,
            items: store.make(0, None)?,
        })
    }
    pub fn from_rows(rows: &[Vec<i32>], store: &ColumnStore, name: Option<&str>) -> Result<Self> {
        let mut start = store.make(
            rows.len() + 1,
            name.map(|n| format!("{n}.start")).as_deref(),
        )?;
        for (i, row) in rows.iter().enumerate() {
            start[i + 1] = start[i] + row.len() as u64;
        }
        let mut items = store.make(
            start[rows.len()] as usize,
            name.map(|n| format!("{n}.items")).as_deref(),
        )?;
        for (i, row) in rows.iter().enumerate() {
            items[start[i] as usize..start[i + 1] as usize].copy_from_slice(row);
        }
        Ok(Self { start, items })
    }
    pub fn build<I: Iterator<Item = i32>>(
        rows: usize,
        store: &ColumnStore,
        row: impl Fn(usize) -> I + Sync,
    ) -> Result<Self> {
        let mut start: Column<u64> = store.make(rows + 1, None)?;
        start[1..]
            .par_chunks_mut(2048)
            .enumerate()
            .for_each(|(batch, counts)| {
                for (offset, count) in counts.iter_mut().enumerate() {
                    *count = row(batch * 2048 + offset).count() as u64;
                }
            });
        for i in 0..rows {
            start[i + 1] += start[i];
        }
        let mut items = store.make(start[rows] as usize, None)?;
        let mut remaining = &mut items[..];
        let mut parts = Vec::new();
        for begin in (0..rows).step_by(2048) {
            let end = (begin + 2048).min(rows);
            let (part, rest) = remaining.split_at_mut((start[end] - start[begin]) as usize);
            parts.push((begin, end, part));
            remaining = rest;
        }
        let valid = parts.into_par_iter().all(|(begin, end, part)| {
            let mut written = 0;
            for i in begin..end {
                for value in row(i) {
                    if written < part.len() {
                        part[written] = value;
                    }
                    written += 1;
                }
            }
            written == part.len()
        });
        if !valid {
            return Err(Error::cache("graph rows changed while being built"));
        }
        Ok(Self { start, items })
    }
    pub fn transpose(
        &self,
        columns: usize,
        store: &ColumnStore,
        name: Option<&str>,
    ) -> Result<Self> {
        let mut start: Column<u64> =
            store.make(columns + 1, name.map(|n| format!("{n}.start")).as_deref())?;
        for &v in &self.items[..] {
            if v < 0 || v as usize >= columns {
                return Err(Error::cache("graph target is outside the object index"));
            }
            start[v as usize + 1] += 1;
        }
        for i in 0..columns {
            start[i + 1] += start[i];
        }
        let mut fill = store.copy(&start, None)?;
        let mut items = store.make(
            self.items.len(),
            name.map(|n| format!("{n}.items")).as_deref(),
        )?;
        for r in 0..self.count() {
            for &v in self.row(r) {
                items[fill[v as usize] as usize] = r as i32;
                fill[v as usize] += 1;
            }
        }
        Ok(Self { start, items })
    }
    pub fn open(store: &ColumnStore, name: &str) -> Result<Self> {
        let rows = Self {
            start: store.open(&format!("{name}.start"))?,
            items: store.open(&format!("{name}.items"))?,
        };
        if rows.start.is_empty()
            || rows.start[0] != 0
            || rows.start.windows(2).any(|w| w[0] > w[1])
            || *rows.start.last().unwrap() != rows.items.len() as u64
        {
            return Err(Error::cache(format!("invalid graph column {name}")));
        }
        Ok(rows)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcHandle {
    #[serde(rename = "type")]
    pub kind: u8,
    pub slot: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSaved {
    pub options: AnalysisOptions,
    pub live_count: usize,
    pub live_bytes: u64,
    pub pending_count: usize,
    pub pending_bytes: u64,
    pub sources: Vec<RootSource>,
    pub handle_counts: BTreeMap<String, usize>,
    pub handle_segment_addresses: Vec<u64>,
    pub handles: Vec<GcHandle>,
    pub dependents: BTreeMap<i32, Vec<i32>>,
}

pub struct HeapGraph {
    pub saved: GraphSaved,
    pub live: Column<u8>,
    pub pending: Column<u8>,
    pub parent: Column<i32>,
    pub root_slot_objects: Column<i32>,
    pub root_slot_addresses: Column<u64>,
    pub reference_rows: CompressedRows,
    pub referrer_rows: CompressedRows,
    pub source_rows: CompressedRows,
    pub sources_of: CompressedRows,
    pub idom: Column<i32>,
    pub retained: Column<u64>,
    pub dominator_rows: CompressedRows,
}
impl std::ops::Deref for HeapGraph {
    type Target = GraphSaved;
    fn deref(&self) -> &GraphSaved {
        &self.saved
    }
}

impl HeapGraph {
    pub fn new(
        heap: &mut ManagedHeap,
        runtime: Option<&RuntimeInfo>,
        options: AnalysisOptions,
    ) -> Result<Self> {
        let store = &heap.store;
        let references = build_references(heap)?;
        heap.dump.check()?;
        let tables = HandleTables::find(&heap.dump, heap.low, heap.high);
        let (mut handles, mut handle_counts, mut dependents) = (
            Vec::new(),
            BTreeMap::new(),
            BTreeMap::<i32, Vec<i32>>::new(),
        );
        for h in &tables.handles {
            *handle_counts.entry(handles::type_name(h.kind)).or_insert(0) += 1;
            handles.push(GcHandle {
                kind: h.kind,
                slot: h.slot,
                target: heap.object_at(h.target),
            });
            if h.kind == 6
                && let (Some(p), Some(q)) = (heap.object_at(h.target), heap.object_at(h.secondary))
            {
                dependents.entry(p as i32).or_default().push(q as i32);
            }
        }
        let mut marker = Marker::new(heap, &tables, &references, &dependents)?;
        marker.mark(runtime, options);
        heap.dump.check()?;
        let source_rows = CompressedRows::from_rows(&marker.root_lists, store, Some("sources"))?;
        marker.slots.sort_unstable_by_key(|s| s.0);
        let root_slot_objects = store.copy(
            &marker.slots.iter().map(|s| s.0).collect::<Vec<_>>(),
            Some("rootSlot.objects"),
        )?;
        let root_slot_addresses = store.copy(
            &marker.slots.iter().map(|s| s.1).collect::<Vec<_>>(),
            Some("rootSlot.addresses"),
        )?;
        let Marker {
            live,
            pending,
            parent,
            sources,
            ..
        } = marker;
        let mut graph = Self {
            saved: GraphSaved {
                options,
                live_count: 0,
                live_bytes: 0,
                pending_count: 0,
                pending_bytes: 0,
                sources,
                handle_counts,
                handle_segment_addresses: tables.segments.clone(),
                handles,
                dependents,
            },
            live,
            pending,
            parent,
            root_slot_objects,
            root_slot_addresses,
            reference_rows: references,
            referrer_rows: CompressedRows::empty(store)?,
            source_rows,
            sources_of: CompressedRows::empty(store)?,
            idom: store.make(0, None)?,
            retained: store.make(0, None)?,
            dominator_rows: CompressedRows::empty(store)?,
        };
        for i in 0..heap.count() {
            if graph.live[i] != 0 {
                let size = heap.size(i);
                let r = heap.region_of(i);
                if graph.pending[i] != 0 {
                    graph.saved.pending_count += 1;
                    graph.saved.pending_bytes += size;
                    heap.saved.regions[r].pending_bytes += size;
                } else {
                    graph.saved.live_count += 1;
                    graph.saved.live_bytes += size;
                    heap.saved.regions[r].live_bytes += size;
                }
            }
        }
        graph.build_referrers(heap)?;
        graph.sources_of =
            graph
                .source_rows
                .transpose(heap.count(), &heap.store, Some("sourcesOf"))?;
        let result = dominators(&graph, &heap.store, |v| {
            if v < heap.count() { heap.size(v) } else { 0 }
        })?;
        graph.idom = result.0;
        graph.retained = result.1;
        graph.dominator_rows = result.2;
        Ok(graph)
    }

    pub fn from_saved(heap: &ManagedHeap, saved: GraphSaved) -> Result<Self> {
        let s = &heap.store;
        let graph = Self {
            saved,
            live: s.open("live")?,
            pending: s.open("pending")?,
            parent: s.open("parent")?,
            root_slot_objects: s.open("rootSlot.objects")?,
            root_slot_addresses: s.open("rootSlot.addresses")?,
            reference_rows: CompressedRows::open(s, "references")?,
            referrer_rows: CompressedRows::open(s, "referrers")?,
            source_rows: CompressedRows::open(s, "sources")?,
            sources_of: CompressedRows::open(s, "sourcesOf")?,
            idom: s.open("idom")?,
            retained: s.open("retained")?,
            dominator_rows: CompressedRows::open(s, "dominatorChildren")?,
        };
        if graph.live.len() != heap.count()
            || graph.pending.len() != heap.count()
            || graph.parent.len() != heap.count()
            || graph.reference_rows.count() != heap.count()
            || graph.idom.len() != graph.root_node() + 1
            || graph.retained.len() != graph.idom.len()
        {
            return Err(Error::cache("graph columns do not match the manifest"));
        }
        let n = heap.count();
        let sources = graph.sources.len();
        let total = graph.idom.len();
        let objects_valid = |items: &[i32]| items.iter().all(|&i| i >= 0 && (i as usize) < n);
        if graph.referrer_rows.count() != n
            || graph.sources_of.count() != n
            || graph.source_rows.count() != sources
            || graph.dominator_rows.count() != total
            || graph.root_slot_objects.len() != graph.root_slot_addresses.len()
            || !objects_valid(&graph.reference_rows.items)
            || !objects_valid(&graph.referrer_rows.items)
            || !objects_valid(&graph.source_rows.items)
            || !objects_valid(&graph.root_slot_objects)
            || graph.root_slot_objects.windows(2).any(|v| v[0] > v[1])
            || graph
                .sources_of
                .items
                .iter()
                .any(|&i| i < 0 || i as usize >= sources)
            || graph
                .dominator_rows
                .items
                .iter()
                .any(|&i| i < 0 || i as usize >= total)
            || graph
                .idom
                .iter()
                .any(|&i| i < -1 || i >= 0 && i as usize >= total)
            || graph.parent.iter().any(|&i| {
                i != i32::MIN
                    && if i < 0 {
                        -(i as i64) > sources as i64
                    } else {
                        i as usize >= n
                    }
            })
            || graph
                .live
                .iter()
                .zip(graph.pending.iter())
                .any(|(&live, &pending)| live > 1 || pending > live)
            || graph
                .handles
                .iter()
                .any(|h| h.target.is_some_and(|i| i >= n))
            || graph
                .dependents
                .iter()
                .any(|(&i, targets)| i < 0 || i as usize >= n || !objects_valid(targets))
            || !valid_parent_forest(&graph.idom, None)
            || !valid_parent_forest(&graph.parent, None)
        {
            return Err(Error::cache("invalid reference graph"));
        }
        Ok(graph)
    }

    pub fn root_node(&self) -> usize {
        self.reference_rows.count() + self.sources.len()
    }
    pub fn state(&self, i: usize) -> &'static str {
        if self.live[i] == 0 {
            "dead"
        } else if self.pending[i] != 0 {
            "pending finalization"
        } else {
            "live"
        }
    }
    pub fn root_slot(&self, i: usize) -> Option<u64> {
        self.root_slot_objects
            .binary_search(&(i as i32))
            .ok()
            .map(|k| self.root_slot_addresses[k])
    }
    pub fn references(&self, i: usize) -> Vec<i32> {
        let mut seen = HashSet::new();
        self.edges(i).filter(|&v| seen.insert(v)).collect()
    }
    pub fn edges(&self, i: usize) -> impl Iterator<Item = i32> + '_ {
        self.reference_rows
            .row(i)
            .iter()
            .chain(self.dependents.get(&(i as i32)).into_iter().flatten())
            .copied()
    }
    pub fn dominated_children(&self, node: usize) -> Vec<usize> {
        let mut children: Vec<_> = self
            .dominator_rows
            .row(node)
            .iter()
            .map(|&v| v as usize)
            .collect();
        children.sort_by_key(|&v| std::cmp::Reverse(self.retained[v]));
        children
    }
    pub fn path_to_root(&self, i: usize) -> (Option<usize>, Vec<usize>) {
        let mut chain = vec![i];
        let mut p = self.parent[i];
        while p >= 0 && chain.len() <= self.parent.len() {
            chain.push(p as usize);
            p = self.parent[p as usize];
        }
        chain.reverse();
        ((p != i32::MIN && p < 0).then(|| (-(p + 1)) as usize), chain)
    }
    fn build_referrers(&mut self, heap: &ManagedHeap) -> Result<()> {
        let n = heap.count();
        self.referrer_rows = CompressedRows::build(n, &heap.store, |i| {
            self.edges(i)
                .take(if self.live[i] != 0 { usize::MAX } else { 0 })
        })?
        .transpose(n, &heap.store, Some("referrers"))?;
        Ok(())
    }
}

pub(crate) fn valid_parent_forest(parents: &[i32], self_root: Option<usize>) -> bool {
    let mut state = vec![0u8; parents.len()];
    for start in 0..parents.len() {
        if state[start] != 0 {
            continue;
        }
        let mut current = start;
        loop {
            if state[current] == 1 {
                return false;
            }
            if state[current] == 2 {
                break;
            }
            state[current] = 1;
            let parent = parents[current];
            if parent < 0 || Some(current) == self_root && parent as usize == current {
                break;
            }
            current = parent as usize;
            if current >= parents.len() {
                return false;
            }
        }
        let mut current = start;
        while state[current] == 1 {
            state[current] = 2;
            let parent = parents[current];
            if parent < 0 {
                break;
            }
            current = parent as usize;
        }
    }
    true
}

fn build_references(heap: &ManagedHeap) -> Result<CompressedRows> {
    let descriptors: Vec<_> = heap
        .types
        .iter()
        .map(|&t| GcDesc::read(t, &heap.dump))
        .collect::<Result<_>>()?;
    let mut start: Column<u64> = heap
        .store
        .make(heap.count() + 1, Some("references.start"))?;
    start[1..]
        .par_chunks_mut(2048)
        .enumerate()
        .for_each(|(chunk, counts)| {
            let mut cursor = Cursor::new(&heap.dump, true);
            for (j, count) in counts.iter_mut().enumerate() {
                let i = chunk * 2048 + j;
                descriptors[heap.type_ids[i] as usize].scan(heap, i, &mut cursor, |_| *count += 1);
            }
        });
    heap.dump.check()?;
    for i in 0..heap.count() {
        start[i + 1] += start[i];
    }
    let mut items = heap
        .store
        .make(start[heap.count()] as usize, Some("references.items"))?;
    let mut remaining = &mut items[..];
    let mut parts = Vec::new();
    for begin in (0..heap.count()).step_by(2048) {
        let end = (begin + 2048).min(heap.count());
        let (part, rest) = remaining.split_at_mut((start[end] - start[begin]) as usize);
        remaining = rest;
        parts.push((begin, end, part));
    }
    let valid = parts.into_par_iter().all(|(begin, end, part)| {
        let mut cursor = Cursor::new(&heap.dump, true);
        let mut written = 0;
        for i in begin..end {
            descriptors[heap.type_ids[i] as usize].scan(heap, i, &mut cursor, |v| {
                if written < part.len() {
                    part[written] = v;
                }
                written += 1;
            });
        }
        written == part.len()
    });
    heap.dump.check()?;
    if !valid {
        return Err(Error(
            "the dump changed while its references were being read".into(),
        ));
    }
    Ok(CompressedRows { start, items })
}

struct Marker<'a> {
    heap: &'a ManagedHeap,
    tables: &'a HandleTables,
    references: &'a CompressedRows,
    dependents: &'a BTreeMap<i32, Vec<i32>>,
    live: Column<u8>,
    pending: Column<u8>,
    parent: Column<i32>,
    queue: Column<i32>,
    head: usize,
    tail: usize,
    sources: Vec<RootSource>,
    root_lists: Vec<Vec<i32>>,
    slots: Vec<(i32, u64)>,
}
impl<'a> Marker<'a> {
    fn new(
        heap: &'a ManagedHeap,
        tables: &'a HandleTables,
        references: &'a CompressedRows,
        dependents: &'a BTreeMap<i32, Vec<i32>>,
    ) -> Result<Self> {
        let n = heap.count();
        let s = &heap.store;
        let mut parent = s.make(n, Some("parent"))?;
        parent.fill(i32::MIN);
        Ok(Self {
            heap,
            tables,
            references,
            dependents,
            live: s.make(n, Some("live"))?,
            pending: s.make(n, Some("pending"))?,
            parent,
            queue: s.make(n, None)?,
            head: 0,
            tail: 0,
            sources: vec![],
            root_lists: vec![],
            slots: vec![],
        })
    }
    fn add(
        &mut self,
        source: RootSource,
        scan: bool,
        interior: bool,
        values: impl IntoIterator<Item = (u64, u64)>,
    ) {
        let id = self.sources.len() as i32;
        let mut found = Vec::new();
        let mut visit = |v, slot| {
            if v < self.heap.low || v >= self.heap.high {
                return;
            }
            let i = if interior {
                self.heap.object_containing(v)
            } else {
                self.heap.object_at(v)
            };
            if let Some(i) = i {
                if self.live[i] == 0 {
                    self.live[i] = 1;
                    self.parent[i] = -(id + 1);
                    self.slots.push((i as i32, slot));
                    self.queue[self.tail] = i as i32;
                    self.tail += 1;
                }
                found.push(i as i32);
            }
        };
        if scan {
            let end = source.start + (source.end - source.start) / 8 * 8;
            self.heap.dump.scan(source.start, end, |address, bytes| {
                // Align to the source, including when a compressed frame ends inside a word.
                let first = ((8 - ((address - source.start) & 7)) & 7) as usize;
                for k in (first..bytes.len()).step_by(8) {
                    let slot = address + k as u64;
                    if slot + 8 > end
                        || self.tables.contains(slot)
                        || self
                            .heap
                            .finalization
                            .iter()
                            .any(|q| slot >= q.start && slot < q.end)
                    {
                        continue;
                    }
                    let value = bytes.u64(k).or_else(|| self.heap.dump.u64(slot));
                    if let Some(value) = value {
                        visit(value, slot);
                    }
                }
            });
        }
        for (value, slot) in values {
            visit(value, slot);
        }
        found.sort_unstable();
        found.dedup();
        self.sources.push(source);
        self.root_lists.push(found);
    }
    fn propagate(&mut self) {
        while self.head < self.tail {
            let i = self.queue[self.head] as usize;
            self.head += 1;
            for &j in self
                .references
                .row(i)
                .iter()
                .chain(self.dependents.get(&(i as i32)).into_iter().flatten())
            {
                let j = j as usize;
                if self.live[j] == 0 {
                    self.live[j] = 1;
                    self.parent[j] = i as i32;
                    self.queue[self.tail] = j as i32;
                    self.tail += 1;
                }
            }
        }
    }
    fn mark(&mut self, runtime: Option<&RuntimeInfo>, options: AnalysisOptions) {
        let heap = self.heap;
        let dump = &heap.dump;
        let source = |kind| RootSource {
            kind,
            start: 0,
            end: 0,
        };
        for t in &dump.threads {
            self.add(
                source(RootKind::Registers { tid: t.tid }),
                false,
                true,
                t.registers.iter().map(|&v| (v, 0)),
            );
        }
        for kind in 0..12 {
            if handles::is_strong(kind) {
                let values: Vec<_> = self
                    .tables
                    .handles
                    .iter()
                    .filter(|h| h.kind == kind)
                    .map(|h| (h.target, h.slot))
                    .collect();
                if !values.is_empty() {
                    self.add(
                        source(RootKind::Handles {
                            kind: handles::type_name(kind),
                        }),
                        false,
                        false,
                        values,
                    );
                }
            }
        }
        let immortal: Vec<_> = heap
            .regions
            .iter()
            .filter(|r| r.immortal)
            .flat_map(|r| (r.first_object..r.end_object).map(|i| (heap.addresses[i], 0)))
            .collect();
        if !immortal.is_empty() {
            self.add(source(RootKind::Immortal {}), false, false, immortal);
        }
        if let Some(runtime) = runtime
            && let Some(d) = &runtime.descriptor
            && let (Some(tld), Some(array)) = (
                d.offset("Thread", "ThreadLocalDataPtr"),
                d.offset("ThreadLocalData", "NonCollectibleTlsArrayData"),
            )
        {
            for (thread, tid) in runtime.threads(dump) {
                if let Some(tld) = thread
                    .checked_add(tld)
                    .and_then(|at| dump.u64(at))
                    .filter(|&v| v != 0)
                    && let Some(slot) = tld.checked_add(array)
                    && let Some(value) = dump.u64(slot).filter(|&v| v != 0)
                {
                    self.add(
                        source(RootKind::ThreadStatics { tid }),
                        false,
                        false,
                        [(value, slot)],
                    );
                }
            }
        }
        let stacks: HashMap<_, _> = dump
            .threads
            .iter()
            .filter_map(|t| dump.segment_index(t.stack_pointer).map(|s| (s, t)))
            .collect();
        for (i, seg) in dump.segments.iter().enumerate() {
            if !seg.writable
                || (seg.address < heap.high && seg.end() > heap.low)
                || dump.exited_thread_stacks.contains(&i)
            {
                continue;
            }
            let (start, kind, interior) = if let Some(t) = stacks.get(&i) {
                (
                    seg.address.max(t.stack_pointer.saturating_sub(128) & !7),
                    RootKind::Stack { tid: t.tid },
                    true,
                )
            } else {
                if !options.native_memory_roots {
                    continue;
                }
                let kind = if let Some(file) = dump.file_mapping(seg.address) {
                    RootKind::Mapping {
                        path: Some(file.path.clone()),
                    }
                } else if let Some(file) = dump.files.iter().rev().find(|f| f.end == seg.address) {
                    RootKind::Bss {
                        path: file.path.clone(),
                    }
                } else {
                    RootKind::Mapping { path: None }
                };
                (seg.address, kind, false)
            };
            self.add(
                RootSource {
                    kind,
                    start,
                    end: seg.end(),
                },
                true,
                interior,
                [],
            );
        }
        self.propagate();
        let pending_start = self.tail;
        for registered in [false, true] {
            let mut values = Vec::new();
            for q in &heap.finalization {
                let (start, end, stride) = if registered {
                    (q.registered_start, q.registered_end, 8)
                } else {
                    (q.ready_start, q.ready_end, q.stride)
                };
                for slot in (start..end).step_by(stride as usize) {
                    if let Some(v) = dump.u64(slot)
                        && let Some(i) = heap.object_at(v)
                        && self.live[i] == 0
                        && v.checked_sub(4).and_then(|p| dump.u32(p)).unwrap_or(0) & 0x4000_0000
                            == 0
                    {
                        values.push((v, slot));
                    }
                }
            }
            if !values.is_empty() {
                self.add(
                    source(if registered {
                        RootKind::FinalizationRegistered {}
                    } else {
                        RootKind::FinalizerQueue {}
                    }),
                    false,
                    false,
                    values,
                );
            }
        }
        self.propagate();
        for k in pending_start..self.tail {
            self.pending[self.queue[k] as usize] = 1;
        }
    }
}

pub trait DominatorGraph {
    fn total(&self) -> usize;
    fn root(&self) -> usize;
    fn capacity(&self) -> usize {
        self.total()
    }
    fn first_position(&self, node: usize) -> u64;
    fn next(&self, node: usize, position: &mut u64) -> Option<usize>;
    fn predecessors(&self, node: usize, body: &mut dyn FnMut(usize));
}
impl DominatorGraph for HeapGraph {
    fn total(&self) -> usize {
        self.root_node() + 1
    }
    fn root(&self) -> usize {
        self.root_node()
    }
    fn capacity(&self) -> usize {
        self.live_count + self.pending_count + self.sources.len() + 1
    }
    fn first_position(&self, node: usize) -> u64 {
        if node < self.reference_rows.count() {
            self.reference_rows.start[node]
        } else {
            0
        }
    }
    fn next(&self, node: usize, position: &mut u64) -> Option<usize> {
        let n = self.reference_rows.count();
        if node == self.root_node() {
            while (*position as usize) < self.sources.len() {
                let k = *position as usize;
                *position += 1;
                if !self.source_rows.row(k).is_empty() {
                    return Some(n + k);
                }
            }
            None
        } else if node >= n {
            let v = self.source_rows.row(node - n).get(*position as usize)?;
            *position += 1;
            Some(*v as usize)
        } else {
            let end = self.reference_rows.start[node + 1];
            if *position < end {
                let v = self.reference_rows.items[*position as usize];
                *position += 1;
                return Some(v as usize);
            }
            let v = self
                .dependents
                .get(&(node as i32))?
                .get((*position - end) as usize)?;
            *position += 1;
            Some(*v as usize)
        }
    }
    fn predecessors(&self, node: usize, body: &mut dyn FnMut(usize)) {
        let n = self.reference_rows.count();
        if node >= n {
            body(self.root_node());
            return;
        }
        for &v in self.referrer_rows.row(node) {
            body(v as usize);
        }
        for &v in self.sources_of.row(node) {
            body(n + v as usize);
        }
    }
}

pub fn dominators(
    graph: &impl DominatorGraph,
    store: &ColumnStore,
    weight: impl Fn(usize) -> u64,
) -> Result<(Column<i32>, Column<u64>, CompressedRows)> {
    let (total, root) = (graph.total(), graph.root());
    if total > i32::MAX as usize {
        return Err(Error("the dominator graph has too many nodes".into()));
    }
    let mut dfn: Column<i32> = store.make(total, None)?;
    dfn.fill(-1);
    let mut vertex: Column<i32> = store.make(graph.capacity(), None)?;
    let mut dfs_parent: Column<i32> = store.make(graph.capacity(), None)?;
    let mut stack = vec![(root, graph.first_position(root))];
    dfn[root] = 0;
    vertex[0] = root as i32;
    dfs_parent[0] = -1;
    let mut count = 1;
    while let Some((v, position)) = stack.last_mut() {
        let mut next = None;
        while let Some(w) = graph.next(*v, position) {
            if dfn[w] < 0 {
                next = Some(w);
                break;
            }
        }
        if let Some(w) = next {
            if count >= vertex.len() {
                return Err(Error(
                    "the marked heap does not match its reference graph".into(),
                ));
            }
            dfn[w] = count as i32;
            vertex[count] = w as i32;
            dfs_parent[count] = dfn[*v];
            count += 1;
            stack.push((w, graph.first_position(w)));
        } else {
            stack.pop();
        }
    }
    let mut semi: Column<i32> = store.make(count, None)?;
    for (i, v) in semi.iter_mut().enumerate() {
        *v = i as i32;
    }
    let mut label = store.copy(&semi, None)?;
    let scratch = || -> Result<Column<i32>> {
        let mut c = store.make(count, None)?;
        c.fill(-1);
        Ok(c)
    };
    let (mut idom_dfn, mut ancestor, mut bucket_head, mut bucket_next) =
        (scratch()?, scratch()?, scratch()?, scratch()?);
    let mut path = Vec::new();
    fn eval(
        v: i32,
        ancestor: &mut [i32],
        label: &mut [i32],
        semi: &[i32],
        path: &mut Vec<usize>,
    ) -> i32 {
        if ancestor[v as usize] < 0 {
            return v;
        }
        path.clear();
        let mut u = v as usize;
        while ancestor[ancestor[u] as usize] >= 0 {
            path.push(u);
            u = ancestor[u] as usize;
        }
        while let Some(x) = path.pop() {
            let a = ancestor[x] as usize;
            if semi[label[a] as usize] < semi[label[x] as usize] {
                label[x] = label[a];
            }
            ancestor[x] = ancestor[a];
        }
        label[v as usize]
    }
    for w in (1..count).rev() {
        let p = dfs_parent[w] as usize;
        graph.predecessors(vertex[w] as usize, &mut |v| {
            if dfn[v] < 0 {
                return;
            }
            let u = eval(dfn[v], &mut ancestor, &mut label, &semi, &mut path) as usize;
            semi[w] = semi[w].min(semi[u]);
        });
        let sw = semi[w] as usize;
        bucket_next[w] = bucket_head[sw];
        bucket_head[sw] = w as i32;
        ancestor[w] = p as i32;
        let mut b = bucket_head[p];
        while b >= 0 {
            let u = eval(b, &mut ancestor, &mut label, &semi, &mut path) as usize;
            idom_dfn[b as usize] = if semi[u] < semi[b as usize] {
                u as i32
            } else {
                p as i32
            };
            b = bucket_next[b as usize];
        }
        bucket_head[p] = -1;
    }
    for w in 1..count {
        if idom_dfn[w] != semi[w] {
            idom_dfn[w] = idom_dfn[idom_dfn[w] as usize];
        }
    }
    let mut idom: Column<i32> = store.make(total, Some("idom"))?;
    idom.fill(-1);
    for w in 1..count {
        idom[vertex[w] as usize] = vertex[idom_dfn[w] as usize];
    }
    let mut retained: Column<u64> = store.make(total, Some("retained"))?;
    for v in 0..total {
        if dfn[v] >= 0 {
            retained[v] = weight(v);
        }
    }
    for w in (1..count).rev() {
        let v = vertex[w] as usize;
        retained[idom[v] as usize] += retained[v];
    }
    let mut start: Column<u64> = store.make(total + 1, Some("dominatorChildren.start"))?;
    for &p in &idom[..] {
        if p >= 0 {
            start[p as usize + 1] += 1;
        }
    }
    for v in 0..total {
        start[v + 1] += start[v];
    }
    let mut fill = store.copy(&start, None)?;
    let mut items = store.make(start[total] as usize, Some("dominatorChildren.items"))?;
    for v in 0..total {
        if idom[v] >= 0 {
            let p = idom[v] as usize;
            items[fill[p] as usize] = v as i32;
            fill[p] += 1;
        }
    }
    Ok((idom, retained, CompressedRows { start, items }))
}
