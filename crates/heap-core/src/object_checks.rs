use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding, format_size},
    code_map::CodeMap,
    heap::{GRANULE, Gc},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Default)]
struct ObjectCount {
    count: usize,
    bytes: u64,
    samples: Vec<usize>,
}

impl ObjectCount {
    fn add(&mut self, object: usize, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
        if self.samples.len() < 50 {
            self.samples.push(object);
        }
    }
}

impl HeapSnapshot {
    pub fn instance_count_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "instance-counts",
                "Instance counts",
                "Live counts of types commonly shared or disposed, such as HttpClient, Regex and \
                    JsonSerializerOptions, grouped by owner. Counts at or above each type's \
                    threshold produce findings. Inspect lifetime and reuse; the threshold alone \
                    does not establish a leak.",
            );
            let mut section = CheckSection::new(
                "By type",
                "Open a type to see its live instances grouped by what holds them.",
                &["live", "threshold", "type"],
            );
            for &(name, limit, why) in COUNTED_TYPES {
                let Some(stats) = self
                    .type_stats
                    .iter()
                    .find(|t| t.name == name && t.live_count > 0)
                else {
                    continue;
                };
                let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for object in (0..self.heap.count())
                    .filter(|&o| self.heap.type_ids[o] == stats.type_id && self.graph.live[o] != 0)
                {
                    groups
                        .entry(self.owning_field(object)?)
                        .or_default()
                        .push(object);
                }
                let mut groups: Vec<_> = groups.into_iter().collect();
                groups.sort_by(|(a, av), (b, bv)| bv.len().cmp(&av.len()).then_with(|| a.cmp(b)));
                let samples = groups
                    .iter()
                    .flat_map(|(_, objects)| objects)
                    .copied()
                    .take(50)
                    .collect();
                let children = groups
                    .into_iter()
                    .map(|(holder, objects)| {
                        CheckRow::new(
                            vec![
                                Cell::int(objects.len()),
                                Cell::None,
                                Cell::text(format!("held by {holder}")),
                            ],
                            objects.into_iter().take(50).collect(),
                        )
                    })
                    .collect();
                section.rows.push(CheckRow {
                    cells: vec![
                        Cell::int(stats.live_count),
                        Cell::int(limit),
                        Cell::text(name),
                    ],
                    samples,
                    children,
                });
                if stats.live_count >= limit {
                    report.findings.push(Finding {
                        bytes: 0,
                        text: format!("{} live {name} instances: {why}", stats.live_count),
                    });
                }
            }
            report.sections.push(section);
            Ok(report)
        };
        self.saved("check instance counts", compute)
    }

    pub fn thrower(&self, exception: usize, code: &CodeMap) -> Option<String> {
        let mut trace = self.target(exception, "_stackTrace")?;
        if self.heap.method_table(trace).contains_pointers() {
            trace = self.reference_elements(trace, 1)?.first()?.target?;
        }
        let data = self.heap.addresses[trace] + 16;
        let count = self.dump.u32(data)? as u64;
        if count == 0 || count * 32 + 16 > self.component_count(trace)? as u64 {
            return None;
        }
        let ip = self.dump.u64(data + 16)?;
        Some(
            code.locate(ip)
                .map(|l| l.display_name())
                .unwrap_or_else(|| format!("0x{ip:x}")),
        )
    }

    pub fn exceptions_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            #[derive(Default)]
            struct Exceptions {
                messages: BTreeSet<String>,
                message: String,
                live: usize,
                dead: usize,
                samples: Vec<usize>,
            }
            #[derive(Default)]
            struct FaultedTasks {
                live: usize,
                dead: usize,
                unobserved: usize,
                samples: Vec<usize>,
            }
            let mut report = CheckReport::new(
                "exceptions",
                "Exceptions",
                "Live and dead exceptions by type and the first frame of their saved throw \
                    stack, plus faulted tasks by exception. Unthrown or preallocated exceptions can \
                    have no saved stack. Many dead exceptions at one site can indicate frequent \
                    throwing; live exceptions still have an owner. Unobserved task failures have \
                    not been awaited or read; finalization raises \
                    TaskScheduler.UnobservedTaskException.",
            );
            let code = self.code_map()?;
            let mut all: BTreeMap<(String, String), Exceptions> = BTreeMap::new();
            for exception in self.objects(&self.types_with_field("_remoteStackTraceString")) {
                let message = self
                    .target(exception, "_message")
                    .and_then(|o| self.string_value(o, 120))
                    .unwrap_or_default()
                    .replace('\n', " ");
                let thrower = self
                    .thrower(exception, &code)
                    .unwrap_or("never thrown (no stack trace)".into());
                let group = all
                    .entry((self.type_name(exception), thrower))
                    .or_insert_with(|| Exceptions {
                        message: message.clone(),
                        ..Exceptions::default()
                    });
                if self.graph.live[exception] != 0 {
                    group.live += 1;
                } else {
                    group.dead += 1;
                }
                if group.messages.len() < 100 {
                    group.messages.insert(message);
                }
                if group.samples.len() < 50 {
                    group.samples.push(exception);
                }
            }
            let mut exceptions = CheckSection::new(
                "By type and throwing method",
                "Messages: how many different messages the group has, and one of them.",
                &["live", "dead", "type", "thrown in", "messages", "message"],
            );
            let mut all: Vec<_> = all.into_iter().collect();
            all.sort_by(|(a, av), (b, bv)| {
                (bv.live + bv.dead)
                    .cmp(&(av.live + av.dead))
                    .then_with(|| a.cmp(b))
            });
            for ((name, thrower), group) in all {
                if group.live >= 1000 {
                    report.findings.push(Finding {
                        bytes: 0,
                        text: format!(
                            "{} live {name} exceptions thrown in {thrower} are kept alive",
                            group.live
                        ),
                    });
                }
                if group.dead >= 1000 {
                    report.findings.push(Finding {
                        bytes: 0,
                        text: format!(
                            "{} dead {name} exceptions thrown in {thrower} wait for collection: \
                            the code throws there often",
                            group.dead
                        ),
                    });
                }
                exceptions.rows.push(CheckRow::new(
                    vec![
                        Cell::int(group.live),
                        Cell::int(group.dead),
                        Cell::text(name),
                        Cell::text(thrower),
                        Cell::int(group.messages.len()),
                        Cell::text(group.message),
                    ],
                    group.samples,
                ));
            }
            let mut tasks: BTreeMap<String, FaultedTasks> = BTreeMap::new();
            for task in self.objects(&self.types_with_field("m_stateFlags")) {
                if self.integer(task, "m_stateFlags").unwrap_or(0) & 0x200000 == 0 {
                    continue;
                }
                let holder = self.target(
                    self.target(task, "m_contingentProperties"),
                    "m_exceptionsHolder",
                );
                let handled = self.integer(holder, "m_isHandled").unwrap_or(0) != 0;
                let list = self.target(self.target(holder, "m_faultExceptions"), "_items");
                let first = list
                    .and_then(|o| self.reference_elements(o, 1))
                    .and_then(|e| e.first().and_then(|e| e.target));
                let description = if let Some(exception) = self.target(first, "_exception") {
                    let message = self
                        .target(exception, "_message")
                        .and_then(|o| self.string_value(o, 120))
                        .unwrap_or_default()
                        .replace('\n', " ");
                    let suffix = if message.is_empty() {
                        message
                    } else {
                        format!(": {message}")
                    };
                    self.type_name(exception) + &suffix
                } else {
                    "?".into()
                };
                let group = tasks.entry(description).or_default();
                if self.graph.live[task] != 0 {
                    group.live += 1;
                } else {
                    group.dead += 1;
                }
                group.unobserved += usize::from(!handled);
                if group.samples.len() < 20 {
                    group.samples.push(task);
                }
            }
            let unobserved: usize = tasks.values().map(|g| g.unobserved).sum();
            let mut tasks: Vec<_> = tasks.into_iter().collect();
            tasks.sort_by(|(a, av), (b, bv)| {
                (bv.live + bv.dead)
                    .cmp(&(av.live + av.dead))
                    .then_with(|| a.cmp(b))
            });
            let mut faulted = CheckSection::new(
                "Faulted tasks",
                "Tasks that ended with an exception, by exception type and message.",
                &["live", "dead", "unobserved", "exception"],
            );
            faulted.rows = tasks
                .into_iter()
                .map(|(name, group)| {
                    CheckRow::new(
                        vec![
                            Cell::int(group.live),
                            Cell::int(group.dead),
                            Cell::int(group.unobserved),
                            Cell::text(name),
                        ],
                        group.samples,
                    )
                })
                .collect();
            report.sections = vec![exceptions, faulted];
            if unobserved >= 10 {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("{unobserved} faulted tasks have exceptions nobody observed"),
                });
            }
            Ok(report)
        };
        self.saved("check exceptions", compute)
    }

    pub fn large_object_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            #[derive(Default)]
            struct Huge {
                objects: ObjectCount,
                dead: usize,
                region: u64,
                past: u64,
            }
            let stock = self.heap.kind.gc == Gc::CoreClr;
            let threshold = if stock { 85_000 } else { 32 * 1024 };
            let mut report = CheckReport::new(
                "large-objects",
                "Large objects",
                if stock {
                    "Objects of 85,000 bytes and more (GCLOHThreshold can raise this) live in \
                        the LOH. Only gen2 collections free them. The LOH is swept and is compacted \
                        only in special cases, so short-lived large arrays leave dead objects until \
                        the next gen2 collection and free gaps after it."
                } else {
                    "Objects of 32 KB and more, by type. Objects that do not fit in a 2 MB \
                        region have a region of their own."
                },
            );
            let own: BTreeSet<_> = self
                .heap
                .regions
                .iter()
                .enumerate()
                .filter(|(_, r)| !stock && r.size() > GRANULE && r.end_object - r.first_object == 1)
                .map(|(index, _)| index)
                .collect();
            let mut huge: BTreeMap<String, Huge> = BTreeMap::new();
            for &index in &own {
                let region = &self.heap.regions[index];
                let object = region.first_object;
                let size = self.heap.size(object);
                let group = huge.entry(self.type_name(object)).or_default();
                group.objects.add(object, size);
                group.region += region.size();
                group.past += region
                    .committed_bytes()
                    .saturating_sub(region.header_bytes())
                    .saturating_sub(size);
                group.dead += usize::from(self.graph.live[object] == 0);
            }
            let mut dead: BTreeMap<String, ObjectCount> = BTreeMap::new();
            let mut live: BTreeMap<String, ObjectCount> = BTreeMap::new();
            for object in (0..self.heap.count()).filter(|&o| {
                self.heap.size(o) >= threshold && !own.contains(&self.heap.region_of(o))
            }) {
                let groups = if self.graph.live[object] != 0 {
                    &mut live
                } else {
                    &mut dead
                };
                groups
                    .entry(self.type_name(object))
                    .or_default()
                    .add(object, self.heap.size(object));
            }
            let place = if stock { "" } else { ", in shared regions" };
            for (title, explanation, groups) in [
                (
                    format!("Dead{place}"),
                    "Dead objects are not swept yet.",
                    dead,
                ),
                (format!("Live{place}"), "", live),
            ] {
                let mut section =
                    CheckSection::new(&title, explanation, &["objects", "bytes", "type"]);
                let mut groups: Vec<_> = groups.into_iter().collect();
                groups.sort_by(|(a, av), (b, bv)| bv.bytes.cmp(&av.bytes).then_with(|| a.cmp(b)));
                section.rows = groups
                    .into_iter()
                    .map(|(name, g)| {
                        CheckRow::new(
                            vec![Cell::int(g.count), Cell::Bytes(g.bytes), Cell::text(name)],
                            g.samples,
                        )
                    })
                    .collect();
                report.sections.push(section);
            }
            if stock {
                let regions: Vec<_> = self
                    .heap
                    .regions
                    .iter()
                    .filter(|r| r.generation_name == "LOH")
                    .collect();
                let dead: u64 = regions.iter().map(|r| r.dead_bytes()).sum();
                let free: u64 = regions.iter().map(|r| r.free_bytes).sum();
                let live = regions.iter().map(|r| r.live_bytes).sum();
                let mut totals = CheckSection::new(
                    "LOH",
                    "Dead LOH objects wait for gen2 collection. The LOH free list reuses gaps \
                        for new large objects. Compaction can remove gaps; \
                        GCSettings.LargeObjectHeapCompactionMode requests it.",
                    &["regions", "live", "dead", "free"],
                );
                totals.rows.push(CheckRow::new(
                    vec![
                        Cell::int(regions.len()),
                        Cell::Bytes(live),
                        Cell::Bytes(dead),
                        Cell::Bytes(free),
                    ],
                    Vec::new(),
                ));
                report.sections.insert(0, totals);
                if dead + free > 64 << 20 {
                    report.findings.push(Finding {
                        bytes: dead + free,
                        text: format!(
                            "{} of dead objects in the LOH wait for a gen2 collection; \
                            {} of free gaps wait for new large objects",
                            format_size(dead),
                            format_size(free)
                        ),
                    });
                }
            } else {
                let mut section = CheckSection::new(
                    "In regions of their own",
                    "Committed past the object is memory at the end of the region after the object.",
                    &[
                        "objects",
                        "dead",
                        "object bytes",
                        "region bytes",
                        "committed past the object",
                        "type",
                    ],
                );
                let mut huge: Vec<_> = huge.into_iter().collect();
                huge.sort_by(|(a, av), (b, bv)| {
                    bv.objects
                        .bytes
                        .cmp(&av.objects.bytes)
                        .then_with(|| a.cmp(b))
                });
                section.rows = huge
                    .into_iter()
                    .map(|(name, group)| {
                        CheckRow::new(
                            vec![
                                Cell::int(group.objects.count),
                                Cell::int(group.dead),
                                Cell::Bytes(group.objects.bytes),
                                Cell::Bytes(group.region),
                                Cell::Bytes(group.past),
                                Cell::text(name),
                            ],
                            group.objects.samples,
                        )
                    })
                    .collect();
                report.sections.push(section);
            }
            Ok(report)
        };
        self.saved("check large", compute)
    }
}

const COUNTED_TYPES: &[(&str, usize, &str)] = &[
    (
        "System.Net.Http.HttpClient",
        20,
        "each HttpClient with its own handler keeps its own connection pool; create few and \
            reuse them (IHttpClientFactory)",
    ),
    (
        "System.Net.Http.SocketsHttpHandler",
        20,
        "each handler has its own connection pool and sockets",
    ),
    (
        "System.Text.Json.JsonSerializerOptions",
        20,
        "each options instance builds its own metadata cache; create one and reuse it",
    ),
    (
        "System.Text.RegularExpressions.Regex",
        200,
        "each Regex instance compiles and keeps its own matcher state; cache instances or use static Regex methods",
    ),
    (
        "Microsoft.Extensions.DependencyInjection.ServiceProvider",
        5,
        "service providers are meant to be built once; each keeps its own singletons",
    ),
    (
        "Microsoft.Extensions.Caching.Memory.MemoryCache",
        20,
        "each MemoryCache keeps its own entries and timer",
    ),
    ("System.Net.Sockets.Socket", 5000, "many open sockets"),
    (
        "System.Diagnostics.Activity",
        10000,
        "many live activities: spans that are started and never stopped",
    ),
    ("System.Threading.Thread", 500, "many managed threads"),
];
