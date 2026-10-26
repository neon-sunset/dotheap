use crate::{
    Error, Result,
    analysis::HeapSnapshot,
    checks::CheckReport,
    graph::RootKind,
    queries::FieldValue,
    tasks::{
        self, TaskGraph, TaskGraphs, TaskGroup, TaskNode, TaskNodeKind, TaskState, async_method,
        field_targets, without_namespace,
    },
    type_names::{property_name, without_namespaces},
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AsyncAnalysis {
    pub graphs: TaskGraphs,
    pub report: CheckReport,
}

impl HeapSnapshot {
    pub fn async_check(&self) -> Result<Arc<CheckReport>> {
        self.saved("check async", || Ok(self.async_analysis()?.report.clone()))
    }

    pub fn async_analysis(&self) -> Result<Arc<AsyncAnalysis>> {
        let compute = || {
            let mut graph = AsyncGraph::new(self)?;
            let graphs = graph.model()?;
            let report = graph.report(&graphs)?;
            Ok(AsyncAnalysis { graphs, report })
        };
        self.saved("async graphs", compute)
    }
}

pub(crate) struct Method {
    pub name: String,
    pub short: String,
    pub state: i64,
    pub awaited: Option<usize>,
    pub this: Option<usize>,
    pub location: Option<String>,
}

impl Method {
    pub fn position(&self) -> String {
        match self.state {
            -1 => ", running".into(),
            state if state >= 0 => format!(
                " at await {state}{}",
                self.location
                    .as_ref()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default()
            ),
            _ => String::new(),
        }
    }
}

pub(crate) struct AsyncGraph<'a> {
    pub snapshot: &'a HeapSnapshot,
    pub methods: BTreeMap<usize, Method>,
    tasks: BTreeSet<usize>,
    pub task_types: HashSet<u32>,
    pooled_types: HashSet<u32>,
    delegate_types: HashSet<u32>,
    parents: BTreeMap<usize, Vec<usize>>,
    children: BTreeMap<usize, Vec<usize>>,
    sync_waits: BTreeMap<usize, Vec<usize>>,
    pub sync_waiters: HashSet<usize>,
    queued: HashSet<usize>,
    flags: HashMap<usize, u32>,
    pub waiters_of: BTreeMap<usize, BTreeSet<usize>>,
    pub waits: BTreeMap<(String, String, String), (usize, bool)>,
    remaining_nodes: usize,
}

const COMPLETED: u32 = 0x200000 | 0x400000 | 0x1000000;

enum Waiter {
    Task(usize),
    Thread(usize),
}

#[derive(Clone)]
struct Leaf {
    what: String,
    held: String,
    never: bool,
    resource: Option<usize>,
    holder: Option<usize>,
}

impl Leaf {
    fn text(&self) -> String {
        if self.held.is_empty() {
            self.what.clone()
        } else {
            format!("{}, {}", self.what, self.held)
        }
    }
}

#[derive(Clone, Copy)]
enum ItemKind {
    Node,
    Leaf,
    Thread { task: usize },
    Cycle,
}

#[derive(Clone)]
struct Item {
    object: usize,
    path: Vec<usize>,
    label: String,
    kind: ItemKind,
    leaf: Option<Leaf>,
    held_by: Option<(String, usize)>,
}

impl Item {
    fn new(object: usize, path: Vec<usize>, label: String, kind: ItemKind) -> Self {
        Self {
            object,
            path,
            label,
            kind,
            leaf: None,
            held_by: None,
        }
    }
}

struct Tree {
    item: Item,
    children: Vec<Tree>,
}

impl Tree {
    fn objects(&self) -> BTreeSet<usize> {
        let mut result = BTreeSet::new();
        let mut stack = vec![self];
        while let Some(tree) = stack.pop() {
            result.insert(tree.item.object);
            result.extend(&tree.item.path);
            if let ItemKind::Thread { task } = tree.item.kind {
                result.insert(task);
            }
            if let Some(resource) = tree.item.leaf.as_ref().and_then(|l| l.resource) {
                result.insert(resource);
            }
            stack.extend(&tree.children);
        }
        result
    }
}

