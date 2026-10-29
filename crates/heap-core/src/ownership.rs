use crate::{
    Result,
    analysis::HeapSnapshot,
    cache::CacheGuard,
    column::{Column, ColumnStore},
    compressed::atomic_write,
    graph::{CompressedRows, DominatorGraph, RootKind, dominators, valid_parent_forest},
    handles,
    native_graph::{ChunkAttributionKind, NativeGraph},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OwnershipNode {
    Root,
    Source(usize),
    Object(usize),
    Chunk(usize),
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipSaved {
    format: u32,
    pub source_names: Vec<String>,
    pub object_base: usize,
    pub chunk_base: usize,
    pub count: usize,
}
pub struct OwnershipGraph {
    pub saved: OwnershipSaved,
    pub idom: Column<i32>,
    pub retained_managed: Column<u64>,
    pub retained_native: Column<u64>,
    pub children_rows: CompressedRows,
    pub parent: Column<i32>,
    pub source_rows: CompressedRows,
}
impl std::ops::Deref for OwnershipGraph {
    type Target = OwnershipSaved;
    fn deref(&self) -> &Self::Target {
        &self.saved
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnershipGroup {
    pub name: String,
    pub kind: Option<ChunkAttributionKind>,
    pub library: Option<String>,
    pub nodes: Vec<usize>,
    pub size: u64,
    pub managed: u64,
    pub native: u64,
}
pub struct OwnershipRoot {
    pub node: usize,
    pub name: String,
    pub objects: usize,
    pub chunks: usize,
    pub managed: u64,
    pub native: u64,
}
#[derive(Default)]
pub struct OwnershipTypeGroup {
    pub name: String,
    pub type_id: Option<u32>,
    pub objects: usize,
    pub bytes: u64,
    pub nodes: Vec<usize>,
}
impl OwnershipGroup {
    pub fn total(&self) -> u64 {
        self.managed + self.native
    }
}
impl HeapSnapshot {
    pub fn ownership(&self) -> Result<Arc<OwnershipGraph>> {
        self.memo("ownership graph", || OwnershipGraph::open(self))
    }
    /// The native allocations that store each GC handle slot. Handles to static field storage stay roots: the runtime
    /// keeps static fields alive, not the native structure that stores their handle.
    pub(crate) fn handle_holders(&self, native: &NativeGraph) -> Result<BTreeMap<u64, Vec<i32>>> {
        let statics = self.statics_arrays()?;
        let static_slots: HashSet<_> = self
            .graph
            .handles
            .iter()
            .filter(|h| h.target.is_some_and(|t| statics.contains(&t)))
            .map(|h| h.slot)
            .collect();
        let mut holders = BTreeMap::<u64, Vec<i32>>::new();
        for (&c, slots) in &native.handle_references {
            if native.is_used(c as usize) {
                for &slot in slots.iter().filter(|s| !static_slots.contains(s)) {
                    let list = holders.entry(slot).or_default();
                    if !list.contains(&c) {
                        list.push(c);
                    }
                }
            }
        }
        Ok(holders)
    }
    pub fn type_retained(&self) -> Result<Arc<Vec<u64>>> {
        self.saved("type retained with native memory v3", || {
            let graph = self.ownership()?;
            let mut retained = vec![0; self.heap.types.len()];
            let mut on_path = vec![0usize; retained.len()];
            let mut stack = vec![(0, 0)];
            while let Some((node, child)) = stack.last_mut() {
                if let Some(&next) = graph.children_rows.row(*node).get(*child) {
                    *child += 1;
                    let next = next as usize;
                    if let OwnershipNode::Object(object) = graph.node(next) {
                        let ty = self.heap.type_ids[object] as usize;
                        if on_path[ty] == 0 {
                            retained[ty] += graph.retained(next);
                        }
                        on_path[ty] += 1;
                    }
                    stack.push((next, 0));
                } else {
                    let (node, _) = stack.pop().unwrap();
                    if let OwnershipNode::Object(object) = graph.node(node) {
                        on_path[self.heap.type_ids[object] as usize] -= 1;
                    }
                }
            }
            Ok(retained)
        })
    }
    pub fn ownership_instances(&self, type_id: u32, limit: usize) -> Result<Vec<usize>> {
        let graph = self.ownership()?;
        let mut objects: Vec<_> = (0..self.heap.count())
            .filter(|&i| self.heap.type_ids[i] == type_id)
            .collect();
        objects.sort_by(|&a, &b| {
            graph
                .retained(graph.object_node(b))
                .cmp(&graph.retained(graph.object_node(a)))
                .then(self.heap.size(b).cmp(&self.heap.size(a)))
                .then(a.cmp(&b))
        });
        objects.truncate(limit);
        Ok(objects)
    }
    pub fn ownership_dominator_types(&self, type_id: u32) -> Result<Vec<OwnershipTypeGroup>> {
        let graph = self.ownership()?;
        let mut groups = HashMap::<_, OwnershipTypeGroup>::new();
        for i in (0..self.heap.count())
            .filter(|&i| self.heap.type_ids[i] == type_id && self.graph.live[i] != 0)
        {
            let node = graph.object_node(i);
            let dominator = graph.idom[node];
            if dominator < 0 {
                continue;
            }
            let dominator = dominator as usize;
            let name = self.ownership_name(dominator)?;
            let g = groups
                .entry(name.clone())
                .or_insert_with(|| OwnershipTypeGroup {
                    name,
                    type_id: match graph.node(dominator) {
                        OwnershipNode::Object(i) => Some(self.heap.type_ids[i]),
                        _ => None,
                    },
                    ..Default::default()
                });
            g.objects += 1;
            g.bytes += graph.retained(node);
            if g.nodes.len() < 300 && !g.nodes.contains(&dominator) {
                g.nodes.push(dominator);
            }
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
        Ok(groups)
    }
    pub fn ownership_name(&self, v: usize) -> Result<String> {
        let graph = self.ownership()?;
        Ok(match graph.node(v) {
            OwnershipNode::Root => "roots".into(),
            OwnershipNode::Source(k) => graph.source_names[k].clone(),
            OwnershipNode::Object(i) => self.node_name(i)?,
            OwnershipNode::Chunk(c) => self.native_graph()?.group_label(c, self)?,
        })
    }
    pub fn ownership_groups(&self, v: usize) -> Result<Arc<Vec<OwnershipGroup>>> {
        let compute = || {
            let graph = self.ownership()?;
            let native = self.native_graph()?;
            let mut groups = BTreeMap::<_, Vec<usize>>::new();
            for n in graph.children(v) {
                let key = match graph.node(n) {
                    OwnershipNode::Chunk(c) => {
                        let attribution = native.attribution(c, self)?;
                        (
                            3,
                            attribution.label,
                            Some(attribution.kind.group_kind()),
                            attribution.library,
                        )
                    }
                    OwnershipNode::Object(_) => (2, self.ownership_name(n)?, None, None),
                    OwnershipNode::Source(_) => (1, self.ownership_name(n)?, None, None),
                    OwnershipNode::Root => (0, self.ownership_name(n)?, None, None),
                };
                groups.entry(key).or_default().push(n);
            }
            let mut list: Vec<_> = groups
                .into_iter()
                .map(|((_, name, kind, library), nodes)| OwnershipGroup {
                    name,
                    kind,
                    library,
                    size: nodes
                        .iter()
                        .map(|&n| match graph.node(n) {
                            OwnershipNode::Object(i) => self.heap.size(i),
                            OwnershipNode::Chunk(c) => native.chunks[c].size,
                            _ => 0,
                        })
                        .sum(),
                    managed: nodes.iter().map(|&n| graph.retained_managed[n]).sum(),
                    native: nodes.iter().map(|&n| graph.retained_native[n]).sum(),
                    nodes,
                })
                .collect();
            list.sort_by(|a, b| b.total().cmp(&a.total()).then(a.name.cmp(&b.name)));
            Ok(list)
        };
        if v == 0 {
            self.saved("ownership groups of the root v6", compute)
        } else {
            self.memo(format!("ownership groups {v}"), compute)
        }
    }
}
impl OwnershipGraph {
    fn open(s: &HeapSnapshot) -> Result<Self> {
        let directory = s.results_directory.as_ref().map(|p| p.join("ownership"));
        let _guard = directory
            .as_ref()
            .map(|p| CacheGuard::lock(p))
            .transpose()?;
        let store = ColumnStore::new(directory.clone())?;
        if let Some(dir) = &directory
            && let Ok(bytes) = std::fs::read(dir.join("manifest.json"))
            && let Ok(saved) = serde_json::from_slice::<OwnershipSaved>(&bytes)
            && saved.format == 3
            && let Ok(graph) = Self::from_saved(saved, &store)
        {
            return Ok(graph);
        }
        if let Some(dir) = &directory {
            let _ = std::fs::remove_file(dir.join("manifest.json"));
        }
        let out = Self::new(s, &store)?;
        s.dump.check()?;
        if let Some(dir) = &directory {
            for c in [&out.idom, &out.parent] {
                c.flush()?;
            }
            for c in [
                &out.retained_managed,
                &out.retained_native,
                &out.children_rows.start,
            ] {
                c.flush()?;
            }
            out.children_rows.items.flush()?;
            out.source_rows.start.flush()?;
            out.source_rows.items.flush()?;
            atomic_write(&dir.join("manifest.json"), &serde_json::to_vec(&out.saved)?)?;
        }
        Ok(out)
    }
    fn from_saved(saved: OwnershipSaved, store: &ColumnStore) -> Result<Self> {
        let graph = Self {
            saved,
            idom: store.open("idom")?,
            retained_managed: store.open("retained")?,
            retained_native: store.open("retainedNative")?,
            children_rows: CompressedRows::open(store, "dominatorChildren")?,
            parent: store.open("parent")?,
            source_rows: CompressedRows::open(store, "rootReferences")?,
        };
        let n = graph.count;
        if graph.idom.len() != n
            || n == 0
            || graph.retained_managed.len() != n
            || graph.retained_native.len() != n
            || graph.parent.len() != n
            || graph.children_rows.count() != n
            || graph.object_base != 1 + graph.source_names.len()
            || graph.source_rows.count() != graph.source_names.len()
            || graph
                .source_rows
                .items
                .iter()
                .any(|&v| v < graph.object_base as i32 || v as usize >= n)
            || graph.chunk_base < graph.object_base
            || graph.chunk_base > n
            || graph
                .children_rows
                .items
                .iter()
                .any(|&v| v < 0 || v as usize >= n)
            || graph
                .idom
                .iter()
                .any(|&v| v < -1 || v as usize >= n && v >= 0)
            || !valid_parent_forest(&graph.idom, None)
            || !valid_parent_forest(&graph.parent, Some(0))
            || graph
                .parent
                .iter()
                .any(|&v| v < -1 || v as usize >= n && v >= 0)
        {
            return Err(crate::Error::cache("invalid ownership graph"));
        }
        Ok(graph)
    }
    pub fn node(&self, v: usize) -> OwnershipNode {
        if v == 0 {
            OwnershipNode::Root
        } else if v < self.object_base {
            OwnershipNode::Source(v - 1)
        } else if v < self.chunk_base {
            OwnershipNode::Object(v - self.object_base)
        } else {
            OwnershipNode::Chunk(v - self.chunk_base)
        }
    }
    pub fn object_node(&self, i: usize) -> usize {
        self.object_base + i
    }
    pub fn chunk_node(&self, c: usize) -> usize {
        self.chunk_base + c
    }
    pub fn retained(&self, v: usize) -> u64 {
        self.retained_managed[v] + self.retained_native[v]
    }
    pub fn is_reachable(&self, v: usize) -> bool {
        v == 0 || self.idom[v] >= 0
    }
    pub fn children(&self, v: usize) -> Vec<usize> {
        let mut list: Vec<_> = self
            .children_rows
            .row(v)
            .iter()
            .map(|&v| v as usize)
            .collect();
        list.sort_by(|&a, &b| self.retained(b).cmp(&self.retained(a)).then(a.cmp(&b)));
        list
    }
    pub fn path_to_root(&self, v: usize) -> Vec<usize> {
        if !self.is_reachable(v) {
            return vec![];
        }
        let mut path = vec![v];
        let mut x = v;
        while x != 0 && path.len() < 100_000 {
            let p = self.parent[x];
            if p < 0 {
                return vec![];
            }
            x = p as usize;
            path.push(x);
        }
        path.reverse();
        path
    }
    pub fn roots(&self) -> Vec<OwnershipRoot> {
        self.source_names
            .iter()
            .enumerate()
            .filter_map(|(source, name)| {
                let references = self.source_rows.row(source);
                if references.is_empty() {
                    return None;
                }
                let node = source + 1;
                let objects = references
                    .iter()
                    .filter(|&&n| (n as usize) < self.chunk_base)
                    .count();
                Some(OwnershipRoot {
                    node,
                    name: name.clone(),
                    objects,
                    chunks: references.len() - objects,
                    managed: self.retained_managed[node],
                    native: self.retained_native[node],
                })
            })
            .collect()
    }
    pub fn root_references(&self, node: usize) -> &[i32] {
        if node == 0 || node >= self.object_base {
            return &[];
        }
        self.source_rows.row(node - 1)
    }
    fn new(s: &HeapSnapshot, store: &ColumnStore) -> Result<Self> {
        let native = s.native_graph()?;
        let mut source_index = HashMap::new();
        let mut source_names = Vec::new();
        let mut source = |key: String, name: String| {
            if let Some(&i) = source_index.get(&key) {
                i
            } else {
                let i = source_names.len();
                source_index.insert(key, i);
                source_names.push(name);
                i
            }
        };
        let mut managed_sources = Vec::new();
        for (k, src) in s.graph.sources.iter().enumerate() {
            let key = match src.kind {
                RootKind::Stack { tid } => format!("stack of thread {tid}"),
                RootKind::Registers { tid } => format!("registers of thread {tid}"),
                _ => src.description(),
            };
            managed_sources.push(source(key, s.describe_source(k)?));
        }
        let mut native_sources = BTreeMap::new();
        let stacks = s.thread_stacks()?;
        for &c in &native.root_chunks {
            if !native.is_used(c as usize) {
                continue;
            }
            let keys = native
                .root_sources
                .get(&c)
                .cloned()
                .unwrap_or_else(|| vec!["native memory".into()]);
            let mut indices = Vec::new();
            for key in keys {
                let mut name = key.clone();
                for prefix in ["stack of thread ", "registers of thread "] {
                    if let Some(tid) = key.strip_prefix(prefix).and_then(|v| v.parse().ok()) {
                        name = format!("{}{}", &prefix[..prefix.len() - 7], stacks.label(tid));
                    }
                }
                indices.push(source(key, name));
            }
            native_sources.insert(c, indices);
        }
        let source_count = source_names.len();
        let object_base = 1 + source_count;
        let chunk_base = object_base + s.heap.count();
        let count = chunk_base + native.chunks.len();
        if count > i32::MAX as usize {
            return Err(crate::Error(
                "the ownership graph has too many nodes".into(),
            ));
        }
        let mut handle_edges = BTreeMap::<i32, Vec<i32>>::new();
        let mut handle_predecessors = BTreeMap::<i32, Vec<i32>>::new();
        let mut held_native = HashSet::new();
        let mut held_elsewhere = HashSet::new();
        let holders = s.handle_holders(&native)?;
        for h in &s.graph.handles {
            if !handles::is_strong(h.kind) {
                continue;
            }
            let Some(t) = h.target.filter(|&t| s.graph.live[t] != 0) else {
                continue;
            };
            let kind = handles::type_name(h.kind);
            if let Some(chunks) = holders.get(&h.slot) {
                for &c in chunks {
                    handle_edges.entry(c).or_default().push(t as i32);
                    handle_predecessors.entry(t as i32).or_default().push(c);
                }
                held_native.insert((kind, t));
            } else {
                held_elsewhere.insert((kind, t));
            }
        }
        let mut skipped = HashSet::new();
        for (k, src) in s.graph.sources.iter().enumerate() {
            if let RootKind::Handles { kind } = &src.kind {
                for (k_type, t) in &held_native {
                    if k_type == kind && !held_elsewhere.contains(&(k_type.clone(), *t)) {
                        skipped.insert((k, *t));
                    }
                }
            }
        }
        let mut source_objects = vec![Vec::new(); source_count];
        for k in 0..s.graph.sources.len() {
            for &o in s.graph.source_rows.row(k) {
                if s.graph.live[o as usize] != 0 && !skipped.contains(&(k, o as usize)) {
                    source_objects[managed_sources[k]].push(o);
                }
            }
        }
        let mut source_chunks = vec![Vec::new(); source_count];
        for (&c, sources) in &native_sources {
            for &k in sources {
                source_chunks[k].push(c);
            }
        }
        let references: Vec<Vec<i32>> = source_objects
            .iter()
            .zip(&source_chunks)
            .map(|(objects, chunks)| {
                let mut nodes: Vec<_> = objects
                    .iter()
                    .map(|&o| (object_base + o as usize) as i32)
                    .chain(chunks.iter().map(|&c| (chunk_base + c as usize) as i32))
                    .collect();
                nodes.sort_unstable();
                nodes.dedup();
                nodes
            })
            .collect();
        let source_rows = CompressedRows::from_rows(&references, store, Some("rootReferences"))?;
        drop(references);
        let edges = Edges {
            s,
            native: &native,
            object_base,
            chunk_base,
            count,
            source_objects,
            source_chunks,
            managed_sources,
            native_sources,
            skipped,
            handle_edges,
            handle_predecessors,
        };
        let (idom, retained_managed, children_rows) = dominators(&edges, store, |v| {
            if v >= object_base && v < chunk_base {
                s.heap.size(v - object_base)
            } else {
                0
            }
        })?;
        let mut order = vec![0];
        let mut head = 0;
        while head < order.len() {
            let v = order[head];
            head += 1;
            order.extend(children_rows.row(v).iter().map(|&v| v as usize));
        }
        let mut retained_native = store.make(count, Some("retainedNative"))?;
        for &v in &order {
            if v >= chunk_base && native.is_in_use(v - chunk_base) {
                retained_native[v] = native.chunks[v - chunk_base].size;
            }
        }
        for &v in order.iter().rev() {
            if idom[v] >= 0 {
                retained_native[idom[v] as usize] += retained_native[v];
            }
        }
        let mut parent: Column<i32> = store.make(count, Some("parent"))?;
        parent.fill(-1);
        parent[0] = 0;
        let mut queue = vec![0];
        let mut head = 0;
        while head < queue.len() {
            let v = queue[head];
            head += 1;
            let mut p = 0;
            while let Some(w) = edges.next(v, &mut p) {
                if parent[w] < 0 {
                    parent[w] = v as i32;
                    queue.push(w);
                }
            }
        }
        Ok(Self {
            saved: OwnershipSaved {
                format: 3,
                source_names,
                object_base,
                chunk_base,
                count,
            },
            idom,
            retained_managed,
            retained_native,
            children_rows,
            parent,
            source_rows,
        })
    }
}
struct Edges<'a> {
    s: &'a HeapSnapshot,
    native: &'a NativeGraph,
    object_base: usize,
    chunk_base: usize,
    count: usize,
    source_objects: Vec<Vec<i32>>,
    source_chunks: Vec<Vec<i32>>,
    managed_sources: Vec<usize>,
    native_sources: BTreeMap<i32, Vec<usize>>,
    skipped: HashSet<(usize, usize)>,
    handle_edges: BTreeMap<i32, Vec<i32>>,
    handle_predecessors: BTreeMap<i32, Vec<i32>>,
}
impl DominatorGraph for Edges<'_> {
    fn total(&self) -> usize {
        self.count
    }
    fn root(&self) -> usize {
        0
    }
    fn first_position(&self, _: usize) -> u64 {
        0
    }
    fn next(&self, v: usize, p: &mut u64) -> Option<usize> {
        loop {
            let k = *p as usize;
            *p += 1;
            if v == 0 {
                return (k + 1 < self.object_base).then_some(k + 1);
            }
            if v < self.object_base {
                let objects = &self.source_objects[v - 1];
                return if k < objects.len() {
                    Some(self.object_base + objects[k] as usize)
                } else {
                    self.source_chunks[v - 1]
                        .get(k - objects.len())
                        .map(|&c| self.chunk_base + c as usize)
                };
            }
            if v < self.chunk_base {
                let i = v - self.object_base;
                let refs = self.s.graph.reference_rows.row(i);
                let dependents = self
                    .s
                    .graph
                    .dependents
                    .get(&(i as i32))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if k < refs.len() + dependents.len() {
                    let j = if k < refs.len() {
                        refs[k]
                    } else {
                        dependents[k - refs.len()]
                    } as usize;
                    if self.s.graph.live[j] != 0 {
                        return Some(self.object_base + j);
                    }
                } else {
                    let c = *self
                        .native
                        .owned_chunks
                        .get(&i)?
                        .get(k - refs.len() - dependents.len())?
                        as usize;
                    if self.native.is_used(c) {
                        return Some(self.chunk_base + c);
                    }
                }
            } else {
                let c = v - self.chunk_base;
                let refs = self.native.pointers(c);
                if k < refs.len() {
                    let t = refs[k] as usize;
                    if self.native.is_used(t) {
                        return Some(self.chunk_base + t);
                    }
                } else {
                    return self
                        .handle_edges
                        .get(&(c as i32))?
                        .get(k - refs.len())
                        .map(|&t| self.object_base + t as usize);
                }
            }
        }
    }
    fn predecessors(&self, v: usize, emit: &mut dyn FnMut(usize)) {
        if v < self.object_base {
            if v > 0 {
                emit(0);
            }
            return;
        }
        if v < self.chunk_base {
            let i = v - self.object_base;
            for &r in self.s.graph.referrer_rows.row(i) {
                if self.s.graph.live[r as usize] != 0 {
                    emit(self.object_base + r as usize);
                }
            }
            for &k in self.s.graph.sources_of.row(i) {
                if !self.skipped.contains(&(k as usize, i)) {
                    emit(1 + self.managed_sources[k as usize]);
                }
            }
            for &c in self
                .handle_predecessors
                .get(&(i as i32))
                .into_iter()
                .flatten()
            {
                emit(self.chunk_base + c as usize);
            }
        } else {
            let c = (v - self.chunk_base) as i32;
            for &k in self.native_sources.get(&c).into_iter().flatten() {
                emit(1 + k);
            }
            for &o in self.native.chunk_owners.get(&c).into_iter().flatten() {
                if self.s.graph.live[o] != 0 {
                    emit(self.object_base + o);
                }
            }
            for &r in self.native.referrers(c as usize) {
                if self.native.is_used(r as usize) {
                    emit(self.chunk_base + r as usize);
                }
            }
        }
    }
}
