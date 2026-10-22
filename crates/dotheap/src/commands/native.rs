use super::*;
use heap_core::{
    bytes::ReadBytes,
    core_dump::Cursor,
    native_graph::{ChunkState, Reach},
    native_memory::STATE_ATTACHED_THREADS,
};
use std::collections::{BTreeMap, BTreeSet};

pub fn native(c: &Context<'_>) -> Result<Value> {
    c.require_malloc()?;
    let s = c.snapshot;
    let g = s.native_graph()?;
    let reach = c
        .arguments
        .value("--reach")
        .map(|r| match r {
            "native" => Ok(Reach::NativeRoots),
            "live" => Ok(Reach::LiveManaged),
            "pending" => Ok(Reach::PendingManaged),
            "dead" => Ok(Reach::DeadManaged),
            "unreferenced" => Ok(Reach::Unreferenced),
            _ => Err(Error(format!(
                "unknown --reach {r}; use native, live, pending, dead or unreferenced"
            ))),
        })
        .transpose()?;
    let groups = s.chunk_groups()?;
    let groups = groups.iter().filter(|group| {
        reach.is_none_or(|r| group.reach == r)
            && c.arguments
                .value("--match")
                .is_none_or(|q| contains(&group.label, q))
            && c.arguments
                .value("--library")
                .is_none_or(|q| group.library.as_deref().is_some_and(|l| contains(l, q)))
    });
    let mut libraries = BTreeMap::<String, (usize, u64)>::new();
    for (i, ch) in g.chunks.iter().enumerate().filter(|(i, _)| g.is_in_use(*i)) {
        let library = g.library(i).unwrap_or_else(|| {
            if [
                Reach::LiveManaged,
                Reach::PendingManaged,
                Reach::DeadManaged,
            ]
            .contains(&g.reach[i])
            {
                "(held by managed objects)".into()
            } else {
                "(no C++ object found)".into()
            }
        });
        let entry = libraries.entry(library).or_default();
        entry.0 += 1;
        entry.1 += ch.size;
    }
    let mut owners = BTreeMap::<String, (usize, u64)>::new();
    for (&owner, &bytes) in &g.native_retained {
        let entry = owners.entry(s.type_name(owner)).or_default();
        entry.0 += 1;
        entry.1 += bytes;
    }
    let mut libraries: Vec<_> = libraries.into_iter().collect();
    let mut owners: Vec<_> = owners.into_iter().collect();
    for items in [&mut libraries, &mut owners] {
        items.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(&b.0)));
    }
    let mut heaps: Vec<_> = s.native.malloc_heaps.iter().collect();
    heaps.sort_by(|a, b| {
        (b.top_resident + b.free_resident)
            .cmp(&(a.top_resident + a.free_resident))
            .then(a.start.cmp(&b.start))
    });
    Ok(record([
        (
            "note",
            text(
                "Managed references come from pointer fields and array elements. Native memory, stacks and registers are scanned conservatively; matching values can be stale or unrelated. Native roots take priority over managed references. Missing layouts and encoded pointers can hide references; unreferenced chunks are leak candidates.",
            ),
        ),
        (
            "heldBy",
            list(g.totals().iter().map(|t| {
                record([
                    ("bytes", bytes(t.bytes)),
                    ("chunks", int(t.chunks)),
                    ("heldBy", text(t.reach.holder())),
                ])
            })),
        ),
        (
            "freedChunksCachedByGlibc",
            int(g.tcache_chunks + g.fastbin_chunks),
        ),
        (
            "mallocParameters",
            s.malloc_parameters()?
                .as_ref()
                .as_ref()
                .map(|p| {
                    record([
                        ("trimThreshold", bytes(p.trim_threshold)),
                        ("mmapThreshold", bytes(p.mmap_threshold)),
                        ("arenaMax", int(p.arena_max)),
                        ("dynamicThreshold", Value::Bool(p.dynamic_threshold)),
                        ("mmappedChunks", int(p.mmapped_chunks)),
                        ("mmappedBytes", bytes(p.mmapped_bytes)),
                        ("address", address(p.address)),
                    ])
                })
                .unwrap_or(Value::Null),
        ),
        (
            "heaps",
            limited(heaps, c.limit(), |h| {
                record([
                    ("heap", address(h.start)),
                    (
                        "arena",
                        text(if h.main_arena {
                            "main (brk)".into()
                        } else {
                            hex(h.arena)
                        }),
                    ),
                    ("size", bytes(h.end - h.start)),
                    ("inUse", bytes(h.in_use_bytes)),
                    ("freeResident", bytes(h.free_resident)),
                    ("topResident", bytes(h.top_resident)),
                    ("top", bytes(h.top_bytes)),
                    ("trimWouldFree", bytes(h.trim_releasable)),
                    (
                        "arenaThreads",
                        h.arena
                            .checked_add(STATE_ATTACHED_THREADS)
                            .and_then(|a| s.dump.u64(a))
                            .map(int)
                            .unwrap_or(Value::Null),
                    ),
                ])
            }),
        ),
        (
            "byLibrary",
            limited(libraries, c.limit(), |(name, (count, size))| {
                record([
                    ("bytes", bytes(size)),
                    ("chunks", int(count)),
                    ("library", text(name)),
                ])
            }),
        ),
        (
            "managedTypesByNativeMemoryKeptAlone",
            limited(owners, c.limit(), |(name, (count, size))| {
                record([
                    ("bytes", bytes(size)),
                    ("objects", int(count)),
                    ("type", text(name)),
                ])
            }),
        ),
        (
            "groups",
            limited(groups, c.limit(), |group| {
                let largest = group
                    .chunks
                    .iter()
                    .copied()
                    .min_by_key(|&i| (std::cmp::Reverse(g.chunks[i as usize].size), i))
                    .map(|i| g.chunks[i as usize].user_start());
                record([
                    ("bytes", bytes(group.bytes)),
                    ("chunks", int(group.chunks.len())),
                    ("heldBy", text(group.reach.holder())),
                    ("library", text(group.library.as_deref().unwrap_or(""))),
                    ("largest", optional_address(largest)),
                    ("label", text(&group.label)),
                ])
            }),
        ),
    ]))
}