impl AsyncGraph<'_> {
    fn held_only_by(&self, calls: &BTreeSet<usize>, object: usize) -> bool {
        let graph = &self.snapshot.graph;
        let mut current = graph.idom[object];
        for _ in 0..100_000 {
            if current < 0 || current as usize >= self.snapshot.heap.count() {
                return false;
            }
            if calls.contains(&(current as usize)) {
                return true;
            }
            current = graph.idom[current as usize];
        }
        false
    }

    fn leaf(&mut self, object: usize, ancestors: &[usize]) -> Result<Leaf> {
        let snapshot = self.snapshot;
        let name = snapshot.type_name(object);
        let mut what = if self.task_types.contains(&snapshot.heap.type_ids[object]) {
            self.label(object)?
        } else {
            without_namespaces(&name)
        };
        for (kind, operation) in [
            ("WaitingReadAsyncOperation", "WaitToReadAsync"),
            ("BlockedReadAsyncOperation`1", "ReadAsync"),
            ("WaitingWriteAsyncOperation", "WaitToWriteAsync"),
            ("BlockedWriteAsyncOperation`1", "WriteAsync"),
        ] {
            if name.starts_with(&format!("System.Threading.Channels.{kind}")) {
                what = format!("Channel {operation}");
                break;
            }
        }
        let excluded: BTreeSet<_> = ancestors.iter().copied().collect();
        let mut held = self.holder(object, &excluded, false)?;
        let mut resource = object;
        if name == "System.Net.Sockets.Socket+AwaitableSocketAsyncEventArgs"
            && let Some(owner) = snapshot.target(object, "_owner")
        {
            resource = owner;
            what = if snapshot.integer(owner, "_isListening").unwrap_or(0) != 0 {
                "Socket accept"
            } else if snapshot.integer(object, "_isReadForCaching").unwrap_or(0) != 0 {
                "Socket receive"
            } else {
                "Socket send"
            }
            .into();
        }
        if self.is_task_completion_source_task(object) {
            what = "TaskCompletionSource task".into();
            if let Some((holder, _)) = &held
                && snapshot.type_name(*holder).contains("TaskCompletionSource")
            {
                let mut excluded = excluded.clone();
                excluded.insert(object);
                held = self.holder(*holder, &excluded, false)?;
            }
        }
        let waiter_list = snapshot
            .names
            .fields(snapshot.heap.method_table(object).address)
            .iter()
            .any(|f| f.name == "Next" || f.name == "<Next>k__BackingField");
        if waiter_list
            && let Some((holder, _)) = &held
            && !self.methods.contains_key(holder)
        {
            resource = if snapshot.type_name(*holder).contains("Channel`") {
                snapshot.target(*holder, "_parent").unwrap_or(*holder)
            } else {
                *holder
            };
            let name = without_namespaces(&snapshot.type_name(resource));
            if !what.starts_with(&format!("{name}.")) {
                what += &format!(" on {name}");
            }
            held = self.resource_holder(resource, &excluded)?;
        }
        if let Some(&waiter) = ancestors.last() {
            self.waiters_of.entry(resource).or_default().insert(waiter);
        }
        let live = snapshot.graph.live[object] != 0;
        let never = live && self.held_only_by(&excluded, object);
        let held_text = if never {
            "held only by the calls above: nothing else can complete it".into()
        } else if let Some(holder) = &held {
            format!("held by {}", self.describe_holder(holder)?)
        } else if live {
            format!("held by {}", self.root_description(object)?)
        } else {
            String::new()
        };
        let leaf = Leaf {
            what,
            held: held_text,
            never,
            resource: (resource != object).then_some(resource),
            holder: held.map(|h| h.0),
        };
        if let (Some(&root), Some(&inner)) = (ancestors.first(), ancestors.last())
            && snapshot.graph.live[root] != 0
        {
            let key = (self.label(root)?, self.label(inner)?, leaf.text());
            self.waits.entry(key).or_insert((0, never)).0 += 1;
        }
        Ok(leaf)
    }

    fn item(&mut self, object: usize, path: Vec<usize>) -> Result<Item> {
        if self.methods.contains_key(&object)
            || self.children.get(&object).is_some_and(|c| !c.is_empty())
        {
            return Ok(Item::new(object, path, self.label(object)?, ItemKind::Node));
        }
        self.leaf_item(object, path)
    }

    fn leaf_item(&mut self, object: usize, path: Vec<usize>) -> Result<Item> {
        let leaf = self.leaf(object, &path)?;
        let mut item = Item::new(object, path, leaf.text(), ItemKind::Leaf);
        item.leaf = Some(leaf);
        Ok(item)
    }

    fn child_items(&mut self, item: &Item) -> Result<Vec<Item>> {
        match item.kind {
            ItemKind::Leaf | ItemKind::Cycle => Ok(Vec::new()),
            ItemKind::Thread { task } => Ok(vec![self.item(task, vec![item.object])?]),
            ItemKind::Node => {
                if item.path.len() >= 256 {
                    return Err(Error("async call graph exceeds 256 levels".into()));
                }
                let mut path = item.path.clone();
                path.push(item.object);
                let children = self.children.get(&item.object).cloned().unwrap_or_default();
                if children.is_empty()
                    && let Some(awaited) = self.methods.get(&item.object).and_then(|m| m.awaited)
                    && !path.contains(&awaited)
                {
                    return Ok(vec![self.leaf_item(awaited, path)?]);
                }
                let mut items = Vec::new();
                for &child in children.iter().filter(|c| !path.contains(c)) {
                    items.push(self.item(child, path.clone())?);
                }
                for child in children.into_iter().filter(|c| path.contains(c)) {
                    items.push(Item::new(
                        child,
                        path.clone(),
                        format!("{}, above: a cycle", self.label(child)?),
                        ItemKind::Cycle,
                    ));
                }
                Ok(items)
            }
        }
    }

    fn tree(&mut self, item: Item) -> Result<Tree> {
        self.remaining_nodes = self
            .remaining_nodes
            .checked_sub(1)
            .ok_or_else(|| Error("async call graph exceeds one million expanded nodes".into()))?;
        let children = self
            .child_items(&item)?
            .into_iter()
            .map(|child| self.tree(child))
            .collect::<Result<_>>()?;
        Ok(Tree { item, children })
    }

    fn merge(&self, trees: &[&Tree], merging: bool) -> Result<Vec<TaskNode>> {
        let mut positions = HashMap::new();
        let mut groups: Vec<Vec<&Tree>> = Vec::new();
        for (index, &tree) in trees.iter().enumerate() {
            let key = if merging {
                tree.item.label.clone()
            } else {
                index.to_string()
            };
            let position = *positions.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[position].push(tree);
        }
        let mut result = Vec::new();
        for group in groups {
            let first = &group[0].item;
            let mut seen = HashSet::new();
            let objects: Vec<_> = group
                .iter()
                .map(|t| t.item.object)
                .filter(|o| seen.insert(*o))
                .collect();
            let method = self.methods.get(&first.object);
            let mut kind = TaskNodeKind::Task;
            let mut title = first.label.clone();
            let mut details = Vec::new();
            let mut threads = Vec::new();
            let queued = objects.iter().any(|o| self.queued.contains(o));
            match first.kind {
                ItemKind::Node => {
                    if let Some(method) = method {
                        kind = TaskNodeKind::Method;
                        title = method.short.clone();
                        if method.state >= 0 {
                            details.push(format!(
                                "await {}{}",
                                method.state,
                                method
                                    .location
                                    .as_ref()
                                    .map(|l| format!(", {l}"))
                                    .unwrap_or_default()
                            ));
                        }
                        if method.state == -1 {
                            details.push("running".into());
                            threads = objects
                                .iter()
                                .map(|&o| self.thread_label(o))
                                .collect::<Result<_>>()?;
                        }
                        if queued {
                            details.push("queued for a thread pool thread".into());
                        }
                    } else {
                        title = self.label(first.object)?;
                    }
                    if first.path.len() == 1 && self.is_task_run(first.path[0]) {
                        details.push("in Task.Run".into());
                    }
                }
                ItemKind::Leaf => {
                    kind = TaskNodeKind::Suspension;
                    if let Some(leaf) = &first.leaf {
                        title = leaf.what.clone();
                        if !leaf.held.is_empty() {
                            details.push(leaf.held.clone());
                        }
                    }
                }
                ItemKind::Thread { .. } => {
                    kind = TaskNodeKind::Thread;
                    threads = objects
                        .iter()
                        .map(|&o| self.thread_label(o))
                        .collect::<Result<_>>()?;
                }
                ItemKind::Cycle => kind = TaskNodeKind::Cycle,
            }
            let mut holders: Vec<_> = group
                .iter()
                .filter_map(|t| t.item.held_by.as_ref().map(|h| h.1))
                .filter(|o| seen.insert(*o))
                .collect();
            let resources = group
                .iter()
                .filter_map(|t| t.item.leaf.as_ref().and_then(|l| l.resource))
                .filter(|o| seen.insert(*o))
                .collect();
            holders.extend(
                group
                    .iter()
                    .filter_map(|t| t.item.leaf.as_ref().and_then(|l| l.holder))
                    .filter(|o| seen.insert(*o)),
            );
            let this = objects
                .iter()
                .filter_map(|o| self.methods.get(o).and_then(|m| m.this))
                .collect();
            let children = self.merge(
                &group.iter().flat_map(|t| &t.children).collect::<Vec<_>>(),
                merging,
            )?;
            result.push(TaskNode {
                kind,
                objects,
                text: first.label.clone(),
                title,
                detail: details.join(", "),
                held_by: first.held_by.as_ref().map(|h| h.0.clone()),
                holders,
                resources,
                this,
                never_resumes: first.leaf.as_ref().is_some_and(|l| l.never),
                running: kind == TaskNodeKind::Method && method.is_some_and(|m| m.state == -1),
                queued: kind == TaskNodeKind::Method && queued,
                threads,
                children,
            });
        }
        result.sort_by(|a, b| {
            b.objects
                .len()
                .cmp(&a.objects.len())
                .then_with(|| a.text.cmp(&b.text))
        });
        Ok(result)
    }

    fn named(&self, object: usize) -> String {
        let snapshot = self.snapshot;
        let name = without_namespaces(&snapshot.type_name(object));
        if name == "Socket"
            && let Some(text) = snapshot.socket_text(object)
        {
            return format!("Socket {text}");
        }
        let socket = snapshot
            .field_values(object)
            .into_iter()
            .filter_map(|f| f.target)
            .find(|&o| snapshot.type_name(o) == "System.Net.Sockets.Socket");
        if let Some(text) = socket.and_then(|o| snapshot.socket_text(o)) {
            let id = snapshot
                .target(object, "_connectionId")
                .and_then(|o| snapshot.string_value(o, 100))
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            return format!("{name}{id} {text}");
        }
        format!("{name} 0x{:x}", snapshot.heap.addresses[object])
    }

    fn groups(&mut self, items: Vec<Item>, state: TaskState) -> Result<Vec<TaskGroup>> {
        let mut trees = items
            .into_iter()
            .map(|i| self.tree(i))
            .collect::<Result<Vec<_>>>()?;
        let sets: Vec<_> = trees.iter().map(Tree::objects).collect();
        let mut parents: Vec<_> = (0..trees.len()).collect();
        let mut first = HashMap::new();
        for (index, set) in sets.iter().enumerate() {
            for &object in set {
                if let Some(&other) = first.get(&object) {
                    let root = find(&mut parents, index);
                    parents[root] = find(&mut parents, other);
                } else {
                    first.insert(object, index);
                }
            }
        }
        let mut members: HashMap<usize, BTreeSet<usize>> = HashMap::new();
        for (index, set) in sets.into_iter().enumerate() {
            members
                .entry(find(&mut parents, index))
                .or_default()
                .extend(set);
        }
        for (index, tree) in trees.iter_mut().enumerate() {
            if matches!(tree.item.kind, ItemKind::Thread { .. }) {
                continue;
            }
            let top = tree.item.path.first().copied().unwrap_or(tree.item.object);
            let Some(holder) = self.holder(top, &members[&find(&mut parents, index)], false)?
            else {
                continue;
            };
            let name = self.snapshot.type_name(holder.0);
            let description = if name.ends_with(']') || name.starts_with("System.Collections.") {
                self.snapshot
                    .owning_field_with(holder.0, without_namespaces)?
            } else {
                self.describe_holder(&holder)?
            };
            let text = format!("task held by {description}");
            tree.item.label += &format!(", {text}");
            tree.item.held_by = Some((text, holder.0));
        }
        let mut by_graph: BTreeMap<usize, Vec<&Tree>> = BTreeMap::new();
        for (index, tree) in trees.iter().enumerate() {
            by_graph
                .entry(find(&mut parents, index))
                .or_default()
                .push(tree);
        }
        let mut shapes: BTreeMap<String, Vec<TaskGraph>> = BTreeMap::new();
        for list in by_graph.into_values() {
            let roots = self.merge(&list, true)?;
            let top = list[0].item.object;
            let shared = tasks::descendants(&roots)
                .filter(|n| n.kind == TaskNodeKind::Suspension)
                .find_map(|n| {
                    n.resources
                        .first()
                        .copied()
                        .or_else(|| (n.objects.len() == 1 && list.len() > 1).then(|| n.objects[0]))
                });
            let name = shared
                .or_else(|| self.methods.get(&top).and_then(|m| m.this))
                .map(|o| self.named(o))
                .or_else(|| roots[0].threads.first().cloned())
                .unwrap_or_else(|| {
                    format!(
                        "{} 0x{:x}",
                        roots[0].title, self.snapshot.heap.addresses[top]
                    )
                });
            let graph = TaskGraph::new(name, shared, roots, self.merge(&list, false)?);
            let key = format!(
                "{}{}",
                if graph.is_shared() { "shared|" } else { "" },
                shape(&graph.roots)
            );
            shapes.entry(key).or_default().push(graph);
        }
        Ok(shapes
            .into_values()
            .map(|mut graphs| {
                graphs.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
                let first = &graphs[0];
                let state = if state != TaskState::Waiting {
                    state
                } else if first.suspensions.iter().any(|n| n.never_resumes) {
                    TaskState::NeverResumes
                } else if first.running_calls > 0 {
                    TaskState::Running
                } else if first.queued_calls > 0 {
                    TaskState::Queued
                } else {
                    TaskState::Waiting
                };
                TaskGroup {
                    id: 0,
                    state,
                    graphs,
                }
            })
            .collect())
    }

    fn model(&mut self) -> Result<TaskGraphs> {
        let roots: Vec<_> = self
            .methods
            .keys()
            .chain(&self.tasks)
            .copied()
            .filter(|o| {
                !self.parents.contains_key(o)
                    && !self.sync_waits.contains_key(o)
                    && (self.methods.contains_key(o) || self.children.contains_key(o))
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut items = Vec::new();
        for &root in &roots {
            if self.is_task_run(root)
                && let Some(children) = self.children.get(&root).cloned()
            {
                for child in children {
                    let mut item = self.item(child, vec![root])?;
                    item.label += ", in Task.Run";
                    items.push(item);
                }
            } else {
                items.push(self.item(root, Vec::new())?);
            }
        }
        for (&task, waiters) in &self.sync_waits {
            for &waiter in waiters {
                items.push(Item::new(
                    waiter,
                    Vec::new(),
                    self.label(waiter)?,
                    ItemKind::Thread { task },
                ));
            }
        }
        let mut reached = HashSet::new();
        let reach = |object, reached: &mut HashSet<usize>| {
            let mut stack = vec![object];
            while let Some(current) = stack.pop() {
                if reached.insert(current) {
                    stack.extend(self.children.get(&current).into_iter().flatten());
                }
            }
        };
        for object in roots.into_iter().chain(self.sync_waits.keys().copied()) {
            reach(object, &mut reached);
        }
        let mut cycle_roots = Vec::new();
        for &object in self.methods.keys() {
            if !reached.contains(&object) && self.snapshot.graph.live[object] != 0 {
                reach(object, &mut reached);
                cycle_roots.push(object);
            }
        }
        let mut cycles = Vec::new();
        for object in cycle_roots {
            cycles.push(self.item(object, Vec::new())?);
        }
        let (live, dead): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| {
            let object = match item.kind {
                ItemKind::Thread { task } => task,
                _ => item.object,
            };
            self.snapshot.graph.live[object] != 0
        });
        let mut groups = self.groups(live, TaskState::Waiting)?;
        groups.extend(self.groups(cycles, TaskState::Cycle)?);
        groups.extend(self.groups(dead, TaskState::Dropped)?);
        groups.sort_by_cached_key(|g| {
            (
                g.state == TaskState::Dropped,
                std::cmp::Reverse(g.outermost_calls()),
                g.summary(),
            )
        });
        for (index, group) in groups.iter_mut().enumerate() {
            group.id = index;
        }
        Ok(TaskGraphs { groups })
    }
}

