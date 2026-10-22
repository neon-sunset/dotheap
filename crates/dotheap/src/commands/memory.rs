use super::*;
use heap_core::{analysis::OccupancyHistogram, fragmentation::Contributor, heap::Gc};
use std::collections::BTreeMap;

pub fn regions(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let heap = &s.heap;
    if !c.arguments.positional.is_empty() {
        let a = c.address(0)?;
        let k = heap
            .region_containing(a)
            .ok_or_else(|| Error(format!("{} is not in a region: {}", hex(a), c.where_is(a))))?;
        let r = &heap.regions[k];
        let mut out = record([
            ("start", address(r.start)),
            ("end", address(r.end)),
            ("size", bytes(r.size())),
            ("generation", text(&r.generation_name)),
            ("queue", text(&r.queue)),
            ("committed", bytes(r.committed_bytes())),
            ("used", bytes(r.used_bytes())),
            ("live", bytes(r.live_bytes)),
            ("pendingFinalization", bytes(r.pending_bytes)),
            ("dead", bytes(r.dead_bytes())),
            ("free", bytes(r.free_bytes)),
            ("unparsed", bytes(r.unparsed_bytes)),
            ("gcStorage", bytes(r.gc_storage_bytes())),
            ("committedUnused", bytes(r.committed_unused_bytes())),
            ("objects", int(r.end_object - r.first_object)),
            ("pinnedObjects", Value::Bool(r.has_pinned_objects)),
            ("walkStopped", Value::Bool(r.walk_stopped)),
        ]);
        if heap.kind.gc == Gc::Satori {
            if r.has_generation() {
                out.add("satoriOccupancy", bytes(r.occupancy));
            }
            out.add("kind", text(&s.satori_region_kinds()?[k]));
        }
        if let Some(t) = r.owner_thread {
            out.add("escapeTrackingThread", Value::Id(t as i64));
        }
        if let Some(t) = r.allocating_thread {
            out.add("allocatingThread", Value::Id(t as i64));
        }
        out.add(
            "types",
            limited(s.region_types(k), c.limit(), |t| {
                record([
                    ("bytes", bytes(t.bytes)),
                    ("objects", int(t.objects)),
                    ("type", text(t.name)),
                ])
            }),
        );
        return Ok(out);
    }
    let rows = match c.arguments.value("--by").unwrap_or("generation") {
        "generation" => s.by_generation(),
        "queue" => s.by_queue(),
        "size" => s.by_size_class(),
        other => {
            return Err(Error(format!(
                "unknown --by {other}; use generation, queue or size"
            )));
        }
    };
    Ok(record([
        (
            "regions",
            list(rows.iter().map(|g| {
                record([
                    ("regions", int(g.regions)),
                    ("committed", bytes(g.committed)),
                    ("used", bytes(g.used)),
                    ("live", bytes(g.live)),
                    ("pendingFinalization", bytes(g.pending)),
                    ("dead", bytes(g.dead())),
                    ("free", bytes(g.free)),
                    ("unparsed", bytes(g.unparsed)),
                    ("gcStorage", bytes(g.gc_storage)),
                    ("committedUnused", bytes(g.committed_unused)),
                    ("name", text(&g.name)),
                ])
            })),
        ),
        (
            "occupancyOf2MBRegions",
            list(s.occupancy().iter().map(|h| {
                let mut row = record([("generation", text(&h.generation))]);
                for (k, label) in OccupancyHistogram::LABELS.iter().enumerate() {
                    row.add(*label, int(h.counts[k]));
                }
                row
            })),
        ),
    ]))
}

