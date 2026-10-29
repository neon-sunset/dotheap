use crate::{
    Result,
    analysis::HeapSnapshot,
    column::ColumnStore,
    gc_desc::GcDesc,
    graph::{DominatorGraph, RootKind, dominators},
    handles,
    native_graph::NativeGraph,
    ownership::{OwnershipGraph, OwnershipGroup, OwnershipNode},
    type_names::StaticField,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnerKind {
    Root,
    Source,
    Object,
    Chunk,
    StaticFields,
    ThreadStatics,
    Type,
    Field,
    Thread,
    Storage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnerEntry {
    pub id: usize,
    pub kind: OwnerKind,
    pub name: String,
    pub object: Option<usize>,
    pub chunk: Option<usize>,
    pub targets: Vec<usize>,
    pub thread_static: bool,
    pub size: u64,
    pub managed: u64,
    pub native: u64,
}
impl OwnerEntry {
    pub fn total(&self) -> u64 {
        self.managed + self.native
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaticReference {
    pub slot: u64,
    pub declaring_type: String,
    pub field: String,
    pub thread: Option<i32>,
    pub target: usize,
    pub owner: usize,
}

pub struct OwnerTree {
    ownership: Option<Arc<OwnershipGraph>>,
    pub root: usize,
    count: usize,
    entries: HashMap<usize, OwnerEntry>,
    children: HashMap<usize, Vec<usize>>,
    fields: HashMap<usize, Vec<StaticReference>>,
    pub roots: Vec<usize>,
}

impl HeapSnapshot {
    pub fn owner_tree(&self, managed_only: bool) -> Result<Arc<OwnerTree>> {
        self.memo(format!("static ownership {managed_only}"), || {
            OwnerTree::new(self, managed_only, None)
        })
    }

    pub fn storage_owner_tree(&self, object: usize) -> Result<Arc<OwnerTree>> {
        self.memo(format!("static storage ownership {object}"), || {
            OwnerTree::new(self, false, Some(object))
        })
    }
}

struct Base<'a> {
    s: &'a HeapSnapshot,
    ownership: Option<&'a OwnershipGraph>,
}
impl Base<'_> {
    fn root(&self) -> usize {
        self.ownership.map_or(self.s.graph.root_node(), |_| 0)
    }
    fn count(&self) -> usize {
        self.ownership
            .map_or(self.s.graph.root_node() + 1, |g| g.count)
    }
    fn object(&self, object: usize) -> usize {
        self.ownership.map_or(object, |g| g.object_node(object))
    }
    fn parent(&self, node: usize) -> Option<usize> {
        let p = self
            .ownership
            .map_or(&self.s.graph.idom[..], |g| &g.idom[..])[node];
        (p >= 0).then_some(p as usize)
    }
    fn retained(&self, node: usize) -> (u64, u64) {
        self.ownership.map_or(
            (self.s.graph.retained.get(node).copied().unwrap_or(0), 0),
            |g| (g.retained_managed[node], g.retained_native[node]),
        )
    }
    fn children(&self, node: usize) -> &[i32] {
        self.ownership.map_or_else(
            || self.s.graph.dominator_rows.row(node),
            |g| g.children_rows.row(node),
        )
    }
    fn node(&self, node: usize) -> OwnershipNode {
        if let Some(g) = self.ownership {
            return g.node(node);
        }
        if node == self.root() {
            OwnershipNode::Root
        } else if node < self.s.heap.count() {
            OwnershipNode::Object(node)
        } else {
            OwnershipNode::Source(node - self.s.heap.count())
        }
    }
    fn entry(&self, id: usize) -> Result<OwnerEntry> {
        let (managed, native) = self.retained(id);
        let (kind, name, object, chunk, size) = match self.node(id) {
            OwnershipNode::Root => (OwnerKind::Root, "roots".into(), None, None, 0),
            OwnershipNode::Source(k) => (
                OwnerKind::Source,
                if let Some(g) = self.ownership {
                    g.source_names[k].clone()
                } else {
                    self.s.describe_source(k)?
                },
                None,
                None,
                0,
            ),
            OwnershipNode::Object(i) => (
                OwnerKind::Object,
                self.s.type_name(i),
                Some(i),
                None,
                self.s.heap.size(i),
            ),
            OwnershipNode::Chunk(c) => {
                let g = self.s.native_graph()?;
                let a = g.attribution(c, self.s)?;
                (OwnerKind::Chunk, a.label, None, Some(c), g.chunks[c].size)
            }
        };
        Ok(OwnerEntry {
            id,
            kind,
            name,
            object,
            chunk,
            targets: vec![],
            thread_static: false,
            size,
            managed,
            native,
        })
    }
}

struct Predecessors<'a> {
    base: Base<'a>,
    native: Option<Arc<NativeGraph>>,
    extra: HashMap<usize, Vec<usize>>,
}
impl<'a> Predecessors<'a> {
    fn new(base: Base<'a>) -> Result<Self> {
        let mut extra = HashMap::<_, Vec<_>>::new();
        let native = if let Some(g) = base.ownership {
            let native = base.s.native_graph()?;
            for source in 0..g.source_names.len() {
                for &target in g.source_rows.row(source) {
                    extra.entry(target as usize).or_default().push(source + 1);
                }
            }
            let holders = base.s.handle_holders(&native)?;
            for h in base
                .s
                .graph
                .handles
                .iter()
                .filter(|h| handles::is_strong(h.kind))
            {
                if let Some(target) = h.target.filter(|&i| base.s.graph.live[i] != 0)
                    && let Some(chunks) = holders.get(&h.slot)
                {
                    extra
                        .entry(g.object_node(target))
                        .or_default()
                        .extend(chunks.iter().map(|&c| g.chunk_node(c as usize)));
                }
            }
            Some(native)
        } else {
            None
        };
        Ok(Self {
            base,
            native,
            extra,
        })
    }
    fn each(&self, node: usize, emit: &mut dyn FnMut(usize)) {
        let Some(g) = self.base.ownership else {
            self.base.s.graph.predecessors(node, emit);
            return;
        };
        let mut live = |p| {
            if g.is_reachable(p) {
                emit(p);
            }
        };
        match g.node(node) {
            OwnershipNode::Root => {}
            OwnershipNode::Source(_) => live(0),
            OwnershipNode::Object(i) => {
                for &p in self.base.s.graph.referrer_rows.row(i) {
                    live(g.object_node(p as usize));
                }
            }
            OwnershipNode::Chunk(c) => {
                let n = self.native.as_ref().unwrap();
                for &p in n.referrers(c) {
                    live(g.chunk_node(p as usize));
                }
                for &p in n.chunk_owners.get(&(c as i32)).into_iter().flatten() {
                    live(g.object_node(p));
                }
            }
        }
        for &p in self.extra.get(&node).into_iter().flatten() {
            live(p);
        }
    }
}

#[derive(Default)]
struct SmallGraph {
    edges: Vec<Vec<usize>>,
    predecessors: Vec<Vec<usize>>,
}
impl SmallGraph {
    fn add(&mut self) -> usize {
        let id = self.edges.len();
        self.edges.push(vec![]);
        self.predecessors.push(vec![]);
        id
    }
    fn edge(&mut self, from: usize, to: usize) {
        if from != to {
            self.edges[from].push(to);
            self.predecessors[to].push(from);
        }
    }
}
impl DominatorGraph for SmallGraph {
    fn total(&self) -> usize {
        self.edges.len()
    }
    fn root(&self) -> usize {
        0
    }
    fn first_position(&self, _: usize) -> u64 {
        0
    }
    fn next(&self, node: usize, position: &mut u64) -> Option<usize> {
        let out = self.edges[node].get(*position as usize).copied();
        *position += 1;
        out
    }
    fn predecessors(&self, node: usize, emit: &mut dyn FnMut(usize)) {
        for &p in &self.predecessors[node] {
            emit(p);
        }
    }
}

impl OwnerTree {
    fn base<'a>(&'a self, s: &'a HeapSnapshot) -> Base<'a> {
        Base {
            s,
            ownership: self.ownership.as_deref(),
        }
    }
    pub fn entry(&self, s: &HeapSnapshot, node: usize) -> Result<Option<OwnerEntry>> {
        if let Some(e) = self.entries.get(&node) {
            return Ok(Some(e.clone()));
        }
        if node < self.count {
            Ok(Some(self.base(s).entry(node)?))
        } else {
            Ok(None)
        }
    }
    pub fn children(&self, s: &HeapSnapshot, node: usize) -> Vec<usize> {
        let mut children = self.children.get(&node).cloned().unwrap_or_default();
        if node < self.count {
            children.extend(
                self.base(s)
                    .children(node)
                    .iter()
                    .map(|&n| n as usize)
                    .filter(|n| !self.entries.contains_key(n)),
            );
        }
        let base = self.base(s);
        let retained = |n| {
            self.entries.get(&n).map_or_else(
                || {
                    let (a, b) = base.retained(n);
                    a + b
                },
                OwnerEntry::total,
            )
        };
        children.sort_by(|&a, &b| retained(b).cmp(&retained(a)).then(a.cmp(&b)));
        children
    }
    pub fn groups(&self, s: &HeapSnapshot, node: usize) -> Result<Vec<OwnershipGroup>> {
        let mut groups = BTreeMap::<_, OwnershipGroup>::new();
        for id in self.children(s, node) {
            let e = self.entry(s, id)?.unwrap();
            let (kind, library) = if let Some(c) = e.chunk {
                let a = s.native_graph()?.attribution(c, s)?;
                (Some(a.kind.group_kind()), a.library)
            } else {
                (None, None)
            };
            let identity = if e.object.is_some() || e.chunk.is_some() {
                0
            } else {
                id + 1
            };
            let g = groups
                .entry((identity, e.name.clone(), kind, library.clone()))
                .or_insert_with(|| OwnershipGroup {
                    name: e.name,
                    kind,
                    library,
                    nodes: vec![],
                    size: 0,
                    managed: 0,
                    native: 0,
                });
            g.nodes.push(id);
            g.size += e.size;
            g.managed += e.managed;
            g.native += e.native;
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by(|a, b| b.total().cmp(&a.total()).then(a.name.cmp(&b.name)));
        Ok(groups)
    }
    pub fn storage_fields(&self, object: usize) -> &[StaticReference] {
        self.fields
            .get(&object)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn new(s: &HeapSnapshot, managed_only: bool, only_storage: Option<usize>) -> Result<Self> {
        let ownership = if managed_only {
            None
        } else {
            Some(s.ownership()?)
        };
        let base = Base {
            s,
            ownership: ownership.as_deref(),
        };
        let slots = s.static_field_slots()?;
        let mut bindings = Vec::<(u64, usize, usize, StaticField)>::new();
        let in_scope = |mut node: usize, object: usize| {
            let root = base.object(object);
            loop {
                if node == root {
                    return true;
                }
                let Some(parent) = base.parent(node) else {
                    return false;
                };
                node = parent;
            }
        };
        let mut reference_slots = HashMap::<usize, HashSet<u64>>::new();
        for (&slot, field) in slots.iter() {
            if let Some(storage) = s.heap.object_containing(slot)
                && only_storage.is_none_or(|object| in_scope(base.object(storage), object))
                && let Some(target) = s.dump.u64(slot).and_then(|a| s.heap.object_at(a))
                && s.graph.live[storage] != 0
                && s.graph.live[target] != 0
                && base.parent(base.object(storage)).is_some()
                && base.parent(base.object(target)).is_some()
                && s.graph
                    .reference_rows
                    .row(storage)
                    .contains(&(target as i32))
            {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    reference_slots.entry(storage)
                {
                    let mut addresses = HashSet::new();
                    GcDesc::read(s.heap.method_table(storage), &s.dump)?.slots(
                        s.heap.addresses[storage],
                        s.heap.size(storage),
                        s.heap.lengths[storage],
                        |at| {
                            addresses.insert(at);
                        },
                    );
                    entry.insert(addresses);
                }
                if reference_slots[&storage].contains(&slot) {
                    bindings.push((slot, storage, target, field.clone()));
                }
            }
        }
        bindings.sort_by_key(|&(slot, _, _, _)| slot);
        let field_storage: HashSet<_> = bindings.iter().map(|b| base.object(b.1)).collect();
        let mut storage = field_storage.clone();
        storage.extend(
            s.graph
                .sources
                .iter()
                .enumerate()
                .filter(|(_, source)| matches!(source.kind, RootKind::ThreadStatics { .. }))
                .flat_map(|(k, _)| {
                    s.graph
                        .source_rows
                        .row(k)
                        .iter()
                        .map(|&o| base.object(o as usize))
                }),
        );
        let mut thread_sources = HashSet::new();
        for (k, source) in s.graph.sources.iter().enumerate() {
            if only_storage.is_none() && matches!(source.kind, RootKind::ThreadStatics { .. }) {
                if let Some(g) = base.ownership {
                    let name = s.describe_source(k)?;
                    if let Some(k) = g.source_names.iter().position(|n| *n == name) {
                        thread_sources.insert(k + 1);
                    }
                } else {
                    thread_sources.insert(s.heap.count() + k);
                }
            }
        }
        thread_sources.retain(|&node| base.parent(node).is_some());
        let mut marked = BTreeSet::from([base.root()]);
        for node in bindings
            .iter()
            .flat_map(|b| [base.object(b.1), base.object(b.2)])
            .chain(thread_sources.iter().copied())
        {
            let mut n = node;
            while marked.insert(n) {
                let Some(p) = base.parent(n) else {
                    break;
                };
                n = p;
            }
        }
        let frontier: BTreeSet<_> = marked
            .iter()
            .flat_map(|&n| base.children(n))
            .map(|&c| c as usize)
            .filter(|c| !marked.contains(c))
            .collect();
        let mut originals = vec![base.root()];
        originals.extend(marked.iter().copied().filter(|&n| n != base.root()));
        originals.extend(frontier);
        let mut index: HashMap<_, _> = originals.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        let mut graph = SmallGraph::default();
        let mut entries = Vec::new();
        let mut weights = Vec::new();
        for &node in &originals {
            graph.add();
            entries.push(base.entry(node)?);
            weights.push(base.retained(node));
        }
        for &node in originals.iter().skip(1) {
            let p = base
                .parent(node)
                .expect("marked ancestors include the root");
            let w = base.retained(node);
            weights[index[&p]].0 -= w.0;
            weights[index[&p]].1 -= w.1;
        }
        let mut removed: HashSet<_> = bindings
            .iter()
            .map(|b| (base.object(b.1), base.object(b.2)))
            .collect();
        for &node in &field_storage {
            let OwnershipNode::Object(object) = base.node(node) else {
                continue;
            };
            // An unnamed reference that aliases a named field must keep its original edge.
            for &at in &reference_slots[&object] {
                if !slots.contains_key(&at)
                    && let Some(target) = s.dump.u64(at).and_then(|a| s.heap.object_at(a))
                {
                    removed.remove(&(node, base.object(target)));
                }
            }
        }

        let predecessors = Predecessors::new(Base {
            s,
            ownership: ownership.as_deref(),
        })?;
        let mut nearest = HashMap::new();
        for (i, &node) in originals.iter().enumerate().skip(1) {
            predecessors.each(node, &mut |p| {
                if removed.contains(&(p, node)) || thread_sources.contains(&node) {
                    return;
                }
                let mut at = p;
                let mut path = Vec::new();
                let found = loop {
                    if let Some(&i) = index.get(&at).or_else(|| nearest.get(&at)) {
                        break Some(i);
                    }
                    path.push(at);
                    let Some(parent) = base.parent(at) else {
                        break None;
                    };
                    at = parent;
                };
                if let Some(parent) = found {
                    for n in path {
                        nearest.insert(n, parent);
                    }
                    graph.edge(parent, i);
                }
            });
        }
        let mut next_id = base.count();
        let mut add = |graph: &mut SmallGraph,
                       entries: &mut Vec<OwnerEntry>,
                       weights: &mut Vec<(u64, u64)>,
                       kind,
                       name| {
            let i = graph.add();
            entries.push(OwnerEntry {
                id: next_id,
                kind,
                name,
                object: None,
                chunk: None,
                targets: vec![],
                thread_static: false,
                size: 0,
                managed: 0,
                native: 0,
            });
            next_id += 1;
            weights.push((0, 0));
            i
        };
        if !thread_sources.is_empty() {
            let group = add(
                &mut graph,
                &mut entries,
                &mut weights,
                OwnerKind::Storage,
                "Thread-static storage".into(),
            );
            graph.edge(0, group);
            for &source in &thread_sources {
                graph.edge(group, index[&source]);
            }
        }
        let mut anchors = HashMap::new();
        let lca = |a: usize, mut b: usize| {
            let mut ancestors = HashSet::from([a]);
            let mut p = a;
            while let Some(parent) = base.parent(p) {
                ancestors.insert(parent);
                p = parent;
            }
            while !ancestors.contains(&b) {
                let Some(p) = base.parent(b) else {
                    return base.root();
                };
                b = p;
            }
            b
        };
        for (_, object, _, f) in &bindings {
            let mut anchor = base.parent(base.object(*object)).unwrap_or(base.root());
            while storage.contains(&anchor) || matches!(base.node(anchor), OwnershipNode::Source(_))
            {
                anchor = base.parent(anchor).unwrap_or(base.root());
            }
            if let Some(object) = only_storage {
                anchor = base.object(object);
            }
            anchors
                .entry((f.method_table, f.thread.is_some()))
                .and_modify(|a| *a = lca(*a, anchor))
                .or_insert(anchor);
        }
        let mut categories = BTreeMap::new();
        let mut types = BTreeMap::new();
        let mut fields = BTreeMap::new();
        let mut threads = BTreeMap::new();
        let mut references = HashMap::<usize, Vec<StaticReference>>::new();
        let stacks = s.thread_stacks()?;
        for (slot, object, target, f) in bindings {
            let thread_static = f.thread.is_some();
            let anchor = index[&anchors[&(f.method_table, thread_static)]];
            let category = *categories
                .entry((anchor, thread_static))
                .or_insert_with(|| {
                    let n = add(
                        &mut graph,
                        &mut entries,
                        &mut weights,
                        if thread_static {
                            OwnerKind::ThreadStatics
                        } else {
                            OwnerKind::StaticFields
                        },
                        if thread_static {
                            "Thread statics"
                        } else {
                            "Static fields"
                        }
                        .into(),
                    );
                    graph.edge(anchor, n);
                    n
                });
            let ty = *types
                .entry((f.method_table, thread_static))
                .or_insert_with(|| {
                    let n = add(
                        &mut graph,
                        &mut entries,
                        &mut weights,
                        OwnerKind::Type,
                        f.declaring_type.clone(),
                    );
                    graph.edge(category, n);
                    n
                });
            let field = *fields.entry((ty, f.token)).or_insert_with(|| {
                let n = add(
                    &mut graph,
                    &mut entries,
                    &mut weights,
                    OwnerKind::Field,
                    f.name.clone(),
                );
                graph.edge(ty, n);
                n
            });
            let owner = if let Some(tid) = f.thread {
                *threads.entry((field, tid)).or_insert_with(|| {
                    let n = add(
                        &mut graph,
                        &mut entries,
                        &mut weights,
                        OwnerKind::Thread,
                        stacks.label(tid),
                    );
                    graph.edge(field, n);
                    n
                })
            } else {
                field
            };
            entries[ty].thread_static = thread_static;
            entries[field].thread_static = thread_static;
            entries[owner].thread_static = thread_static;
            if !entries[field].targets.contains(&target) {
                entries[field].targets.push(target);
            }
            if owner != field {
                entries[owner].targets.push(target);
            }
            graph.edge(owner, index[&base.object(target)]);
            references.entry(object).or_default().push(StaticReference {
                slot,
                declaring_type: f.declaring_type,
                field: f.name,
                thread: f.thread,
                target,
                owner: entries[owner].id,
            });
        }
        if only_storage.is_none() {
            let containers: HashSet<_> = s
                .graph
                .sources
                .iter()
                .enumerate()
                .filter(|(_, source)| matches!(source.kind, RootKind::ThreadStatics { .. }))
                .flat_map(|(k, _)| s.graph.source_rows.row(k).iter().map(|&o| o as usize))
                .collect();
            let mut outer = HashMap::<usize, Vec<StaticReference>>::new();
            for (&object, fields) in &references {
                let mut node = base.object(object);
                while let Some(parent) = base.parent(node) {
                    if let OwnershipNode::Object(container) = base.node(parent)
                        && containers.contains(&container)
                    {
                        outer
                            .entry(container)
                            .or_default()
                            .extend(fields.iter().cloned());
                    }
                    node = parent;
                }
            }
            for (object, fields) in outer {
                references.entry(object).or_default().extend(fields);
            }
        }
        let (idom, managed, _) = dominators(&graph, &ColumnStore::default(), |n| weights[n].0)?;
        let native = retained_weights(&idom, &weights.iter().map(|w| w.1).collect::<Vec<_>>());
        let mut children = HashMap::<_, Vec<_>>::new();
        for i in 0..entries.len() {
            entries[i].managed = managed[i];
            entries[i].native = native[i];
            if idom[i] >= 0 {
                children
                    .entry(entries[idom[i] as usize].id)
                    .or_default()
                    .push(entries[i].id);
            }
        }
        let mut roots = if let Some(g) = base.ownership {
            g.roots()
                .into_iter()
                .map(|r| r.node)
                .filter(|r| !thread_sources.contains(r))
                .collect::<Vec<_>>()
        } else {
            (0..s.graph.sources.len())
                .map(|k| s.heap.count() + k)
                .filter(|r| !thread_sources.contains(r))
                .collect()
        };
        roots.extend(
            types
                .iter()
                .filter(|(key, _)| anchors[*key] == base.root())
                .map(|(_, &i)| entries[i].id),
        );
        roots.extend(
            entries
                .iter()
                .filter(|e| e.kind == OwnerKind::Storage)
                .map(|e| e.id),
        );
        index.clear();
        let root = base.root();
        let count = base.count();
        s.dump.check()?;
        Ok(Self {
            ownership,
            root,
            count,
            entries: entries.into_iter().map(|e| (e.id, e)).collect(),
            children,
            fields: references,
            roots,
        })
    }
}

fn retained_weights(idom: &[i32], weights: &[u64]) -> Vec<u64> {
    let mut children = vec![Vec::new(); idom.len()];
    for (n, &p) in idom.iter().enumerate() {
        if p >= 0 {
            children[p as usize].push(n);
        }
    }
    let mut order = vec![0];
    let mut k = 0;
    while k < order.len() {
        order.extend_from_slice(&children[order[k]]);
        k += 1;
    }
    let mut retained = weights.to_vec();
    for &n in order.iter().rev() {
        if idom[n] >= 0 {
            retained[idom[n] as usize] += retained[n];
        }
    }
    retained
}