fn find(parents: &mut [usize], mut index: usize) -> usize {
    while parents[index] != index {
        parents[index] = parents[parents[index]];
        index = parents[index];
    }
    index
}

fn shape(nodes: &[TaskNode]) -> String {
    let mut children: Vec<_> = nodes
        .iter()
        .map(|n| format!("{}({})", n.text, shape(&n.children)))
        .collect();
    children.sort();
    children.join(",")
}

impl<'a> AsyncGraph<'a> {
    fn new(snapshot: &'a HeapSnapshot) -> Result<Self> {
        let mut graph = Self {
            snapshot,
            methods: BTreeMap::new(),
            tasks: BTreeSet::new(),
            task_types: snapshot.types_with_field("m_continuationObject"),
            pooled_types: snapshot.type_ids(|s| {
                s.contains("PoolingAsyncValueTaskMethodBuilder`1+StateMachineBox`1<")
            }),
            delegate_types: snapshot.types_with_field("_methodPtr"),
            parents: BTreeMap::new(),
            children: BTreeMap::new(),
            sync_waits: BTreeMap::new(),
            sync_waiters: HashSet::new(),
            queued: snapshot
                .queued_work_items()?
                .iter()
                .map(|w| w.object)
                .collect(),
            flags: HashMap::new(),
            waiters_of: BTreeMap::new(),
            waits: BTreeMap::new(),
            remaining_nodes: 1_000_000,
        };
        let boxes = snapshot.type_ids(|s| {
            s.contains("+AsyncStateMachineBox`1<")
                || s.contains("+DebugFinalizableAsyncStateMachineBox`1<")
        });
        let mut continuations = Vec::new();
        let tasks: Vec<_> = snapshot
            .objects(&graph.task_types)
            .into_par_iter()
            .filter_map(|task| {
                let flags = snapshot.integer(task, "m_stateFlags")? as u32;
                let (continuation, method) = if flags & COMPLETED == 0 {
                    (
                        snapshot.target(task, "m_continuationObject"),
                        boxes
                            .contains(&snapshot.heap.type_ids[task])
                            .then(|| graph.read_method(task, snapshot.field_values(task))),
                    )
                } else {
                    (None, None)
                };
                Some((task, flags, continuation, method))
            })
            .collect();
        for (task, flags, continuation, method) in tasks {
            graph.flags.insert(task, flags);
            if flags & COMPLETED != 0 {
                continue;
            }
            if let Some(continuation) = continuation {
                continuations.push((task, continuation));
            }
            if let Some(method) = method {
                graph.methods.insert(task, method);
            }
        }
        for object in snapshot.objects(&graph.pooled_types) {
            let fields = snapshot.field_values(object);
            let Some(machine) = fields.iter().find(|f| f.field.name == "StateMachine") else {
                continue;
            };
            let Some(source) = fields.iter().find(|f| f.field.name == "_valueTaskSource") else {
                continue;
            };
            if snapshot
                .integer_field(
                    source
                        .children
                        .iter()
                        .find(|f| f.field.name == "_completed"),
                )
                .unwrap_or(0)
                != 0
            {
                continue;
            }
            let Some(builder) = machine
                .children
                .iter()
                .find(|f| f.field.name == "<>t__builder")
            else {
                continue;
            };
            if ![0, 8].iter().any(|offset| {
                snapshot.dump.u64(builder.address + offset) == Some(snapshot.heap.addresses[object])
            }) {
                continue;
            }
            graph
                .methods
                .insert(object, graph.read_method(object, fields.clone()));
            let end = fields
                .iter()
                .map(|f| f.address)
                .filter(|&a| a > source.address)
                .min()
                .unwrap_or(snapshot.heap.addresses[object] + snapshot.heap.size(object));
            for address in (source.address..end.saturating_sub(7)).step_by(8) {
                if let Some(target) = snapshot
                    .dump
                    .u64(address)
                    .and_then(|v| snapshot.heap.object_at(v))
                {
                    continuations.push((object, target));
                }
            }
        }
        let awaits: Vec<_> = graph
            .methods
            .iter()
            .filter_map(|(&object, method)| {
                method
                    .awaited
                    .filter(|awaited| {
                        graph.methods.contains_key(awaited)
                            || graph.task_types.contains(&snapshot.heap.type_ids[*awaited])
                    })
                    .map(|awaited| (awaited, object))
            })
            .collect();
        for (child, parent) in awaits {
            graph.link(child, parent);
        }
        for (task, continuation) in continuations {
            for waiter in graph.resolve(Some(continuation), 0) {
                match waiter {
                    Waiter::Task(parent) => graph.link(task, parent),
                    Waiter::Thread(thread) => {
                        if graph.sync_waiters.insert(thread) {
                            graph.sync_waits.entry(task).or_default().push(thread);
                        }
                        if !graph.methods.contains_key(&task) {
                            graph.tasks.insert(task);
                        }
                    }
                }
            }
        }
        Ok(graph)
    }

