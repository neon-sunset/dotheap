use super::*;
use heap_core::{heap::Gc, native_graph::Reach};

pub fn summary(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let heap = &s.heap;
    let graph = &s.graph;
    let versions = s.versions();
    let mut dump = record([
        ("path", text(&c.arguments.core)),
        ("platform", text(s.dump.platform.name())),
        ("gc", text(enum_text(heap.kind.gc))),
        ("gcLayout", text(&heap.kind.layout)),
    ]);
    if let Some(runtime) = &s.runtime {
        dump.add(
            "runtime",
            optional(
                runtime
                    .version
                    .as_ref()
                    .map(|v| v.replace("@(#)Version ", "")),
            ),
        );
        dump.add("runtimeBuildId", optional(runtime.build_id.as_ref()));
    }
    dump.add("netCore", optional(versions.net_core));
    dump.add("aspNetCore", optional(versions.asp_net_core));
    dump.add("threads", int(s.dump.threads.len()));
    dump.add("objects", int(heap.count()));
    dump.add("liveObjects", int(graph.live_count));
    dump.add("pendingFinalizationObjects", int(graph.pending_count));
    let mut out = record([("dump", dump)]);

    let (parts, resident) = s.process_memory()?;
    let total: u64 = parts.iter().map(|p| p.bytes).sum();
    let zero = s.heap_zero_pages()?;
    let mut process = record([]);
    if let Some(smaps) = &s.smaps
        && let Some((rss, huge_pages)) = s.heap_residency()
    {
        process.add(
            "rssFromSmaps",
            bytes(
                smaps
                    .iter()
                    .filter(|m| !s.dump.in_shared_cache(m.start))
                    .map(|m| m.rss)
                    .sum(),
            ),
        );
        process.add("heapResident", bytes(rss));
        process.add("heapInHugePages", bytes(huge_pages));
    }
    if !resident {
        process.add(
            "estimatedFromDump",
            record([
                ("atLeast", bytes(total.saturating_sub(zero))),
                ("atMost", bytes(total)),
            ]),
        );
        if heap.kind.gc != Gc::None {
            process.add(
                "note",
                text(format!(
                    "{} of zero-filled heap pages in whole 2 MB committed granules may be resident \
                    after writes or because huge pages back them. Supply smaps to measure RSS.",
                    size(zero)
                )),
            );
        }
    }
    process.add(
        "partsFrom",
        text(if resident {
            "RSS per smaps"
        } else {
            "the dump"
        }),
    );
    process.add(
        "parts",
        limited(parts, c.limit(), |p| {
            record([("bytes", bytes(p.bytes)), ("kind", text(p.name))])
        }),
    );
    out.add("processMemory", process);

    let totals = s.totals();
    let waiting: usize = heap.finalization.iter().map(|f| f.waiting()).sum();
    let mut managed = record([
        ("committed", bytes(totals.committed)),
        ("liveObjects", bytes(totals.live)),
        ("pendingFinalizationObjects", bytes(totals.pending)),
        ("deadObjects", bytes(totals.dead())),
        ("freeGaps", bytes(totals.free)),
        ("committedAboveUsed", bytes(totals.committed_unused)),
        ("regionHeaders", bytes(totals.headers)),
        ("unparsed", bytes(totals.unparsed)),
        ("gcStorage", bytes(totals.gc_storage)),
        ("regions", int(heap.regions.len())),
    ]);
    if !heap.finalization.is_empty() {
        managed.add("waitingForFinalizer", int(waiting));
    }
    managed.add("gcHandles", int(graph.handles.len()));
    out.add("managedHeap", managed);

    let malloc = &s.native.malloc_heaps;
    let mut unreferenced = 0;
    if !malloc.is_empty() {
        let mut m = record([
            ("glibc", optional(s.native.glibc_version.as_ref())),
            (
                "arenas",
                int(malloc.iter().map(|h| h.arena).collect::<HashSet<_>>().len()),
            ),
            ("heaps", int(malloc.len())),
            (
                "inUse",
                bytes(malloc.iter().map(|h| h.in_use_bytes).sum::<u64>() + s.native.mmapped_bytes),
            ),
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
        ]);
        if c.is_linux() {
            let native_totals = s.native_graph()?.totals();
            unreferenced = native_totals
                .iter()
                .find(|t| t.reach == Reach::Unreferenced)
                .map(|t| t.bytes)
                .unwrap_or(0);
            m.add(
                "inUseHeldBy",
                list(native_totals.iter().map(|t| {
                    record([
                        ("bytes", bytes(t.bytes)),
                        ("chunks", int(t.chunks)),
                        ("heldBy", text(t.reach.holder())),
                    ])
                })),
            );
            m.add(
                "largestGroups",
                limited(s.chunk_groups()?.iter().take(10), c.limit(), |g| {
                    record([
                        ("bytes", bytes(g.bytes)),
                        ("chunks", int(g.chunks.len())),
                        ("heldBy", text(g.reach.holder())),
                        ("label", text(&g.label)),
                    ])
                }),
            );
        }
        out.add("malloc", m);
    }
    let ownership = s.ownership()?;
    let groups = s.ownership_groups(0)?;
    out.add(
        "largestRetainers",
        try_limited(groups.iter(), c.limit().min(10), |g| c.ownership_row(g, 0))?,
    );
    let mut types: Vec<_> = s.type_stats.iter().collect();
    types.sort_by(|a, b| b.retained.cmp(&a.retained).then(a.name.cmp(&b.name)));
    out.add(
        "typesByRetained",
        limited(types.into_iter().take(10), c.limit().min(10), |t| {
            record([
                ("retained", bytes(t.retained)),
                ("liveCount", int(t.live_count)),
                ("live", bytes(t.live_bytes)),
                ("type", text(&t.name)),
            ])
        }),
    );
    out.add(
        "findings",
        list(s.summary_findings()?.iter().map(|f| {
            record([
                ("bytes", bytes(f.bytes)),
                (
                    "finding",
                    text(format!(
                        "{}{}",
                        f.text,
                        f.see
                            .as_ref()
                            .map(|see| format!("; see {see}"))
                            .unwrap_or_default()
                    )),
                ),
            ])
        })),
    );
    let mut warnings = Vec::new();
    let missing = s.names.missing_assemblies();
    if !missing.is_empty() {
        warnings.push(text(format!(
            "no metadata for {} assemblies, so some type and field names are missing; pass --dlls \
            with the app and runtime folders: {}",
            missing.len(),
            missing
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let stopped: Vec<_> = heap.regions.iter().filter(|r| r.walk_stopped).collect();
    if let Some(first) = stopped.first() {
        warnings.push(text(format!(
            "the object walk stopped early in {} regions (first {}); \
            object counts there are incomplete",
            stopped.len(),
            hex(first.start)
        )));
    }
    warnings.extend(versions.warnings.into_iter().map(text));
    if heap.kind.gc == Gc::None {
        warnings.push(text(format!(
            "no managed heap ({}): native memory only",
            heap.kind.detection
        )));
    }
    if s.dump.platform.is_linux() && malloc.is_empty() {
        warnings.push(text("no glibc malloc heaps found (another allocator or a static binary): no malloc analysis"));
    }
    if s.smaps.is_none() {
        warnings.push(text(
            "sizes are memory in the dump, not RSS; pass --smaps for resident memory",
        ));
    }
    if let Some(warning) = s.residency().and_then(|r| r.warning()) {
        warnings.push(text(warning));
    }
    out.add("warnings", list(warnings));

    let mut next = Vec::new();
    let core = c.core_word();
    for g in groups.iter() {
        if let Some(&node) = g.nodes.first()
            && let Some(a) = c.ownership_address(node)?
        {
            next.push(text(format!(
                "dotheap retained {core} {}  # what the largest retainer keeps alive",
                hex(a)
            )));
            let (command, reason) = if matches!(ownership.node(node), OwnershipNode::Object(_)) {
                ("object", "its fields and its path to a root")
            } else {
                ("chunk", "what it is and what holds it")
            };
            next.push(text(format!(
                "dotheap {command} {core} {}  # {reason}",
                hex(a)
            )));
            break;
        }
    }
    next.push(text(format!("dotheap types {core} --sort retained")));
    if totals.dead() > totals.committed / 5 {
        next.push(text(format!(
            "dotheap garbage {core}  # dead objects are {}% of committed",
            (100. * totals.dead() as f64 / totals.committed.max(1) as f64) as u64
        )));
    }
    if totals.live < totals.committed / 2 {
        next.push(text(format!(
            "dotheap fragmentation {core}  # committed is {:.1}x live",
            totals.committed as f64 / totals.live.max(1) as f64
        )));
    }
    if !malloc.is_empty() {
        next.push(text(format!(
            "dotheap native {core}  # what holds malloc memory"
        )));
    }
    if unreferenced > 1 << 20 {
        next.push(text(format!(
            "dotheap native {core} --reach unreferenced  # {} of leak candidates",
            size(unreferenced)
        )));
    }
    if waiting > 0 {
        next.push(text(format!(
            "dotheap finalizable {core}  # {waiting} objects wait for the finalizer"
        )));
    }
    next.push(text(format!(
        "dotheap findings {core}  # findings and detailed report commands"
    )));
    next.push(text(format!("dotheap threads {core}")));
    out.add("next", list(next));
    Ok(out)
}
