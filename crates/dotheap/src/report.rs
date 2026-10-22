use dotheap::{
    arguments::Arguments,
    commands::{self, Context},
    output::*,
};
use heap_core::{
    Result,
    analysis::{HeapSnapshot, RegionStats},
    handles,
    heap::Gc,
    native_graph::Reach,
    type_names::short,
};
use std::collections::{BTreeMap, HashSet};

pub fn report(s: &HeapSnapshot, arguments: &Arguments) -> Result<String> {
    let top = arguments
        .value("--top")
        .and_then(|n| n.parse::<usize>().ok())
        .map(|n| if n == 0 { usize::MAX } else { n })
        .unwrap_or(30usize);
    let heap = &s.heap;
    let graph = &s.graph;
    let mut out = String::new();
    let section = |out: &mut String, title: &str, value: Value| {
        out.push_str(&format!("\n## {title}\n{}\n", value.text()));
    };
    let command = |name: &str| -> Result<Value> {
        let mut a = Arguments {
            command: name.into(),
            core: arguments.core.clone(),
            ..Default::default()
        };
        a.values.insert(
            "--limit".into(),
            if top == usize::MAX {
                "0".into()
            } else {
                top.to_string()
            },
        );
        commands::run(&Context {
            snapshot: s,
            arguments: &a,
        })
    };
    section(
        &mut out,
        "Memory in the dump by kind (readable mapped memory, not RSS)",
        command("memory")?,
    );
    out.push_str(&format!(
        "non-zero pages in total: {} (an estimate of resident memory without smaps)\n",
        size(s.native.categories().iter().map(|c| c.non_zero_bytes).sum())
    ));
    if let Some(smaps) = &s.smaps {
        let process_rss: u64 = smaps.iter().map(|m| m.rss).sum();
        let heap_rss = s.heap_residency().map(|r| r.0).unwrap_or(0);
        let t = s.totals();
        out.push_str(&format!(
            "heap RSS {} = committed {} ({} resident), used {}, objects {}, live {}\nlive objects \
            are {} of heap RSS and {} of process RSS\n",
            size(heap_rss),
            size(t.committed),
            percent(heap_rss, t.committed),
            size(t.used),
            size(t.objects),
            size(t.live),
            percent(t.live, heap_rss),
            percent(t.live, process_rss)
        ));
    }
    let malloc = &s.native.malloc_heaps;
    if !malloc.is_empty() {
        section(
            &mut out,
            "glibc malloc",
            record([
                ("version", optional(s.native.glibc_version.as_ref())),
                ("heaps", int(malloc.len())),
                (
                    "fullyWalked",
                    int(malloc.iter().filter(|h| h.walk_complete).count()),
                ),
                (
                    "arenas",
                    int(malloc.iter().map(|h| h.arena).collect::<HashSet<_>>().len()),
                ),
                ("inUse", bytes(malloc.iter().map(|h| h.in_use_bytes).sum())),
                (
                    "inUseChunks",
                    int(malloc.iter().map(|h| h.in_use_chunks).sum::<usize>()),
                ),
                ("free", bytes(malloc.iter().map(|h| h.free_bytes).sum())),
                (
                    "freeChunks",
                    int(malloc.iter().map(|h| h.free_chunks).sum::<usize>()),
                ),
                ("topChunks", bytes(malloc.iter().map(|h| h.top_bytes).sum())),
                ("largeAllocations", int(s.native.mmapped_chunks)),
                ("largeAllocationBytes", bytes(s.native.mmapped_bytes)),
                (
                    "freeChunksResident",
                    bytes(malloc.iter().map(|h| h.free_resident).sum()),
                ),
                (
                    "topChunksResident",
                    bytes(malloc.iter().map(|h| h.top_resident).sum()),
                ),
                (
                    "arenaHeaders",
                    bytes(malloc.iter().map(|h| h.header_bytes).sum()),
                ),
                (
                    "mallocTrimWouldRelease",
                    bytes(malloc.iter().map(|h| h.trim_releasable).sum()),
                ),
            ]),
        );
        let mut arenas = BTreeMap::<u64, (usize, u64, u64)>::new();
        for h in malloc {
            let entry = arenas.entry(h.arena).or_default();
            entry.0 += 1;
            entry.1 += h.free_resident + h.top_resident;
            entry.2 += h.in_use_bytes;
        }
        let mut arenas: Vec<_> = arenas.into_iter().collect();
        arenas.sort_by_key(|(_, (_, free, used))| std::cmp::Reverse((*free, *used)));
        section(
            &mut out,
            "Arenas by resident free and top memory",
            limited(arenas, 12, |(a, (count, free, used))| {
                record([
                    ("arena", address(a)),
                    ("free", bytes(free)),
                    ("used", bytes(used)),
                    ("heaps", int(count)),
                ])
            }),
        );
    }
    if s.dump.platform.is_linux() && !malloc.is_empty() {
        let native = s.native_graph()?;
        section(
            &mut out,
            "malloc chunks by what holds them (conservative pointer scan)",
            command("native")?,
        );
        out.push_str(&format!(
            "tcache entries {}, fastbin entries {} (freed, counted as free)\n",
            native.tcache_chunks, native.fastbin_chunks
        ));
        let groups = s.chunk_groups()?;
        for reach in Reach::ALL {
            section(
                &mut out,
                &format!("{}, largest groups", reach.title()),
                limited(groups.iter().filter(|g| g.reach == reach), top / 2, |g| {
                    record([
                        ("bytes", bytes(g.bytes)),
                        ("chunks", int(g.chunks.len())),
                        ("label", text(&g.label)),
                    ])
                }),
            );
        }
        if !native.handle_references.is_empty() {
            let mut slots = BTreeMap::new();
            for h in &graph.handles {
                slots.entry(h.slot).or_insert(h);
            }
            let mut by_target = BTreeMap::<String, usize>::new();
            for &slot in native.handle_references.values().flatten() {
                if let Some(h) = slots.get(&slot) {
                    let target = h
                        .target
                        .map(|i| s.type_name(i))
                        .unwrap_or_else(|| "(no object)".into());
                    *by_target
                        .entry(format!("{} -> {target}", handles::type_name(h.kind)))
                        .or_default() += 1;
                }
            }
            let mut rows: Vec<_> = by_target.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            section(
                &mut out,
                "GC handles stored in malloc memory (native code that keeps managed objects alive)",
                limited(rows, top, |(target, count)| {
                    record([("handles", int(count)), ("target", text(target))])
                }),
            );
        }
    }
    let stacks = s.thread_stacks()?;
    let mut threads: Vec<_> = stacks.threads.iter().collect();
    threads.sort_by(|a, b| {
        b.touched_bytes()
            .cmp(&a.touched_bytes())
            .then(a.tid.cmp(&b.tid))
    });
    section(
        &mut out,
        "Threads by stack depth (frames estimated from return addresses)",
        limited(threads, top, |t| {
            record([
                ("touched", bytes(t.touched_bytes())),
                ("inUse", bytes(t.in_use_bytes())),
                ("frames", int(t.frames.len())),
                ("thread", text(stacks.label(t.tid))),
                (
                    "topFrames",
                    list(stacks.top_frames(t, 6).into_iter().map(text)),
                ),
            ])
        }),
    );

    let totals = s.totals();
    let headers: u64 = heap.regions.iter().map(|r| r.header_bytes()).sum();
    let gc = match heap.kind.gc {
        Gc::Satori => "Satori",
        Gc::CoreClr => "CoreCLR",
        Gc::None => "none",
    };
    out.push_str(&format!(
        "\n## Heap totals\n{}, {gc} GC: {} ({})\n",
        s.dump.platform.name(),
        heap.kind.layout,
        heap.kind.detection
    ));
    if let Some(runtime) = &s.runtime {
        out.push_str(&format!(
            "runtime {}: {}, build ID {}, contract descriptor {}\n",
            std::path::Path::new(&runtime.path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            runtime.version.as_deref().unwrap_or("no version string"),
            runtime.build_id.as_deref().unwrap_or("unknown"),
            runtime
                .descriptor
                .as_ref()
                .map(|d| format!("with {} types", d.types.len()))
                .unwrap_or_else(|| "not found".into())
        ));
    }
    out.push_str(
        &record([
            ("pages", int(heap.pages.len())),
            (
                "pageAddresses",
                list(heap.pages.iter().map(|p| address(p.start))),
            ),
            (
                "reserved",
                bytes(heap.pages.iter().map(|p| p.end - p.start).sum()),
            ),
            ("regions", int(heap.regions.len())),
            ("objects", int(heap.count())),
            ("liveObjects", int(graph.live_count)),
            ("pendingFinalizationObjects", int(graph.pending_count)),
            ("threads", int(s.dump.threads.len())),
            ("rootSources", int(graph.sources.len())),
            ("handleSegments", int(graph.handle_segment_addresses.len())),
            (
                "handleCounts",
                list(
                    graph.handle_counts.iter().map(|(kind, count)| {
                        record([("count", int(*count)), ("kind", text(kind))])
                    }),
                ),
            ),
            ("committed", bytes(totals.committed)),
            ("used", bytes(totals.used)),
            ("live", bytes(totals.live)),
            ("pendingFinalization", bytes(totals.pending)),
            ("dead", bytes(totals.dead())),
            ("free", bytes(totals.free)),
            ("regionHeaders", bytes(headers)),
            ("unparsed", bytes(totals.unparsed)),
            (
                "committedAboveUsed",
                bytes(totals.committed.saturating_sub(totals.used)),
            ),
        ])
        .text(),
    );
    out.push_str(
        "\nDead objects have not been swept. Free bytes are swept gaps; \
        Satori also includes memory not yet allocated.\n",
    );
    out.push_str(if heap.kind.gc == Gc::Satori {
        "Region headers contain the mark bitmap and index. Unparsed bytes include \
        allocation ranges, zero tails, GC data and new huge objects.\n"
    } else {
        "Region headers are space before the first object. Unparsed bytes include \
        allocation ranges and zero tails.\n"
    });
    out.push_str(&format!(
        "fragmentation (dead + free) / (objects + free) in regions of a generation: {:.0}%\n",
        100. * s.fragmentation_share()
    ));
    if !heap.finalization.is_empty() {
        out.push_str(&format!(
            "finalization: {} objects registered, {} waiting for the finalizer thread\n",
            heap.finalization
                .iter()
                .map(|f| (f.registered_end - f.registered_start) / 8)
                .sum::<u64>(),
            heap.finalization.iter().map(|f| f.waiting()).sum::<usize>()
        ));
    }
    let missing = s.names.missing_assemblies();
    if !missing.is_empty() {
        out.push_str(&format!(
            "no metadata for {} assemblies (pass --dlls with the app and runtime folders): {}{}\n",
            missing.len(),
            missing
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if missing.len() > 12 { ", …" } else { "" }
        ));
    }
    let stopped = heap.regions.iter().filter(|r| r.walk_stopped).count();
    if stopped > 0 {
        out.push_str(&format!("walk stopped early in {stopped} regions\n"));
    }
    let mut attached = BTreeMap::<i32, Vec<_>>::new();
    for r in heap
        .regions
        .iter()
        .filter(|r| r.owner_thread_tag != 0 || r.attachment_point != 0)
    {
        attached
            .entry(r.owner_thread.or(r.allocating_thread).unwrap_or(-1))
            .or_default()
            .push(r);
    }
    if !attached.is_empty() {
        section(
            &mut out,
            "Regions attached to allocation contexts (gen0 = escape tracking for one thread; the \
                allocator's shared regions show no thread)",
            list(attached.into_iter().map(|(thread, regions)| {
                record([
                    (
                        "thread",
                        if thread < 0 {
                            text("no thread found")
                        } else {
                            Value::Id(thread as i64)
                        },
                    ),
                    ("regions", int(regions.len())),
                    (
                        "escapeTracking",
                        int(regions.iter().filter(|r| r.owner_thread_tag != 0).count()),
                    ),
                    ("used", bytes(regions.iter().map(|r| r.used_bytes()).sum())),
                    ("live", bytes(regions.iter().map(|r| r.live_bytes).sum())),
                ])
            })),
        );
    }
    for (title, rows) in [
        ("By generation", s.by_generation()),
        ("By containing queue", s.by_queue()),
        ("By region size", s.by_size_class()),
    ] {
        section(&mut out, title, region_table(&rows));
    }
    section(
        &mut out,
        "Live occupancy of 2 MB regions (live bytes / region size)",
        list(s.occupancy().iter().map(|h| {
            let mut row = record([("generation", text(&h.generation))]);
            for (label, count) in heap_core::analysis::OccupancyHistogram::LABELS
                .iter()
                .zip(h.counts)
            {
                row.add(*label, int(count));
            }
            row.add("usedBelow25Percent", bytes(h.used_below_quarter));
            row
        })),
    );
    for sort in ["live", "retained", "dead"] {
        let mut stats: Vec<_> = s.type_stats.iter().collect();
        let count = |t: &heap_core::analysis::TypeStats| match sort {
            "live" => t.live_bytes,
            "dead" => t.dead_bytes(),
            _ => t.retained,
        };
        stats.sort_by(|a, b| count(b).cmp(&count(a)).then(a.name.cmp(&b.name)));
        section(
            &mut out,
            &if top == usize::MAX {
                format!("Types by {sort} bytes")
            } else {
                format!("Top {top} types by {sort} bytes")
            },
            limited(stats, top, |t| {
                record([
                    ("live", bytes(t.live_bytes)),
                    ("liveCount", int(t.live_count)),
                    ("pending", bytes(t.pending_bytes)),
                    ("dead", bytes(t.dead_bytes())),
                    ("deadCount", int(t.dead_count())),
                    ("retained", bytes(t.retained)),
                    ("inGen2", text(percent(t.gen2_bytes, t.bytes))),
                    ("type", text(short(&t.name))),
                ])
            }),
        );
    }
    section(&mut out, "GC handles by target type", command("handles")?);
    section(
        &mut out,
        "Largest children of the dominator tree root (reachable from more than one root source, or root sources)",
        try_limited(
            s.dominated_children(graph.root_node())?.iter(),
            15,
            |&node| {
                Ok(record([
                    ("retained", bytes(graph.retained[node])),
                    ("name", text(s.node_name(node)?)),
                ]))
            },
        )?,
    );
    if let Some(explain) = arguments.value("--explain") {
        let candidates: Vec<_> = (0..heap.count())
            .filter(|&i| graph.live[i] != 0 && s.type_name(i).contains(explain))
            .collect();
        if let Some(&i) = candidates.iter().max_by_key(|&&i| heap.size(i)) {
            let a = Arguments {
                command: "object".into(),
                core: arguments.core.clone(),
                positional: vec![hex(heap.addresses[i])],
                ..Default::default()
            };
            section(
                &mut out,
                &format!(
                    "Explain {} {} ({} live objects match)",
                    hex(heap.addresses[i]),
                    short(&s.type_name(i)),
                    candidates.len()
                ),
                commands::run(&Context {
                    snapshot: s,
                    arguments: &a,
                })?,
            );
        }
    }
    if arguments.flag("--diagnostics") {
        section(
            &mut out,
            "Garbage: unreferenced dead objects (no dead object references them) by the garbage they hold",
            command("garbage")?,
        );
        section(
            &mut out,
            "Fragmentation: committed region memory without live objects, by what keeps it",
            command("fragmentation")?,
        );
        let fragmentation = s.fragmentation()?;
        section(
            &mut out,
            "Application code above the surviving objects (namespace · owner type)",
            limited(&fragmentation.by_code, top, |c| {
                record([
                    ("kept", bytes(c.waste as u64)),
                    ("survivors", bytes(c.live_bytes)),
                    ("deadAround", bytes(c.dead_around_bytes as u64)),
                    ("name", text(&c.name)),
                    (
                        "survivorTypes",
                        list(
                            c.survivor_types
                                .iter()
                                .take(3)
                                .map(|t| text(format!("{} {}", short(&t.name), size(t.bytes)))),
                        ),
                    ),
                    (
                        "deadTypes",
                        list(
                            c.dead_around.iter().take(3).map(|t| {
                                text(format!("{} {}", short(&t.name), size(t.bytes as u64)))
                            }),
                        ),
                    ),
                ])
            }),
        );
        for (name, title) in [
            ("strings", "Duplicate strings (live)"),
            ("collections", "Collection capacity (live)"),
            (
                "zero-arrays",
                "Mostly zero arrays (live, >= 1 KB, >= 90% zero bytes)",
            ),
            ("boxed", "Boxed values"),
            ("finalizable", "Objects with finalizers"),
        ] {
            section(&mut out, title, command(name)?);
        }
    }
    Ok(out)
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        "-".into()
    } else {
        format!("{:.0}%", 100. * part as f64 / whole as f64)
    }
}

fn region_table(rows: &[RegionStats]) -> Value {
    list(rows.iter().map(|r| {
        record([
            ("regions", int(r.regions)),
            ("span", bytes(r.span)),
            ("committed", bytes(r.committed)),
            ("used", bytes(r.used)),
            ("objects", bytes(r.objects)),
            ("live", bytes(r.live)),
            ("pending", bytes(r.pending)),
            ("dead", bytes(r.dead())),
            ("free", bytes(r.free)),
            ("liveOfUsed", text(percent(r.live, r.used))),
            (
                "satoriOccupancy",
                r.satori_occupancy.map(bytes).unwrap_or(Value::Null),
            ),
            ("name", text(&r.name)),
        ])
    }))
}
