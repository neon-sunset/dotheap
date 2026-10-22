use crate::{analysis::HeapSnapshot, queries::FieldValue};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskNodeKind {
    Method,
    Task,
    Thread,
    Suspension,
    Cycle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNode {
    pub kind: TaskNodeKind,
    pub objects: Vec<usize>,
    pub text: String,
    pub title: String,
    pub detail: String,
    pub held_by: Option<String>,
    pub holders: Vec<usize>,
    pub resources: Vec<usize>,
    pub this: Vec<usize>,
    pub never_resumes: bool,
    pub running: bool,
    pub queued: bool,
    pub threads: Vec<String>,
    pub children: Vec<TaskNode>,
}

impl TaskNode {
    pub fn is_call(&self) -> bool {
        matches!(
            self.kind,
            TaskNodeKind::Method | TaskNodeKind::Task | TaskNodeKind::Thread
        )
    }

    pub fn call_text(&self) -> String {
        if self.detail.is_empty() || self.kind == TaskNodeKind::Suspension {
            self.title.clone()
        } else {
            format!("{} ({})", self.title, self.detail)
        }
    }
}

pub fn descendants(roots: &[TaskNode]) -> impl Iterator<Item = &TaskNode> {
    let mut stack: Vec<_> = roots.iter().rev().collect();
    std::iter::from_fn(move || {
        let node = stack.pop()?;
        stack.extend(node.children.iter().rev());
        Some(node)
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraph {
    pub name: String,
    pub waited_on: Option<usize>,
    pub roots: Vec<TaskNode>,
    pub trees: Vec<TaskNode>,
    pub calls: usize,
    pub suspensions: Vec<TaskNode>,
    pub running_calls: usize,
    pub queued_calls: usize,
}

impl TaskGraph {
    pub fn new(
        name: String,
        waited_on: Option<usize>,
        roots: Vec<TaskNode>,
        trees: Vec<TaskNode>,
    ) -> Self {
        Self {
            name,
            waited_on,
            calls: descendants(&roots)
                .filter(|n| n.is_call())
                .map(|n| n.objects.len())
                .sum(),
            suspensions: descendants(&roots)
                .filter(|n| n.kind == TaskNodeKind::Suspension)
                .cloned()
                .collect(),
            running_calls: descendants(&roots)
                .filter(|n| n.running)
                .map(|n| n.objects.len())
                .sum(),
            queued_calls: descendants(&roots)
                .filter(|n| n.queued)
                .map(|n| n.objects.len())
                .sum(),
            roots,
            trees,
        }
    }

    pub fn tasks(&self) -> usize {
        self.roots.iter().map(|n| n.objects.len()).sum()
    }

    pub fn is_shared(&self) -> bool {
        self.waited_on.is_some() && self.tasks() > 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Running,
    Queued,
    Waiting,
    #[serde(rename = "never resumes")]
    NeverResumes,
    #[serde(rename = "waits on itself")]
    Cycle,
    Dropped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskGroup {
    pub id: usize,
    pub state: TaskState,
    pub graphs: Vec<TaskGraph>,
}

impl TaskGroup {
    pub fn outermost_calls(&self) -> usize {
        self.graphs.iter().map(TaskGraph::tasks).sum()
    }

    pub fn outermost(&self) -> String {
        self.graphs
            .first()
            .map(|g| list(g.roots.iter().map(TaskNode::call_text)))
            .unwrap_or_default()
    }

    pub fn suspended_on(&self) -> String {
        let Some(graph) = self.graphs.first() else {
            return String::new();
        };
        if !graph.suspensions.is_empty() {
            return list(graph.suspensions.iter().map(|n| n.title.clone()));
        }
        let cycles = list(
            descendants(&graph.roots)
                .filter(|n| n.kind == TaskNodeKind::Cycle)
                .map(|n| n.title.clone()),
        );
        if !cycles.is_empty() {
            return cycles;
        }
        if graph.running_calls > 0 {
            "running".into()
        } else if graph.queued_calls > 0 {
            "queued for a thread pool thread".into()
        } else {
            String::new()
        }
    }

    pub fn summary(&self) -> String {
        let Some(graph) = self.graphs.first() else {
            return String::new();
        };
        let mut level: Vec<_> = graph.roots.iter().collect();
        let mut parts = Vec::new();
        while !level.is_empty() {
            parts.push(list(level.iter().map(|n| n.text.clone())));
            level = level.into_iter().flat_map(|n| &n.children).collect();
        }
        if parts.len() > 4 {
            let last = parts.pop().unwrap();
            parts.truncate(2);
            parts.extend(["...".into(), last]);
        }
        parts.join(" → ")
    }
}

fn list(items: impl IntoIterator<Item = String>) -> String {
    let mut seen = HashSet::new();
    let unique: Vec<_> = items
        .into_iter()
        .filter(|s| seen.insert(s.clone()))
        .collect();
    let mut text = unique
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join(" + ");
    if unique.len() > 2 {
        text += &format!(" + {} more", unique.len() - 2);
    }
    text
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskGraphs {
    pub groups: Vec<TaskGroup>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Awaiter {
    pub call: String,
    pub object: usize,
    pub outermost: String,
}

impl TaskGraphs {
    pub fn awaiters(&self) -> HashMap<usize, Awaiter> {
        let mut result = HashMap::new();
        for root in self
            .groups
            .iter()
            .flat_map(|g| &g.graphs)
            .flat_map(|g| &g.trees)
        {
            let mut stack = vec![(root, None::<&TaskNode>)];
            while let Some((node, parent)) = stack.pop() {
                if node.kind == TaskNodeKind::Suspension
                    && let Some(parent) = parent.filter(|p| p.is_call())
                    && let Some(&object) = parent.objects.first()
                {
                    for &awaited in &node.objects {
                        result.entry(awaited).or_insert_with(|| Awaiter {
                            call: parent.call_text(),
                            object,
                            outermost: root.call_text(),
                        });
                    }
                }
                stack.extend(node.children.iter().rev().map(|n| (n, Some(node))));
            }
        }
        result
    }

    pub fn containing(&self, object: usize) -> Option<(usize, usize)> {
        for (group_index, group) in self.groups.iter().enumerate() {
            for (graph_index, graph) in group.graphs.iter().enumerate() {
                if descendants(&graph.roots).any(|n| {
                    n.objects.contains(&object)
                        || n.this.contains(&object)
                        || n.resources.contains(&object)
                }) {
                    return Some((group_index, graph_index));
                }
            }
        }
        None
    }
}

pub fn field_name(field: &str) -> Option<String> {
    match field {
        "<>1__state" | "<>t__builder" => None,
        "<>4__this" => Some("this".into()),
        _ => {
            if let Some(number) = field.strip_prefix("<>u__") {
                return Some(format!("awaiter {number}"));
            }
            if let Some(number) = field.strip_prefix("<>7__wrap") {
                return Some(format!("temporary {number}"));
            }
            if let Some(rest) = field.strip_prefix('<')
                && let Some((name, _)) = rest.split_once(">5__")
            {
                return Some(name.into());
            }
            Some(field.into())
        }
    }
}

pub fn without_namespace(name: &str) -> String {
    let head = name.split(['<', '[', '(']).next().unwrap_or(name);
    head.rfind('.')
        .map(|i| name[i + 1..].to_owned())
        .unwrap_or_else(|| name.to_owned())
}

pub fn async_method(box_type: &str) -> (String, String) {
    let Some((_, arguments)) = box_type.split_once("StateMachineBox`1<") else {
        return (box_type.into(), String::new());
    };
    let mut depth = 0i32;
    let mut machine = arguments;
    for (offset, ch) in arguments.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                machine = &arguments[offset + 1..];
                break;
            }
            _ => {}
        }
    }
    machine = machine.trim();
    machine = machine.strip_suffix('>').unwrap_or(machine);
    if machine.ends_with('>') {
        let mut depth = 0;
        for (offset, ch) in machine.char_indices().rev() {
            if ch == '>' {
                depth += 1;
            } else if ch == '<' {
                depth -= 1;
                if depth == 0 {
                    machine = &machine[..offset];
                    break;
                }
            }
        }
    }
    let Some((mut owner, segment)) = machine.rsplit_once("+<") else {
        return (machine.into(), String::new());
    };
    for closure in ["+<>c__DisplayClass", "+<>c"] {
        owner = owner
            .split_once(closure)
            .map(|(head, _)| head)
            .unwrap_or(owner);
    }
    let method = if let Some(inner) = segment.strip_prefix('<')
        && let Some((outer, rest)) = inner.split_once('>')
    {
        match rest.strip_prefix("g__") {
            Some(local) => format!("{outer}.{}", local.split('|').next().unwrap_or(local)),
            None => format!("{outer} (lambda)"),
        }
    } else {
        segment.split('>').next().unwrap_or(segment).into()
    };
    (owner.into(), method)
}

pub(crate) fn field_targets(field: &FieldValue) -> impl Iterator<Item = usize> + '_ {
    let mut stack = vec![field];
    std::iter::from_fn(move || {
        while let Some(field) = stack.pop() {
            stack.extend(field.children.iter().rev());
            if field.target.is_some() {
                return field.target;
            }
        }
        None
    })
}

impl HeapSnapshot {
    pub fn state_machine_fields(&self, object: usize) -> Vec<(FieldValue, Vec<usize>)> {
        let Some(machine) = self.field(object, "StateMachine") else {
            return Vec::new();
        };
        let end_object = if machine.children.is_empty() {
            machine.target.unwrap_or(object)
        } else {
            object
        };
        let end = self.heap.addresses[end_object] + self.heap.size(end_object);
        let fields = if machine.children.is_empty() {
            machine
                .target
                .map(|o| self.field_values(o))
                .unwrap_or_default()
        } else {
            machine.children
        };
        let mut result = Vec::new();
        for field in &fields {
            let mut objects: Vec<_> = field_targets(field).collect();
            if objects.is_empty()
                && field.children.is_empty()
                && !field.field.is_reference()
                && field.field.primitive_size().is_none()
            {
                let next = fields
                    .iter()
                    .map(|f| f.address)
                    .filter(|&a| a > field.address)
                    .min()
                    .unwrap_or(end)
                    .min(end);
                for address in (field.address..next.saturating_sub(7)).step_by(8) {
                    if let Some(object) =
                        self.dump.u64(address).and_then(|v| self.heap.object_at(v))
                    {
                        objects.push(object);
                    }
                }
            }
            result.push((field.clone(), objects));
        }
        result
    }
}
