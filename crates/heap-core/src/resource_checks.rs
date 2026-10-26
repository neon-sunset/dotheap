use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding, counted_names},
    timers::timer_key,
    type_names::short,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

impl HeapSnapshot {
    pub fn blocked_threads_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "blocked",
                "Blocked threads and the thread pool",
                "Synchronous waits grouped by wait and first application frame, plus work queued \
                    in the thread pool. Workers blocked on tasks can prevent the queued work that \
                    completes those tasks from running. Frames are estimated from return addresses.",
            );
            let stacks = self.thread_stacks()?;
            let states = self.thread_states()?;
            let workers = states.iter().filter(|t| t.pool).count();
            let idle = states.iter().filter(|t| t.idle).count();
            let blocked = states.iter().filter(|t| t.pool && t.wait.is_some()).count();
            let mut groups: BTreeMap<String, (Vec<String>, usize)> = BTreeMap::new();
            for thread in states.iter() {
                let Some(wait) = &thread.wait else { continue };
                let key = format!("{wait} | {}", thread.wait_frame.as_deref().unwrap_or("-"));
                let (threads, pool) = groups.entry(key).or_default();
                threads.push(stacks.label(thread.tid));
                *pool += usize::from(thread.pool);
            }
            let queued = self.queued_work_items()?;
            let mut pool = CheckSection::new(
                "Thread pool",
                "Worker threads by what they do, and the work items that wait in the queues for \
                    a thread. Idle workers wait on the pool's semaphore for work.",
                &[
                    "workers",
                    "busy",
                    "blocked",
                    "idle",
                    "queued",
                    "in worker queues",
                ],
            );
            pool.rows.push(CheckRow::new(
                vec![
                    Cell::int(workers),
                    Cell::int(workers.saturating_sub(blocked + idle)),
                    Cell::int(blocked),
                    Cell::int(idle),
                    Cell::int(queued.len()),
                    Cell::int(queued.iter().filter(|w| w.local).count()),
                ],
                Vec::new(),
            ));
            let mut items = CheckSection::new(
                "Queued work items",
                "Work items in the thread pool's queues, by what they run.",
                &["items", "in worker queues", "work item"],
            );
            let mut work: BTreeMap<&str, Vec<_>> = BTreeMap::new();
            for item in queued.iter() {
                work.entry(&item.what).or_default().push(item);
            }
            let mut work: Vec<_> = work.into_iter().collect();
            work.sort_by(|(a, av), (b, bv)| bv.len().cmp(&av.len()).then_with(|| a.cmp(b)));
            for (what, list) in work {
                items.rows.push(CheckRow::new(
                    vec![
                        Cell::int(list.len()),
                        Cell::int(list.iter().filter(|w| w.local).count()),
                        Cell::text(what),
                    ],
                    list.iter().take(50).map(|w| w.object).collect(),
                ));
            }
            let mut section = CheckSection::new(
                "By wait",
                "",
                &[
                    "threads",
                    "thread pool",
                    "wait | first application frame",
                    "threads",
                ],
            );
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|(a, av), (b, bv)| bv.0.len().cmp(&av.0.len()).then_with(|| a.cmp(b)));
            for (key, (threads, pool)) in groups {
                section.rows.push(CheckRow::new(
                    vec![
                        Cell::int(threads.len()),
                        Cell::int(pool),
                        Cell::text(key),
                        Cell::text(threads.into_iter().take(5).collect::<Vec<_>>().join(", ")),
                    ],
                    Vec::new(),
                ));
            }
            report.sections = vec![pool, items, section];
            if !queued.is_empty() && idle == 0 && blocked > 0 {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!(
                        "{} work items wait for a thread pool thread while none of the {workers} \
                        workers is idle and {blocked} of them are blocked in synchronous waits \
                        (thread pool starvation)",
                        queued.len()
                    ),
                });
            } else if blocked >= 4 {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("{blocked} thread pool threads are blocked in synchronous waits"),
                });
            }
            Ok(report)
        };
        self.saved("check blocked v2", compute)
    }

    pub fn cancellation_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "cancellation",
                "Cancellation registrations",
                "Callbacks registered on each live CancellationTokenSource, by callback method. \
                    Each registration retains its callback and state. Large counts on a long-lived \
                    source can indicate undisposed registrations or linked token sources.",
            );
            let mut section = CheckSection::new(
                "Sources with registrations",
                "",
                &["registrations", "source", "type", "held by", "callbacks"],
            );
            let mut linked = 0;
            for source in self
                .objects(
                    &self.type_ids(|s| s.starts_with("System.Threading.CancellationTokenSource")),
                )
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
            {
                if self.type_name(source).contains("Linked") {
                    linked += 1;
                }
                let mut node = self.target(self.target(source, "_registrations"), "Callbacks");
                let mut seen = HashSet::new();
                let mut methods = BTreeMap::new();
                let mut samples = vec![source];
                while let Some(current) = node.filter(|o| seen.insert(*o)) {
                    if let Some(delegate) = self.target(current, "Callback") {
                        let name =
                            self.delegate_method(delegate)?.unwrap_or_else(|| {
                                short(&self.type_name(
                                    self.target(delegate, "_target").unwrap_or(delegate),
                                ))
                            });
                        *methods.entry(name).or_default() += 1;
                    }
                    if samples.len() < 20 {
                        samples.push(current);
                    }
                    node = self.target(current, "Next");
                }
                if seen.len() >= 10 {
                    section.rows.push(CheckRow::new(
                        vec![
                            Cell::int(seen.len()),
                            Cell::Address(self.heap.addresses[source]),
                            Cell::text(self.type_name(source)),
                            Cell::text(self.holder(source)?),
                            Cell::text(counted_names(&methods, 3)),
                        ],
                        samples,
                    ));
                }
            }
            section
                .rows
                .sort_by_key(|r| std::cmp::Reverse(first_count(r)));
            if let Some(first) = section.rows.first().filter(|r| first_count(r) >= 1000) {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!(
                        "a CancellationTokenSource has {} registered callbacks \
                        ({linked} linked token sources are alive)",
                        first_count(first)
                    ),
                });
            }
            report.sections.push(section);
            Ok(report)
        };
        self.saved("check cancellation", compute)
    }

    pub fn event_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            #[derive(Default)]
            struct Group {
                delegates: usize,
                entries: usize,
                largest: usize,
                targets: BTreeMap<String, usize>,
                samples: Vec<usize>,
            }
            let mut report = CheckReport::new(
                "events",
                "Event subscriptions",
                "Events with many subscribers, grouped by owner and field. A long-lived \
                    publisher keeps its subscribers alive until they unsubscribe.",
            );
            let mut groups: BTreeMap<String, Group> = BTreeMap::new();
            for delegate in self
                .objects(&self.types_with_field("_invocationCount"))
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
            {
                let Some(list) = self
                    .target(delegate, "_invocationList")
                    .filter(|&o| self.type_name(o) == "System.Object[]")
                else {
                    continue;
                };
                let Some(count) = self
                    .integer(delegate, "_invocationCount")
                    .filter(|&n| n >= 16)
                    .map(|n| n as usize)
                else {
                    continue;
                };
                let group = groups.entry(self.holder(delegate)?).or_default();
                group.delegates += 1;
                group.entries += count;
                group.largest = group.largest.max(count);
                for item in self
                    .elements(Some(list))
                    .into_iter()
                    .take(count.min(2000))
                    .flatten()
                {
                    let name = self
                        .target(item, "_target")
                        .map(|o| short(&self.type_name(o)))
                        .unwrap_or("static method".into());
                    *group.targets.entry(name).or_default() += 1;
                }
                if group.samples.len() < 20 {
                    group.samples.push(delegate);
                }
            }
            if let Some((owner, group)) = groups
                .iter()
                .max_by_key(|(_, g)| g.largest)
                .filter(|(_, g)| g.largest >= 1000)
            {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("an event held by {owner} has {} subscribers", group.largest),
                });
            }
            let mut section = CheckSection::new(
                "Delegates with 16 or more subscribers",
                "",
                &[
                    "subscribers",
                    "largest list",
                    "delegates",
                    "held by",
                    "subscriber types",
                ],
            );
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|(a, av), (b, bv)| bv.entries.cmp(&av.entries).then_with(|| a.cmp(b)));
            section.rows = groups
                .into_iter()
                .map(|(name, g)| {
                    CheckRow::new(
                        vec![
                            Cell::int(g.entries),
                            Cell::int(g.largest),
                            Cell::int(g.delegates),
                            Cell::text(name),
                            Cell::text(counted_names(&g.targets, 3)),
                        ],
                        g.samples,
                    )
                })
                .collect();
            report.sections.push(section);
            Ok(report)
        };
        self.saved("check events", compute)
    }

    pub fn timer_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "timers",
                "Timers",
                "Live timers by source (Timer, PeriodicTimer, Task.Delay, Task.WaitAsync, \
                    CancelAfter), callback method or awaiting async call, and owner. Undisposed \
                    timers can retain their callback target and state.",
            );
            let timers = self.timers()?;
            let mut groups: BTreeMap<String, Vec<_>> = BTreeMap::new();
            for timer in &timers.timers {
                groups.entry(timer_key(timer)).or_default().push(timer);
            }
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|(a, av), (b, bv)| bv.len().cmp(&av.len()).then_with(|| a.cmp(b)));
            let mut section = CheckSection::new(
                "By what made them",
                "",
                &["timers", "kind", "calls", "awaited by", "held by", "period"],
            );
            for (key, timers) in groups {
                section.rows.push(CheckRow::new(
                    std::iter::once(Cell::int(timers.len()))
                        .chain(key.split('\t').map(Cell::text))
                        .collect(),
                    timers
                        .iter()
                        .take(20)
                        .map(|t| t.owner.or(t.source).unwrap_or(t.object))
                        .collect(),
                ));
            }
            if let Some(first) = section.rows.first().filter(|r| first_count(r) >= 1000)
                && let (Cell::Text(kind), Cell::Text(owner)) = (&first.cells[1], &first.cells[4])
            {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("{} live {kind} timers held by {owner}", first_count(first)),
                });
            }
            report.sections.push(section);
            Ok(report)
        };
        self.saved("check timers", compute)
    }
}

fn first_count(row: &CheckRow) -> i64 {
    match row.cells.first() {
        Some(Cell::Int(count)) => *count,
        _ => 0,
    }
}