pub fn chunk(c: &Context<'_>) -> Result<Value> {
    c.require_malloc()?;
    let s = c.snapshot;
    let g = s.native_graph()?;
    let a = c.address(0)?;
    let k = g.chunk(a).ok_or_else(|| {
        Error(format!(
            "{} is not in a malloc chunk: {}",
            hex(a),
            c.where_is(a)
        ))
    })?;
    let ch = &g.chunks[k];
    let mut out = record([
        ("address", address(ch.user_start())),
        ("header", address(ch.start)),
        ("size", bytes(ch.size)),
    ]);
    match ch.state {
        ChunkState::InUse | ChunkState::Mmapped => {
            out.add(
                "state",
                text(if ch.state == ChunkState::Mmapped {
                    "in use (own mapping)"
                } else {
                    "in use"
                }),
            );
            out.add("heldBy", text(g.reach[k].holder()));
            out.add("label", text(g.group_label(k, s)?));
            out.add("cppClass", optional(g.class_name(k)));
            out.add("library", optional(g.library(k)));
        }
        ChunkState::Free => out.add("state", text("free")),
        ChunkState::Cached => out.add("state", text("freed, in a tcache or fastbin list")),
        ChunkState::Mapping => out.add("state", text("anonymous mapping (not malloc)")),
    }
    if g.root_chunks.contains(&(k as i32)) {
        out.add("nativeRoot", optional(g.root_source.get(&(k as i32))));
    }
    if let Some(owner) = g.owning_object(k) {
        out.add("keptBy", c.row(owner, false));
    }
    let ownership = s.ownership()?;
    let node = ownership.chunk_node(k);
    if ownership.is_reachable(node) {
        out.add(
            "retained",
            record([
                ("managed", bytes(ownership.retained_managed[node])),
                ("native", bytes(ownership.retained_native[node])),
            ]),
        );
        out.add(
            "pathToRoot",
            c.ownership_path(&ownership.path_to_root(node))?,
        );
    }
    let chunk_row = |&j: &i32| -> Result<Value> {
        let j = j as usize;
        Ok(record([
            ("address", address(g.chunks[j].user_start())),
            ("size", bytes(g.chunks[j].size)),
            ("label", text(g.group_label(j, s)?)),
        ]))
    };
    out.add(
        "pointedToByChunks",
        try_limited(g.referrers(k), c.limit(), chunk_row)?,
    );
    out.add(
        "pointsToChunks",
        try_limited(g.pointers(k), c.limit(), chunk_row)?,
    );
    if let Some(owners) = g.chunk_owners.get(&(k as i32)) {
        out.add(
            "managedObjectsPointingToIt",
            limited(owners, c.limit(), |&i| {
                record([
                    ("address", address(s.heap.addresses[i])),
                    ("field", optional(g.owner_field(i, k, s))),
                    ("state", text(s.graph.state(i))),
                    ("type", text(s.type_name(i))),
                ])
            }),
        );
    }
    if let Some(refs) = g.managed_references.get(&(k as i32)) {
        out.add(
            "managedReferenceNote",
            text("Raw address matches can be stale or unrelated. They do not establish that this chunk keeps these objects alive."),
        );
        out.add(
            "possibleManagedReferences",
            limited(
                refs.iter().copied().collect::<BTreeSet<_>>(),
                c.limit(),
                |i| c.row(i, false),
            ),
        );
    }
    if let Some(handles) = g.handle_references.get(&(k as i32)) {
        let mut slots = BTreeMap::new();
        for h in &s.graph.handles {
            slots.entry(h.slot).or_insert(h);
        }
        out.add(
            "gcHandlesItHolds",
            limited(
                handles.iter().copied().collect::<BTreeSet<_>>(),
                c.limit(),
                |slot| {
                    let handle = slots.get(&slot);
                    let target = handle.and_then(|h| h.target);
                    record([
                        ("slot", address(slot)),
                        (
                            "handleType",
                            text(
                                handle
                                    .map(|h| heap_core::handles::type_name(h.kind))
                                    .unwrap_or_else(|| "?".into()),
                            ),
                        ),
                        (
                            "target",
                            optional_address(target.map(|i| s.heap.addresses[i])),
                        ),
                        ("targetType", optional(target.map(|i| s.type_name(i)))),
                    ])
                },
            ),
        );
    }
    if let Some(data) = s.dump.bytes(
        ch.user_start(),
        (ch.user_end() - ch.user_start()).min(256) as usize,
    ) {
        out.add(
            "firstBytes",
            hex_lines(&data, |offset| format!("+{offset:03x}")),
        );
    }
    Ok(out)
}