    fn link(&mut self, child: usize, parent: usize) {
        if self
            .parents
            .get(&child)
            .is_some_and(|p| p.contains(&parent))
        {
            return;
        }
        self.parents.entry(child).or_default().push(parent);
        self.children.entry(parent).or_default().push(child);
        for task in [child, parent] {
            if !self.methods.contains_key(&task) {
                self.tasks.insert(task);
            }
        }
    }

    fn read_method(&self, object: usize, fields: Vec<FieldValue>) -> Method {
        let snapshot = self.snapshot;
        let (owner, method) = async_method(&snapshot.type_name(object));
        let suffix = if method.is_empty() {
            String::new()
        } else {
            format!(".{method}")
        };
        let mut result = Method {
            name: owner.clone() + &suffix,
            short: without_namespace(&owner) + &suffix,
            state: -2,
            awaited: None,
            this: None,
            location: None,
        };
        let Some(machine) = fields.into_iter().find(|f| f.field.name == "StateMachine") else {
            return result;
        };
        let owner = if machine.children.is_empty() {
            machine.target.unwrap_or(object)
        } else {
            object
        };
        let end = snapshot.heap.addresses[owner] + snapshot.heap.size(owner);
        let machine_type = machine.field.value_type.or_else(|| {
            machine
                .target
                .map(|o| snapshot.heap.method_table(o).address)
        });
        let fields = if machine.children.is_empty() {
            machine
                .target
                .map(|o| snapshot.field_values(o))
                .unwrap_or_default()
        } else {
            machine.children
        };
        result.state = snapshot
            .integer_field(fields.iter().find(|f| f.field.name == "<>1__state"))
            .unwrap_or(-2);
        result.this = fields
            .iter()
            .find(|f| f.field.name == "<>4__this")
            .and_then(|f| field_targets(f).next());
        result.awaited = fields
            .iter()
            .filter(|f| f.field.name.starts_with("<>u__"))
            .find_map(|field| {
                field_targets(field).next().or_else(|| {
                    let next = fields
                        .iter()
                        .map(|f| f.address)
                        .filter(|&a| a > field.address)
                        .min()
                        .unwrap_or(field.address.saturating_add(64))
                        .min(end);
                    (field.address..next.saturating_sub(7))
                        .step_by(8)
                        .find_map(|a| {
                            snapshot
                                .dump
                                .u64(a)
                                .and_then(|v| snapshot.heap.object_at(v))
                        })
                })
            });
        if result.state >= 0 {
            result.location = machine_type.and_then(|mt| {
                snapshot
                    .names
                    .await_locations(mt)
                    .get(&(result.state as i32))
                    .cloned()
            });
        }
        result
    }