pub fn memory(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let residency = s.residency();
    let agreeing = residency.as_ref().filter(|r| r.agrees());
    let mut out = record([(
        "note",
        text(
            "sizes are memory in the dump (readable mappings), and pages that are not all zero \
                as an estimate of resident memory",
        ),
    )]);
    out.add(
        "byKind",
        list(s.native.categories().iter().map(|category| {
            let mut row = record([
                ("mappings", int(category.mappings)),
                ("bytes", bytes(category.bytes)),
                ("nonZero", bytes(category.non_zero_bytes)),
            ]);
            if let Some(resident) = agreeing {
                row.add(
                    "rss",
                    bytes(
                        s.native
                            .mappings
                            .iter()
                            .filter(|m| m.category == category.name)
                            .map(|m| resident.rss.get(&m.start).copied().unwrap_or(0))
                            .sum(),
                    ),
                );
            }
            row.add("kind", text(&category.name));
            row
        })),
    );
    if let Some(r) = residency {
        let mut value = record([
            ("agrees", Value::Bool(r.agrees())),
            ("dumpMappings", bytes(r.dump_bytes)),
            ("sameRangeInSmaps", bytes(r.same_range)),
            ("anonymousPagesWithData", bytes(r.anonymous_data)),
            ("notResidentNorSwapped", bytes(r.data_not_resident)),
        ]);
        if let Some(w) = r.warning() {
            value.add("warning", text(w));
        }
        out.add("smapsMatch", value);
    }
    if let Some(smaps) = &s.smaps {
        let mut groups = BTreeMap::<String, Vec<_>>::new();
        for m in smaps {
            groups.entry(m.category(&s.heap)).or_default().push(m);
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(|(an, a), (bn, b)| {
            b.iter()
                .map(|m| m.rss)
                .sum::<u64>()
                .cmp(&a.iter().map(|m| m.rss).sum())
                .then(an.cmp(bn))
        });
        out.add(
            "residentFromSmaps",
            list(groups.iter().map(|(name, items)| {
                record([
                    ("mappings", int(items.len())),
                    ("size", bytes(items.iter().map(|m| m.end - m.start).sum())),
                    ("rss", bytes(items.iter().map(|m| m.rss).sum())),
                    ("hugePages", bytes(items.iter().map(|m| m.anon_huge).sum())),
                    (
                        "private",
                        bytes(items.iter().map(|m| m.private_bytes).sum()),
                    ),
                    ("kind", text(name)),
                ])
            })),
        );
    }
    Ok(out)
}

pub fn garbage(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let g = s.garbage()?;
    Ok(record([
        ("deadObjects", int(g.dead_count)),
        ("deadBytes", bytes(g.dead_bytes)),
        ("unreferencedDeadObjects", int(g.unreferenced.len())),
        (
            "note",
            text(
                "an unreferenced dead object has no dead referrer: a garbage structure starts \
                    there; holds = the dead objects only it leads to",
            ),
        ),
        (
            "byType",
            limited(s.garbage_by_type()?, c.limit(), |e| {
                record([
                    ("holds", bytes(e.retained)),
                    ("unreferenced", int(e.unreferenced)),
                    (
                        "example",
                        optional_address(e.samples.first().map(|&i| s.heap.addresses[i])),
                    ),
                    ("type", text(e.kind)),
                ])
            }),
        ),
    ]))
}

pub fn fragmentation(c: &Context<'_>) -> Result<Value> {
    let f = c.snapshot.fragmentation()?;
    let contributors = |items: &[Contributor]| {
        limited(items, c.limit(), |e| {
            record([
                ("waste", bytes(e.waste as u64)),
                ("regions", int(e.regions.len())),
                ("live", bytes(e.live_bytes)),
                (
                    "example",
                    optional_address(e.samples.first().map(|&i| c.snapshot.heap.addresses[i])),
                ),
                ("name", text(&e.name)),
            ])
        })
    };
    let mut out = record([
        ("waste", bytes(f.total_waste)),
        (
            "note",
            text(
                "Waste is dead objects and unused committed space, attributed to live objects by size. \
                    GC storage, objects pending finalization and unparsed bytes are excluded.",
            ),
        ),
        (
            "reasons",
            list(f.reasons.iter().map(|r| {
                record([
                    ("waste", bytes(r.waste)),
                    ("regions", int(r.regions)),
                    ("live", bytes(r.live)),
                    ("reason", text(&r.name)),
                    ("explanation", text(&r.explanation)),
                ])
            })),
        ),
    ]);
    if !f.gen2_by_heap.is_empty() {
        out.add(
            "gen2ByHeap",
            record([
                (
                    "note",
                    text(
                        "a blocking gen2 GC compacts when, on one heap, free gaps and dead objects \
                        in gen0, gen1 and gen2 are at least 25% of their size and 200000 bytes; \
                        with server GC all heaps then compact; background gen2 GCs never compact",
                    ),
                ),
                (
                    "heaps",
                    list(f.gen2_by_heap.iter().map(|h| {
                        let share = if h.size > 0 {
                            100. * h.fragmentation as f64 / h.size as f64
                        } else {
                            0.
                        };
                        record([
                            (
                                "heap",
                                if h.heap < 0 {
                                    text("workstation")
                                } else {
                                    int(h.heap)
                                },
                            ),
                            ("gen0To2", bytes(h.size)),
                            ("freeAndDead", bytes(h.fragmentation)),
                            ("share", text(format!("{share:.0}%"))),
                            ("aboveThreshold", Value::Bool(h.above_threshold())),
                        ])
                    })),
                ),
            ]),
        );
    }
    out.add("byApplicationCode", contributors(&f.by_code));
    out.add("bySurvivorType", contributors(&f.by_type));
    out.add("byOwner", contributors(&f.by_owner));
    Ok(out)
}
