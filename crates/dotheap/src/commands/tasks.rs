use super::*;
use heap_core::{
    tasks::{TaskNode, TaskNodeKind, descendants, field_name},
    threads::StackNode,
};
use std::collections::BTreeMap;

pub fn threads(c: &Context<'_>) -> Result<Value> {
    let stacks = c.snapshot.thread_stacks()?;
    if !c.arguments.positional.is_empty() {
        let tid = c.positional(0, "tid")?;
        let t = tid
            .parse()
            .ok()
            .and_then(|tid| stacks.thread(tid))
            .ok_or_else(|| Error(format!("no thread {tid} in the dump")))?;
        let frames = std::iter::once((t.stack_pointer, t.instruction_pointer))
            .chain(t.frames.iter().map(|f| (f.slot, f.return_address)));
        return Ok(record([
            ("tid", Value::Id(t.tid as i64)),
            ("name", optional(t.name.as_ref())),
            ("stackPointer", address(t.stack_pointer)),
            ("deepestCallUsed", bytes(t.touched_bytes())),
            ("inUse", bytes(t.in_use_bytes())),
            (
                "managedThread",
                optional_address(t.managed_thread.map(|i| c.snapshot.heap.addresses[i])),
            ),
            (
                "frames",
                limited(frames, c.limit(), |(slot, code)| {
                    record([
                        ("slot", address(slot)),
                        ("code", address(code)),
                        ("function", text(stacks.describe(code))),
                    ])
                }),
            ),
            (
                "note",
                text(
                    "frames are estimated from return addresses on the stack; an earlier, deeper \
                        call can leave one behind",
                ),
            ),
        ]));
    }
    let mut threads: Vec<_> = stacks.threads.iter().collect();
    threads.sort_by(|a, b| {
        b.touched_bytes()
            .cmp(&a.touched_bytes())
            .then(a.tid.cmp(&b.tid))
    });
    Ok(record([(
        "threads",
        limited(threads, c.limit(), |t| {
            record([
                ("tid", Value::Id(t.tid as i64)),
                ("deepestCallUsed", bytes(t.touched_bytes())),
                ("inUse", bytes(t.in_use_bytes())),
                ("frames", int(t.frames.len())),
                ("name", text(t.name.as_deref().unwrap_or(""))),
                ("current", text(stacks.current_frame(t).unwrap_or_default())),
            ])
        }),
    )]))
}