    fn resolve(&self, object: Option<usize>, depth: usize) -> Vec<Waiter> {
        let Some(object) = object.filter(|_| depth < 6) else {
            return Vec::new();
        };
        let snapshot = self.snapshot;
        let type_id = snapshot.heap.type_ids[object];
        if self.task_types.contains(&type_id) || self.pooled_types.contains(&type_id) {
            return vec![Waiter::Task(object)];
        }
        let name = snapshot.type_name(object);
        if name == "System.Collections.Generic.List`1<System.Object>" {
            let count = snapshot.integer(object, "_size").unwrap_or(0).max(0) as usize;
            return snapshot
                .elements(snapshot.target(object, "_items"))
                .into_iter()
                .take(count)
                .flat_map(|o| self.resolve(o, depth + 1))
                .collect();
        }
        if name.ends_with("+SetOnInvokeMres") || name.ends_with("+SetOnCountdownMres") {
            return vec![Waiter::Thread(object)];
        }
        if self.delegate_types.contains(&type_id) {
            return self.resolve(snapshot.target(object, "_target"), depth + 1);
        }
        if name.contains("Continuation") || name.contains("Wrapper") {
            for field in ["m_task", "m_action", "_continuation", "_innerTask"] {
                let waiters = self.resolve(snapshot.target(object, field), depth + 1);
                if !waiters.is_empty() {
                    return waiters;
                }
            }
        }
        Vec::new()
    }

