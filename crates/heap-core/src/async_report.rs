use crate::{
    Result,
    async_graph::AsyncGraph,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding},
    tasks::{TaskGraphs, TaskNode, TaskState},
    type_names::without_namespaces,
};
use std::collections::{BTreeMap, HashSet};

fn row(node: &TaskNode) -> CheckRow {
    let mut seen: HashSet<_> = node.objects.iter().copied().collect();
    let others = node
        .this
        .iter()
        .chain(&node.holders)
        .chain(&node.resources)
        .copied()
        .filter(|o| seen.insert(*o))
        .take(20);
    CheckRow {
        cells: vec![Cell::int(node.objects.len()), Cell::text(&node.text)],
        samples: node
            .objects
            .iter()
            .copied()
            .take(50)
            .chain(others)
            .collect(),
        children: node.children.iter().map(row).collect(),
    }
}

fn counted_names(counts: &BTreeMap<String, usize>, limit: usize) -> String {
    let mut items: Vec<_> = counts.iter().collect();
    items.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
    items
        .into_iter()
        .take(limit)
        .map(|(name, count)| format!("{name} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl AsyncGraph<'_> {
    fn shared_waits(&self) -> Result<CheckSection> {
        let snapshot = self.snapshot;
        let mut section = CheckSection::new(
            "Objects that more than one call waits on",
            "Channels and SemaphoreSlim objects (through their lists of waiting readers, writers \
                or WaitAsync calls) and tasks awaited by more than one call.",
            &["calls", "object", "held by", "waiting calls"],
        );
        for (&object, waiters) in self.waiters_of.iter().filter(|(_, w)| w.len() > 1) {
            let name = snapshot.type_name(object);
            let completion_source = self.is_task_completion_source_task(object);
            let mut description = if completion_source {
                "TaskCompletionSource task".into()
            } else {
                without_namespaces(&name)
            };
            if name == "System.Threading.SemaphoreSlim" {
                description += &format!(
                    " (count {})",
                    snapshot.integer(object, "m_currentCount").unwrap_or(0)
                );
            }
            let mut calls = BTreeMap::new();
            for &waiter in waiters {
                let name = if self.sync_waiters.contains(&waiter) {
                    self.thread_label(waiter)?
                } else {
                    self.label(waiter)?
                };
                *calls.entry(name).or_default() += 1;
            }
            let mut holding = if completion_source {
                self.holder(object, waiters, false)?
            } else {
                self.resource_holder(object, waiters)?
            };
            if let Some((holder, _)) = &holding
                && completion_source
                && snapshot.type_name(*holder).contains("TaskCompletionSource")
            {
                let mut excluded = waiters.clone();
                excluded.insert(object);
                holding = self.holder(*holder, &excluded, false)?.or(holding);
            }
            let held = if let Some(holder) = holding {
                self.describe_holder(&holder)?
            } else {
                self.root_description(object)?
            };
            section.rows.push(CheckRow::new(
                vec![
                    Cell::int(waiters.len()),
                    Cell::text(description),
                    Cell::text(held),
                    Cell::text(counted_names(&calls, 3)),
                ],
                std::iter::once(object)
                    .chain(waiters.iter().copied().take(19))
                    .collect(),
            ));
        }
        section.rows.sort_by(|a, b| {
            let count = |r: &CheckRow| match r.cells[0] {
                Cell::Int(n) => n,
                _ => 0,
            };
            count(b)
                .cmp(&count(a))
                .then_with(|| a.samples.first().cmp(&b.samples.first()))
        });
        Ok(section)
    }

    fn methods_report(&self) -> Result<(CheckSection, usize)> {
        #[derive(Default)]
        struct Group {
            live: usize,
            dead: usize,
            awaiting: BTreeMap<String, usize>,
            samples: Vec<usize>,
        }
        let snapshot = self.snapshot;
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();
        for (&object, method) in &self.methods {
            let group = groups
                .entry(method.name.clone() + &method.position())
                .or_default();
            if snapshot.graph.live[object] != 0 {
                group.live += 1;
            } else {
                group.dead += 1;
            }
            let kind = if let Some(awaited) = method.awaited {
                if self.methods.contains_key(&awaited)
                    || self.task_types.contains(&snapshot.heap.type_ids[awaited])
                {
                    self.label(awaited)?
                } else {
                    without_namespaces(&snapshot.type_name(awaited))
                }
            } else {
                "nothing (not awaiting)".into()
            };
            *group.awaiting.entry(kind).or_default() += 1;
            if group.samples.len() < 20 {
                group.samples.push(object);
            }
        }
        let dead = groups.values().map(|g| g.dead).sum();
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|(a, ac), (b, bc)| {
            (bc.live + bc.dead)
                .cmp(&(ac.live + ac.dead))
                .then_with(|| a.cmp(b))
        });
        let mut section = CheckSection::new(
            "By method and await",
            "Every waiting async method, by where it stopped.",
            &["live", "dead", "method and await", "awaiting"],
        );
        section.rows = groups
            .into_iter()
            .map(|(name, group)| {
                CheckRow::new(
                    vec![
                        Cell::int(group.live),
                        Cell::int(group.dead),
                        Cell::text(name),
                        Cell::text(counted_names(&group.awaiting, 2)),
                    ],
                    group.samples,
                )
            })
            .collect();
        Ok((section, dead))
    }

    fn wait_findings(&self, report: &mut CheckReport, cycle_calls: i64, dead: usize) {
        let mut waits: Vec<_> = self.waits.iter().collect();
        waits.sort_by(|(a, ac), (b, bc)| bc.0.cmp(&ac.0).then_with(|| a.cmp(b)));
        for ((root, inner, what), &(count, never)) in waits {
            if count < if never { 10 } else { 100 } {
                continue;
            }
            let location = if root == inner {
                " wait on ".into()
            } else {
                format!(" wait in {inner} on ")
            };
            let operation = if never {
                format!(
                    "{} that only these calls hold: they never finish",
                    what.split(", held only by").next().unwrap_or(what)
                )
            } else {
                what.clone()
            };
            report.findings.push(Finding {
                bytes: 0,
                text: format!("{count} async calls of {root}{location}{operation}"),
            });
        }
        if cycle_calls > 0 {
            report.findings.push(Finding {
                bytes: 0,
                text: format!(
                    "{cycle_calls} async calls wait on each other in a cycle: they never finish"
                ),
            });
        }
        if dead >= 100 {
            report.findings.push(Finding {
                bytes: 0,
                text: format!(
                    "{dead} async methods were dropped while waiting (dead, never resumed)"
                ),
            });
        }
    }

    pub fn report(&self, model: &TaskGraphs) -> Result<CheckReport> {
        let mut report = CheckReport::new(
            "async",
            "Waiting async methods",
            "Unfinished async calls, the calls they await and the innermost awaited operations. \
                If only waiting calls retain an operation, no other detected owner can complete it. \
                Cycles show calls waiting on each other. The dump has no wait durations; compare \
                repeated waits and their owners.",
        );
        let mut tree = CheckSection::new(
            "Tasks by async stack",
            "Each top row groups tasks with the same async stack, from the outermost owner to \
                the innermost wait. Its count is the number of tasks; \"on N objects\" counts \
                distinct awaited objects. Child rows show each awaited object or task and its \
                calls. Identical rows are merged, with counts of the objects they represent.",
            &["count", "call"],
        );
        let mut cycles = CheckSection::new(
            "Waiting on each other",
            "Async calls whose awaits form a cycle: each waits, through the others, on itself. They never finish.",
            &["count", "call"],
        );
        let mut dropped = CheckSection::new(
            "Dropped while waiting",
            "Waiting calls that nothing references any more (dead objects): they will never \
                resume, and their finally blocks never run.",
            &["count", "call"],
        );
        for group in &model.groups {
            let suffix = if group.graphs.len() > 1 {
                format!(" (on {} objects)", group.graphs.len())
            } else {
                String::new()
            };
            let mut entry = CheckRow::new(
                vec![
                    Cell::int(group.outermost_calls()),
                    Cell::text(group.summary() + &suffix),
                ],
                group
                    .graphs
                    .iter()
                    .filter_map(|g| g.roots.first()?.objects.first().copied())
                    .take(50)
                    .collect(),
            );
            entry.children = if group.graphs.len() == 1 {
                group.graphs[0].roots.iter().map(row).collect()
            } else {
                group
                    .graphs
                    .iter()
                    .map(|graph| {
                        if graph.roots.len() == 1 {
                            return row(&graph.roots[0]);
                        }
                        let mut text = graph
                            .roots
                            .iter()
                            .take(2)
                            .map(|n| n.text.clone())
                            .collect::<Vec<_>>()
                            .join(" + ");
                        if graph.roots.len() > 2 {
                            text += &format!(" + {} more", graph.roots.len() - 2);
                        }
                        CheckRow {
                            cells: vec![Cell::int(graph.calls), Cell::text(text)],
                            samples: graph
                                .roots
                                .iter()
                                .flat_map(|n| &n.objects)
                                .copied()
                                .take(50)
                                .collect(),
                            children: graph.roots.iter().map(row).collect(),
                        }
                    })
                    .collect()
            };
            match group.state {
                TaskState::Cycle => cycles.rows.push(entry),
                TaskState::Dropped => dropped.rows.push(entry),
                _ => tree.rows.push(entry),
            }
        }
        let shared = self.shared_waits()?;
        let (methods, dead) = self.methods_report()?;
        let cycle_calls: i64 = cycles
            .rows
            .iter()
            .filter_map(|r| match r.cells.first() {
                Some(Cell::Int(count)) => Some(*count),
                _ => None,
            })
            .sum();
        report.sections = vec![tree, shared, cycles, dropped, methods];
        self.wait_findings(&mut report, cycle_calls, dead);
        Ok(report)
    }
}