fn hex_lines(data: &[u8], position: impl Fn(usize) -> String) -> Value {
    list(data.chunks(16).enumerate().map(|(index, line)| {
        let hex = line
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ascii: String = line
            .iter()
            .map(|&b| {
                if (0x20..=0x7e).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        text(format!("{}  {hex:48} {ascii}", position(index * 16)))
    }))
}

pub fn read_bytes(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let a = c.address(0)?;
    let count = c
        .arguments
        .value("--count")
        .map(|n| {
            n.parse::<usize>()
                .map_err(|_| Error("--count needs a non-negative byte count".into()))
        })
        .transpose()?
        .unwrap_or(256)
        .min(1 << 20);
    let data = s
        .dump
        .bytes(a, count)
        .or_else(|| s.dump.bytes(a, 8))
        .ok_or_else(|| Error(format!("{} is not in the dump: {}", hex(a), c.where_is(a))))?;
    let mut words = Vec::new();
    let stacks = s.thread_stacks()?;
    for offset in (0..data.len() / 8 * 8).step_by(8) {
        let v = data.u64(offset).unwrap();
        let mut row = record([
            ("address", address(a + offset as u64)),
            ("value", address(v)),
        ]);
        if let Some(i) = s.heap.object_at(v) {
            row.add("pointsTo", text(s.type_name(i)));
        } else if let Some(f) = stacks.code.locate(v) {
            row.add("pointsTo", text(f.name));
        }
        words.push(row);
    }
    Ok(record([
        ("address", address(a)),
        ("in", text(c.where_is(a))),
        ("hex", hex_lines(&data, |offset| hex(a + offset as u64))),
        ("words", list(words)),
    ]))
}

pub fn find(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let value = c.address(0)?;
    let mut hits = Vec::new();
    let mut omitted = 0;
    let mut cursor = Cursor::new(&s.dump, true);
    for segment in &s.dump.segments {
        for word in 0..segment.size / 8 {
            let a = segment.address + word * 8;
            if cursor.u64(a) == Some(value) {
                if hits.len() < c.limit() {
                    hits.push(a);
                } else {
                    omitted += 1;
                }
            }
        }
    }
    let stacks = s.thread_stacks()?;
    let native = if c.is_linux() {
        Some(s.native_graph()?)
    } else {
        None
    };
    let mut rows = Vec::new();
    for a in hits {
        let mut row = record([("address", address(a))]);
        let location = if let Some(i) = s.heap.object_containing(a) {
            row.add("state", text(s.graph.state(i)));
            format!(
                "object {} {} +{}",
                hex(s.heap.addresses[i]),
                s.type_name(i),
                a - s.heap.addresses[i]
            )
        } else if let Some(t) = stacks
            .threads
            .iter()
            .find(|t| a >= t.stack_pointer && a < t.stack_end)
        {
            format!(
                "stack of {}, frame of {}",
                stacks.label(t.tid),
                stacks.describe(stacks.owner(a, t))
            )
        } else if let Some(g) = &native
            && let Some(k) = g.chunk(a)
        {
            format!(
                "malloc chunk {} +{}, {}, held by {}",
                hex(g.chunks[k].user_start()),
                a - g.chunks[k].user_start(),
                g.group_label(k, s)?,
                g.reach[k].holder()
            )
        } else {
            c.where_is(a)
        };
        row.add("where", text(location));
        rows.push(row);
    }
    Ok(record([
        ("value", address(value)),
        ("in", text(c.where_is(value))),
        ("foundAt", Value::List(rows, omitted)),
    ]))
}