    fn is_task_run(&self, object: usize) -> bool {
        self.snapshot.type_name(object).contains("UnwrapPromise`1<")
            && self.snapshot.integer(object, "_lookForOce").unwrap_or(0) != 0
    }

    pub fn label(&self, object: usize) -> Result<String> {
        let snapshot = self.snapshot;
        if self.sync_waiters.contains(&object) {
            return Ok("waited for synchronously (Task.Wait, GetResult)".into());
        }
        if let Some(method) = self.methods.get(&object) {
            return Ok(method.short.clone() + &method.position());
        }
        let name = snapshot.type_name(object);
        let mut label = if name.contains("+WhenAllPromise") {
            "Task.WhenAll".into()
        } else if name.contains("+TwoTaskWhenAnyPromise`1<")
            || name.contains("+CompleteOnInvokePromise`1<")
        {
            "Task.WhenAny".into()
        } else if name.contains("UnwrapPromise`1<") {
            if self.is_task_run(object) {
                "Task.Run".into()
            } else {
                "Task.Unwrap".into()
            }
        } else if name.contains(".ContinuationTaskFrom")
            || name.contains(".ContinuationResultTaskFrom")
        {
            let method = snapshot
                .target(object, "m_action")
                .map(|o| snapshot.delegate_method(o))
                .transpose()?
                .flatten();
            format!(
                "ContinueWith{}",
                method
                    .map(|s| format!(" {}", without_namespace(&s)))
                    .unwrap_or_default()
            )
        } else if name.contains("+CancellationPromise`1<") {
            "Task.WaitAsync".into()
        } else if name.contains("+CompleteOnCountdownPromise") {
            "TaskFactory.ContinueWhenAll".into()
        } else if name.ends_with("+DelayPromise") || name.ends_with("+DelayPromiseWithCancellation")
        {
            "Task.Delay".into()
        } else if name == "System.Threading.SemaphoreSlim+TaskNode" {
            "SemaphoreSlim.WaitAsync".into()
        } else if name.contains("+ValueTaskSourceAsTask") {
            "ValueTask.AsTask".into()
        } else if let Some(action) = snapshot.target(object, "m_action") {
            format!(
                "Task running {}",
                snapshot
                    .delegate_method(action)?
                    .map(|s| without_namespace(&s))
                    .unwrap_or("a delegate".into())
            )
        } else {
            without_namespaces(&name)
        };
        if self.flags.get(&object).copied().unwrap_or(0) & COMPLETED != 0 {
            label += " (completed; the continuation has not run yet)";
        }
        Ok(label)
    }

