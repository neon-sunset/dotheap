use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding, format_size},
    native_graph::Reach,
    native_memory::STATE_ATTACHED_THREADS,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Default)]
struct RetainedGroup {
    chunks: usize,
    bytes: u64,
    samples: Vec<usize>,
}

#[derive(Default)]
struct ArenaGroup {
    arenas: HashSet<u64>,
    heaps: usize,
    in_use: u64,
    top: u64,
    free: u64,
}

impl HeapSnapshot {
    pub fn finalizer_backlog_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "finalizer-backlog",
                "Native memory waiting for finalizers",
                "Native memory reachable only through managed objects awaiting finalization. \
                    Their finalizers may release it; check whether earlier disposal was possible.",
            );
            let graph = self.native_graph()?;
            let mut groups: BTreeMap<String, RetainedGroup> = BTreeMap::new();
            for (index, chunk) in graph.chunks.iter().enumerate() {
                if !graph.is_in_use(index) || graph.reach[index] != Reach::PendingManaged {
                    continue;
                }
                let owner = graph.nearest_owner(index);
                let name = owner.map(|o| self.type_name(o)).unwrap_or("?".into());
                let group = groups.entry(name).or_default();
                group.chunks += 1;
                group.bytes += chunk.size;
                if group.samples.len() < 20
                    && let Some(owner) = owner
                {
                    group.samples.push(owner);
                }
            }
            let total: u64 = groups.values().map(|g| g.bytes).sum();
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|(a, av), (b, bv)| bv.bytes.cmp(&av.bytes).then_with(|| a.cmp(b)));
            let mut section = CheckSection::new(
                "By type of the object pending finalization",
                "Retained: the bytes of the malloc chunks that the objects of the type retain.",
                &["chunks", "retained", "type"],
            );
            section.rows = groups
                .into_iter()
                .map(|(name, g)| {
                    CheckRow::new(
                        vec![Cell::int(g.chunks), Cell::Bytes(g.bytes), Cell::text(name)],
                        g.samples,
                    )
                })
                .collect();
            report.sections.push(section);
            if total >= 1 << 20 {
                report.findings.push(Finding {
                    bytes: total,
                    text: format!(
                        "{} of native memory is held only by managed objects pending finalization \
                        and may be released by their finalizers; inspect disposal and finalizer \
                        behavior",
                        format_size(total)
                    ),
                });
            }
            Ok(report)
        };
        self.saved("check finalizer backlog", compute)
    }

    pub fn native_held_by_dead_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "native-held-by-dead",
                "Native memory of dead objects",
                "Malloc chunks referenced only by dead managed objects, with no pending \
                    finalizer for those owners. The object may have died without freeing the \
                    memory, or malloc may have reused a freed allocation while the dead field \
                    retained its old pointer.",
            );
            let graph = self.native_graph()?;
            let mut groups: BTreeMap<(String, String), RetainedGroup> = BTreeMap::new();
            for (index, chunk) in graph.chunks.iter().enumerate() {
                if !graph.is_in_use(index) || graph.reach[index] != Reach::DeadManaged {
                    continue;
                }
                let Some(owner) = graph.nearest_owner(index) else {
                    continue;
                };
                let field = graph.owner_field(owner, index, self).unwrap_or_default();
                let group = groups.entry((self.type_name(owner), field)).or_default();
                group.chunks += 1;
                group.bytes += chunk.size;
                if group.samples.len() < 20 {
                    group.samples.push(owner);
                }
            }
            let total: u64 = groups.values().map(|g| g.bytes).sum();
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|(a, av), (b, bv)| bv.bytes.cmp(&av.bytes).then_with(|| a.cmp(b)));
            let mut section = CheckSection::new(
                "By type and field of the dead object",
                "Bytes: the malloc chunks that the dead objects of the type retain through the field.",
                &["chunks", "bytes", "type", "field"],
            );
            section.rows = groups
                .into_iter()
                .map(|((name, field), g)| {
                    CheckRow::new(
                        vec![
                            Cell::int(g.chunks),
                            Cell::Bytes(g.bytes),
                            Cell::text(name),
                            Cell::text(field),
                        ],
                        g.samples,
                    )
                })
                .collect();
            report.sections.push(section);
            if total >= 1 << 20 {
                report.findings.push(Finding {
                    bytes: total,
                    text: format!("{} of native memory is held only by dead managed objects, and no finalizer will free it", format_size(total)),
                });
            }
            Ok(report)
        };
        self.saved("check native held by dead", compute)
    }

    pub fn malloc_arena_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "malloc-arenas",
                "Freed memory in malloc arenas",
                "glibc retains freed memory in arena free chunks and at the end of each heap \
                    (top chunks). On 64-bit systems it defaults to at most eight arenas per CPU \
                    core. Arenas can keep memory after their threads exit. A non-main heap can \
                    shrink when a free of at least 64 KB triggers trimming and its top chunk \
                    reaches the trim threshold. malloc_trim(0) releases whole pages inside free \
                    chunks in every arena, but trims only the main arena's top chunk. Sizes count \
                    pages with data in the dump, including swapped pages.",
            );
            let heaps = &self.native.malloc_heaps;
            if !self.dump.platform.is_linux() || heaps.is_empty() {
                return Ok(report);
            }
            let threads = |arena: u64| {
                if arena == 0 {
                    return None;
                }
                self.dump.u64(arena.checked_add(STATE_ATTACHED_THREADS)?)
            };
            let mut groups: BTreeMap<&str, ArenaGroup> = BTreeMap::new();
            for heap in heaps {
                let key = if heap.main_arena {
                    "main arena"
                } else {
                    match threads(heap.arena) {
                        Some(0) => "threads exited",
                        Some(1) | None => "1 thread",
                        _ => "2 or more threads",
                    }
                };
                let group = groups.entry(key).or_default();
                group.arenas.insert(heap.arena);
                group.heaps += 1;
                group.in_use += heap.in_use_bytes;
                group.top += heap.top_resident;
                group.free += heap.free_resident;
            }
            let arenas: usize = groups.values().map(|g| g.arenas.len()).sum();
            let concurrent = arenas.saturating_sub(1);
            let current_threads = self.dump.threads.len();
            let mut by_threads = CheckSection::new(
                "Arenas by the threads that use them",
                &format!(
                    "Thread counts come from malloc_state.attached_threads. glibc reuses \
                        detached arenas before creating more. The {arenas} arenas indicate at least \
                        {concurrent} threads used malloc concurrently when the last was created; \
                        the dump contains {current_threads} threads now. Detached arenas can still \
                        retain memory."
                ),
                &[
                    "arenas",
                    "heaps",
                    "in use",
                    "top chunks",
                    "free chunks",
                    "threads",
                ],
            );
            for key in [
                "threads exited",
                "1 thread",
                "2 or more threads",
                "main arena",
            ] {
                if let Some(group) = groups.get(key) {
                    by_threads.rows.push(CheckRow::new(
                        vec![
                            Cell::int(group.arenas.len()),
                            Cell::int(group.heaps),
                            Cell::Bytes(group.in_use),
                            Cell::Bytes(group.top),
                            Cell::Bytes(group.free),
                            Cell::text(key),
                        ],
                        Vec::new(),
                    ));
                }
            }
            let parameters = self.malloc_parameters()?;
            let mut settings = CheckSection::new(
                "malloc parameters",
                "Settings read from glibc's mp_ in the dump. With dynamic thresholds, freeing a \
                    separately mapped allocation up to 32 MB can raise the mmap threshold to that \
                    size and the trim threshold to twice it.",
                &["parameter", "value"],
            );
            if let Some(p) = parameters.as_ref() {
                let values = [
                    ("trim threshold", format_size(p.trim_threshold)),
                    ("mmap threshold", format_size(p.mmap_threshold)),
                    (
                        "thresholds",
                        if p.dynamic_threshold {
                            "dynamic: no tunable or mallopt set them"
                        } else {
                            "set by a tunable or mallopt"
                        }
                        .into(),
                    ),
                    (
                        "arena limit",
                        if p.arena_max == 0 {
                            "8 per core (arena_max not set)".into()
                        } else {
                            format!("{} (arena_max)", p.arena_max)
                        },
                    ),
                    (
                        "chunks mapped on their own",
                        format!("{}, {}", p.mmapped_chunks, format_size(p.mmapped_bytes)),
                    ),
                ];
                settings.rows = values
                    .into_iter()
                    .map(|(name, value)| text_row(name, value))
                    .collect();
            }
            let mut by_heap = CheckSection::new(
                "Heaps by resident free memory",
                "Top chunk: the free end of the heap. Free chunks: freed memory inside the heap.",
                &[
                    "heap",
                    "arena threads",
                    "size",
                    "in use",
                    "top chunk",
                    "free chunks",
                ],
            );
            let mut sorted: Vec<_> = heaps.iter().collect();
            sorted.sort_by_key(|h| (std::cmp::Reverse(h.top_resident + h.free_resident), h.start));
            for heap in sorted {
                let count = if heap.main_arena {
                    "main arena".into()
                } else {
                    threads(heap.arena)
                        .map(|n| n.to_string())
                        .unwrap_or("?".into())
                };
                by_heap.rows.push(CheckRow::new(
                    vec![
                        Cell::Address(heap.start),
                        Cell::text(count),
                        Cell::Bytes(heap.end - heap.start),
                        Cell::Bytes(heap.in_use_bytes),
                        Cell::Bytes(heap.top_resident),
                        Cell::Bytes(heap.free_resident),
                    ],
                    Vec::new(),
                ));
            }
            let top: u64 = heaps.iter().map(|h| h.top_resident).sum();
            let free: u64 = heaps.iter().map(|h| h.free_resident).sum();
            let trim: u64 = heaps.iter().map(|h| h.trim_releasable).sum();
            let empty = ArenaGroup::default();
            let exited = groups.get("threads exited").unwrap_or(&empty);
            let mut actions = CheckSection::new(
                "What releases it",
                "Tunables are read when the process starts: \
                    GLIBC_TUNABLES=glibc.malloc.trim_threshold=<bytes>:glibc.malloc.arena_max=<n>.",
                &["change", "effect in this process"],
            );
            let threshold = parameters
                .as_ref()
                .as_ref()
                .map(|p| {
                    format!(
                        " (now {}{})",
                        format_size(p.trim_threshold),
                        if p.dynamic_threshold { ", dynamic" } else { "" }
                    )
                })
                .unwrap_or_default();
            actions.rows = vec![
                text_row(
                    "glibc.malloc.trim_threshold",
                    format!(
                        "a fixed trim threshold{threshold}; it also turns off the dynamic \
                            thresholds. A heap whose top chunk passes it shrinks at the next free() \
                            of 64 KB or more into its arena. Top chunks hold {} now.",
                        format_size(top)
                    ),
                ),
                text_row(
                    "glibc.malloc.arena_max (MALLOC_ARENA_MAX)",
                    format!(
                        "fewer arenas: threads share them and reuse the same freed memory, with \
                            more lock contention in malloc. The process has {arenas} arenas, {} of \
                            them of threads that exited.",
                        exited.arenas.len()
                    ),
                ),
                text_row(
                    "malloc_trim(0)",
                    format!(
                        "releases about {}: the pages inside free chunks of every arena. It does \
                            not shrink the top chunks of arenas other than the main one.",
                        format_size(trim)
                    ),
                ),
                text_row(
                    "restart",
                    format!(
                        "releases the {} in top chunks of arenas whose threads exited, which \
                            only a free() into such an arena, or a new thread that takes it, can \
                            reduce.",
                        format_size(exited.top)
                    ),
                ),
            ];
            report.sections = vec![by_threads, settings, by_heap, actions];
            if top + free >= 64 << 20 {
                report.findings.push(Finding {
                    bytes: top + free,
                    text: format!(
                        "glibc keeps {} of freed malloc memory resident in {arenas} arenas: {} at \
                        the ends of their heaps, {} in free chunks; malloc_trim(0) would release \
                        about {}",
                        format_size(top + free),
                        format_size(top),
                        format_size(free),
                        format_size(trim)
                    ),
                });
            }
            if exited.top + exited.free >= 64 << 20 {
                report.findings.push(Finding {
                    bytes: exited.top + exited.free,
                    text: format!(
                        "{} malloc arenas of threads that exited keep {} of it; at least \
                        {concurrent} threads used malloc at once, and the dump has {current_threads}",
                        exited.arenas.len(),
                        format_size(exited.top + exited.free)
                    ),
                });
            }
            Ok(report)
        };
        self.saved("check malloc arenas", compute)
    }
}

fn text_row(name: &str, value: String) -> CheckRow {
    CheckRow::new(vec![Cell::text(name), Cell::text(value)], Vec::new())
}