pub fn stacks(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let stacks = s.thread_stacks()?;
    let states = s.thread_states()?;
    let states: BTreeMap<_, _> = states.iter().map(|t| (t.tid, t.state())).collect();
    fn node(
        n: &StackNode,
        states: &BTreeMap<i32, &str>,
        stacks: &heap_core::thread_stacks::ThreadStacks,
        limit: usize,
    ) -> Value {
        let mut counts = BTreeMap::<&str, usize>::new();
        for tid in &n.threads {
            *counts
                .entry(states.get(tid).copied().unwrap_or("other"))
                .or_default() += 1;
        }
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let frames = match n.frames.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            frames => format!(
                "{} ... {} ({} frames)",
                frames[0],
                frames.last().unwrap(),
                frames.len()
            ),
        };
        let mut out = record([
            ("threads", int(n.threads.len())),
            (
                "state",
                text(
                    counts
                        .iter()
                        .map(|(state, count)| format!("{count} {state}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ),
            ("frames", text(frames)),
        ]);
        if n.children.is_empty() {
            out.add(
                "threadIds",
                text(
                    n.threads
                        .iter()
                        .take(5)
                        .map(|&tid| stacks.label(tid))
                        .collect::<Vec<_>>()
                        .join(", ")
                        + if n.threads.len() > 5 { ", ..." } else { "" },
                ),
            );
        } else {
            out.add(
                "children",
                limited(&n.children, limit, |n| node(n, states, stacks, limit)),
            );
        }
        out
    }
    Ok(record([
        (
            "stacks",
            limited(s.merged_stacks()?.iter(), c.limit(), |n| {
                node(n, &states, &stacks, c.limit())
            }),
        ),
        (
            "note",
            text(
                "Frames are estimated from return addresses on the stack; stale calls can add \
                    frames. Frames shared by the same threads form one branch, ordered from \
                    outermost to innermost.",
            ),
        ),
    ]))
}

pub fn tasks(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let analysis = s.async_analysis()?;
    let model = &analysis.graphs;
    let object = |i| address(s.heap.addresses[i]);
    if c.arguments.positional.is_empty() {
        return Ok(record([
            (
                "groups",
                limited(&model.groups, c.limit(), |g| {
                    record([
                        ("tasks", int(g.outermost_calls())),
                        ("objectsWaitedOn", int(g.graphs.len())),
                        ("state", text(enum_text(g.state))),
                        ("example", object(g.graphs[0].roots[0].objects[0])),
                        ("outermostCall", text(g.outermost())),
                        ("innermostCallWaitsOn", text(g.suspended_on())),
                    ])
                }),
            ),
            (
                "note",
                text(
                    "each row is the tasks with the same async stack; objectsWaitedOn is how \
                        many different objects they wait on",
                ),
            ),
            (
                "next",
                text(format!(
                    "dotheap tasks {} <example address>: the tasks that wait on the same object \
                        as that task, with the objects of each call",
                    c.core_word()
                )),
            ),
        ]));
    }
    let a = c.address(0)?;
    let (group, graph) = s
        .heap
        .object_containing(a)
        .and_then(|i| model.containing(i))
        .ok_or_else(|| {
            Error(format!(
                "{} is not in a task graph (a box of a waiting async method, a task, \
                 what a call awaits, or its channel or semaphore); see dotheap tasks {}",
                hex(a),
                c.core_word()
            ))
        })?;
    let group = &model.groups[group];
    let graph = &group.graphs[graph];
    fn tree(n: &TaskNode, c: &Context<'_>) -> Value {
        let mut out = record([
            ("count", int(n.objects.len())),
            (
                "example",
                optional_address(n.objects.first().map(|&i| c.snapshot.heap.addresses[i])),
            ),
            ("call", text(&n.text)),
        ]);
        if !n.children.is_empty() {
            out.add("children", list(n.children.iter().map(|n| tree(n, c))));
        }
        out
    }
    let mut details = Vec::new();
    for n in descendants(&graph.roots) {
        let Some(&first) = n.objects.first() else {
            continue;
        };
        let mut out = record([
            ("call", text(&n.text)),
            (
                "objects",
                limited(n.objects.iter().copied(), c.limit(), object),
            ),
        ]);
        if n.kind == TaskNodeKind::Method {
            out.add(
                "stateMachine",
                list(
                    s.state_machine_fields(first)
                        .into_iter()
                        .filter_map(|(field, objects)| {
                            let name = field_name(&field.field.name)?;
                            let value = if objects.is_empty() {
                                field.display
                            } else {
                                objects
                                    .iter()
                                    .map(|&i| {
                                        format!("{} {}", hex(s.heap.addresses[i]), s.type_name(i))
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            };
                            Some(text(format!("{name} = {value}")))
                        }),
                ),
            );
        }
        if !n.threads.is_empty() {
            out.add(
                if n.running { "runsOn" } else { "threads" },
                list(n.threads.iter().map(text)),
            );
        }
        if !n.resources.is_empty() {
            out.add("on", list(n.resources.iter().copied().map(object)));
        }
        if !n.holders.is_empty() {
            out.add("holders", list(n.holders.iter().copied().map(object)));
        }
        if s.graph.live[first] != 0 {
            let (source, path) = s.graph.path_to_root(first);
            let mut steps = vec![text(
                source
                    .map(|k| s.describe_source(k))
                    .transpose()?
                    .unwrap_or_else(|| "no root".into()),
            )];
            for (index, &i) in path.iter().enumerate() {
                let field = if index == 0 {
                    None
                } else {
                    s.reference_name(path[index - 1], i)?
                };
                steps.push(text(format!(
                    "{} {}{}",
                    hex(s.heap.addresses[i]),
                    s.type_name(i),
                    field.map(|f| format!(" ({f})")).unwrap_or_default()
                )));
            }
            out.add("pathFromRoot", list(steps));
        }
        details.push(out);
    }
    Ok(record([
        (
            "waitedOn",
            if graph.waited_on.is_some() {
                text(&graph.name)
            } else {
                Value::Null
            },
        ),
        (
            "task",
            if graph.waited_on.is_none() {
                text(&graph.name)
            } else {
                Value::Null
            },
        ),
        ("state", text(enum_text(group.state))),
        ("tasks", int(graph.tasks())),
        ("calls", int(graph.calls)),
        ("objectsWaitedOnWithThisAsyncStack", int(group.graphs.len())),
        ("tree", list(graph.roots.iter().map(|n| tree(n, c)))),
        ("details", list(details)),
    ]))
}