    pub fn holder(
        &self,
        object: usize,
        excluding: &BTreeSet<usize>,
        parts: bool,
    ) -> Result<Option<(usize, String)>> {
        let snapshot = self.snapshot;
        let type_id = snapshot.heap.type_ids[object];
        let mut queue = vec![object];
        let mut seen = HashSet::from([object]);
        let mut callback = None;
        let mut call = None;
        let mut index = 0;
        while index < queue.len() && index < 100_000 {
            let current = queue[index];
            index += 1;
            for &parent in snapshot.graph.referrer_rows.row(current) {
                let parent = parent as usize;
                if snapshot.graph.live[parent] != snapshot.graph.live[object]
                    || excluding.contains(&parent)
                    || !seen.insert(parent)
                {
                    continue;
                }
                let parent_type = snapshot.heap.type_ids[parent];
                if parent_type == type_id || self.delegate_types.contains(&parent_type) {
                    queue.push(parent);
                    continue;
                }
                if self.methods.contains_key(&parent)
                    || self.tasks.contains(&parent)
                    || self.sync_waiters.contains(&parent)
                {
                    if self.methods.contains_key(&parent) {
                        call.get_or_insert((parent, String::new()));
                    }
                    continue;
                }
                if parts
                    && snapshot
                        .graph
                        .referrer_rows
                        .row(parent)
                        .contains(&(current as i32))
                {
                    continue;
                }
                if snapshot.type_name(parent)
                    == "System.Threading.CancellationTokenSource+CallbackNode"
                {
                    callback.get_or_insert((parent, ".CallbackState".into()));
                    continue;
                }
                return Ok(Some((
                    parent,
                    snapshot
                        .reference_name(parent, current)?
                        .as_deref()
                        .map(property_name)
                        .unwrap_or_default(),
                )));
            }
        }
        Ok(callback.or(call))
    }

    pub fn resource_holder(
        &self,
        object: usize,
        excluding: &BTreeSet<usize>,
    ) -> Result<Option<(usize, String)>> {
        if let Some(holder) = self.holder(object, excluding, true)? {
            return Ok(Some(holder));
        }
        let snapshot = self.snapshot;
        let mut parts: Vec<_> = snapshot
            .graph
            .referrer_rows
            .row(object)
            .iter()
            .map(|&o| o as usize)
            .filter(|&o| snapshot.target(o, "_parent") == Some(object))
            .collect();
        parts.sort_by_key(|&o| !snapshot.type_name(o).contains("Reader<"));
        let mut excluding = excluding.clone();
        excluding.insert(object);
        for part in parts {
            if let Some(holder) = self.holder(part, &excluding, false)? {
                return Ok(Some(holder));
            }
        }
        Ok(None)
    }

    pub fn is_task_completion_source_task(&self, object: usize) -> bool {
        let snapshot = self.snapshot;
        let name = snapshot.type_name(object);
        (name == "System.Threading.Tasks.Task"
            || name.starts_with("System.Threading.Tasks.Task`1<"))
            && snapshot.graph.referrer_rows.row(object).iter().any(|&p| {
                snapshot
                    .type_name(p as usize)
                    .contains("TaskCompletionSource")
            })
    }

    pub fn describe_holder(&self, holder: &(usize, String)) -> Result<String> {
        if self.methods.contains_key(&holder.0) {
            return Ok(format!("the waiting call {}", self.label(holder.0)?));
        }
        if holder.1.starts_with("static ") {
            return Ok(holder.1.clone());
        }
        Ok(without_namespaces(&self.snapshot.type_name(holder.0)) + &holder.1)
    }

    pub fn root_description(&self, object: usize) -> Result<String> {
        let Some(&source) = self.snapshot.graph.sources_of.row(object).first() else {
            return Ok("nothing".into());
        };
        let text = self.snapshot.describe_source(source as usize)?;
        Ok(text
            .rsplit_once(" (0x")
            .map(|(head, _)| head.to_owned())
            .unwrap_or(text))
    }

    pub fn thread_label(&self, object: usize) -> Result<String> {
        let snapshot = self.snapshot;
        let stacks = snapshot.thread_stacks()?;
        for &source in snapshot.graph.sources_of.row(object) {
            if let RootKind::Stack { tid } = snapshot.graph.sources[source as usize].kind {
                return Ok(stacks.label(tid));
            }
        }
        let address = snapshot.heap.addresses[object];
        for thread in &stacks.threads {
            if (thread.stack_pointer & !7..thread.stack_end.saturating_sub(7))
                .step_by(8)
                .any(|a| snapshot.dump.u64(a) == Some(address))
            {
                return Ok(stacks.label(thread.tid));
            }
        }
        Ok("a thread".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cache::CacheChoice, graph::AnalysisOptions};
    use std::{path::Path, process::Command};

    #[test]
    fn cached_findings_do_not_load_the_task_graph() {
        let directory = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/generate-compatibility-fixtures.py");
        assert!(
            Command::new(if cfg!(windows) { "python" } else { "python3" })
                .arg(script)
                .arg(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let open = || {
            HeapSnapshot::open(
                &directory.path().join("reference-rooted.dmp"),
                Vec::new(),
                None,
                AnalysisOptions::default(),
                CacheChoice::Folder(directory.path().join("cache")),
                |_, _| {},
            )
            .unwrap()
        };
        let expected = serde_json::to_value(open().async_check().unwrap().as_ref()).unwrap();
        let snapshot = open();
        assert!(
            snapshot
                .memo::<AsyncAnalysis>("async graphs", || Err(Error(
                    "task graph must not load".into()
                )))
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(snapshot.async_check().unwrap().as_ref()).unwrap(),
            expected
        );
    }

    fn method(name: &str, awaited: Option<usize>) -> Method {
        Method {
            name: name.into(),
            short: name.into(),
            state: 0,
            awaited,
            this: None,
            location: None,
        }
    }

    #[test]
    fn task_graphs_preserve_cycles_shared_waits_and_dropped_calls() {
        let directory = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/generate-compatibility-fixtures.py");
        assert!(
            Command::new("python3")
                .arg(script)
                .arg(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let open = |name: &str| {
            HeapSnapshot::open(
                &directory.path().join(name),
                Vec::new(),
                None,
                AnalysisOptions::default(),
                CacheChoice::None,
                |_, _| {},
            )
            .unwrap()
        };
        let snapshot = open("reference-array-rooted.dmp");

        for edges in [vec![(0, 0)], vec![(0, 1), (1, 0)]] {
            let mut graph = AsyncGraph::new(&snapshot).unwrap();
            for &(child, parent) in &edges {
                graph
                    .methods
                    .insert(parent, method(&format!("Worker{parent}.Run"), Some(child)));
            }
            for (child, parent) in edges {
                graph.link(child, parent);
            }
            let model = graph.model().unwrap();
            assert_eq!(model.groups.len(), 1);
            assert_eq!(model.groups[0].state, TaskState::Cycle);
            let cycle = &model.groups[0].graphs[0];
            assert_eq!(cycle.calls, graph.methods.len());
            assert_eq!(
                tasks::descendants(&cycle.roots)
                    .filter(|n| n.kind == TaskNodeKind::Cycle)
                    .count(),
                1
            );
            assert!(!graph.report(&model).unwrap().findings.is_empty());
        }

        let mut graph = AsyncGraph::new(&snapshot).unwrap();
        graph.methods.insert(0, method("First.Run", Some(2)));
        graph.methods.insert(1, method("Second.Run", Some(2)));
        let model = graph.model().unwrap();
        assert_eq!(model.groups.len(), 1);
        assert_eq!(model.groups[0].outermost_calls(), 2);
        assert!(model.groups[0].graphs[0].is_shared());
        assert_eq!(model.groups[0].graphs[0].waited_on, Some(2));
        assert_eq!(model.containing(2), Some((0, 0)));
        assert_eq!(model.awaiters().len(), 1);
        assert_eq!(graph.waiters_of[&2], BTreeSet::from([0, 1]));

        let snapshot = open("reference-array-dead.dmp");
        let mut graph = AsyncGraph::new(&snapshot).unwrap();
        graph.methods.insert(0, method("Dropped.Run", Some(1)));
        let model = graph.model().unwrap();
        assert_eq!(model.groups[0].state, TaskState::Dropped);
        assert_eq!(model.groups[0].outermost_calls(), 1);
        graph.remaining_nodes = 0;
        assert!(
            graph
                .model()
                .unwrap_err()
                .to_string()
                .contains("expanded nodes")
        );
    }
}
