use super::*;
use heap_core::queries::FieldValue;

pub fn types(c: &Context<'_>) -> Result<Value> {
    let sort = c.arguments.value("--sort").unwrap_or("retained");
    if !["retained", "live", "dead", "count"].contains(&sort) {
        return Err(Error(format!(
            "unknown --sort {sort}; use retained, live, dead or count"
        )));
    }
    let mut stats: Vec<_> = c
        .snapshot
        .type_stats
        .iter()
        .filter(|s| {
            s.count > 0
                && c.arguments
                    .value("--match")
                    .is_none_or(|m| contains(&s.name, m))
        })
        .collect();
    let value = |s: &heap_core::analysis::TypeStats| match sort {
        "live" => s.live_bytes,
        "dead" => s.dead_bytes(),
        "count" => s.count as u64,
        _ => s.retained,
    };
    stats.sort_by(|a, b| value(b).cmp(&value(a)).then(a.name.cmp(&b.name)));
    Ok(record([(
        "types",
        limited(stats, c.limit(), |s| {
            record([
                ("retained", bytes(s.retained)),
                ("liveCount", int(s.live_count)),
                ("live", bytes(s.live_bytes)),
                ("pendingFinalizationCount", int(s.pending_count)),
                ("pendingFinalization", bytes(s.pending_bytes)),
                ("deadCount", int(s.dead_count())),
                ("dead", bytes(s.dead_bytes())),
                ("type", text(&s.name)),
            ])
        }),
    )]))
}

pub fn instances(c: &Context<'_>) -> Result<Value> {
    let query = c.positional(0, "type")?;
    let ids = c.matching_types(query);
    if ids.is_empty() {
        return Err(Error(format!(
            "no type matches {query}; see dotheap types {} --match <text>",
            c.core_word()
        )));
    }
    let s = c.snapshot;
    let mut objects: Vec<_> = (0..s.heap.count())
        .filter(|&i| {
            ids.contains(&s.heap.type_ids[i])
                && (!c.arguments.flag("--live") || s.graph.state(i) == "live")
                && (!c.arguments.flag("--pending") || s.graph.state(i) == "pending finalization")
                && (!c.arguments.flag("--dead") || s.graph.state(i) == "dead")
                && c.arguments
                    .value("--text")
                    .is_none_or(|text| s.identifying_text(i).is_some_and(|s| contains(&s, text)))
        })
        .collect();
    match c.arguments.value("--sort").unwrap_or("retained") {
        "retained" => objects.sort_by(|&a, &b| {
            (s.graph.retained[b], s.heap.size(b))
                .cmp(&(s.graph.retained[a], s.heap.size(a)))
                .then(a.cmp(&b))
        }),
        "size" => objects.sort_by(|&a, &b| s.heap.size(b).cmp(&s.heap.size(a)).then(a.cmp(&b))),
        other => {
            return Err(Error(format!(
                "unknown --sort {other}; use retained or size"
            )));
        }
    }
    let mut types: Vec<_> = s
        .type_stats
        .iter()
        .filter(|t| ids.contains(&t.type_id))
        .collect();
    types.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
    Ok(record([
        (
            "matchedTypes",
            limited(types, c.limit(), |t| {
                record([
                    ("count", int(t.count)),
                    ("liveCount", int(t.live_count)),
                    ("pendingFinalizationCount", int(t.pending_count)),
                    ("type", text(&t.name)),
                ])
            }),
        ),
        ("objects", int(objects.len())),
        ("instances", limited(objects, c.limit(), |i| c.row(i, true))),
    ]))
}

pub fn object(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let heap = &s.heap;
    let graph = &s.graph;
    let a = c.address(0)?;
    let i = c.object(a)?;
    let region = &heap.regions[heap.region_of(i)];
    let mut out = record([
        ("address", address(heap.addresses[i])),
        ("type", text(s.type_name(i))),
        ("methodTable", address(heap.method_table(i).address)),
        ("size", bytes(heap.size(i))),
    ]);
    if heap.addresses[i] != a {
        out.add("offsetOfGivenAddress", int(a - heap.addresses[i]));
    }
    if let Some(n) = s.component_count(i) {
        out.add("length", int(n));
    }
    out.add("state", text(graph.state(i)));
    out.add("generation", text(&region.generation_name));
    out.add("region", address(region.start));
    if graph.live[i] != 0 {
        out.add("retained", bytes(graph.retained[i]));
        let ownership = s.ownership()?;
        let node = ownership.object_node(i);
        if ownership.is_reachable(node)
            && (ownership.retained_native[node] > 0
                || ownership.retained(node) != graph.retained[i])
        {
            out.add(
                "retainedWithNative",
                record([
                    ("managed", bytes(ownership.retained_managed[node])),
                    ("native", bytes(ownership.retained_native[node])),
                ]),
            );
        }
        let d = graph.idom[i];
        if d >= 0 {
            let d = d as usize;
            out.add(
                "dominator",
                if d < heap.count() {
                    record([
                        ("address", address(heap.addresses[d])),
                        ("type", text(s.type_name(d))),
                    ])
                } else {
                    text(s.node_name(d)?)
                },
            );
        }
    }
    if let Some(p) = s.text_preview(i, 2000) {
        let mut contents = record([("kind", text(p.kind))]);
        if let Some(t) = p.text {
            contents.add("text", text(t));
        }
        if !p.hex.is_empty() {
            contents.add("hex", list(p.hex.into_iter().take(16).map(text)));
        }
        contents.add("truncated", Value::Bool(p.truncated));
        out.add("contents", contents);
    }
    let values = s.field_values(i);
    let mut pending: Vec<&FieldValue> = values.iter().rev().collect();
    let mut fields = Vec::new();
    while let Some(f) = pending.pop() {
        let mut row = record([
            (
                "field",
                text(property_name(f.path.strip_prefix('.').unwrap_or(&f.path))),
            ),
            ("fieldType", text(f.field.type_name())),
        ]);
        if let Some(t) = f.target {
            row.add("value", address(heap.addresses[t]));
            row.add("targetType", text(s.type_name(t)));
            row.add("text", optional(s.string_value(t, 100)));
        } else {
            row.add("value", text(&f.display));
        }
        fields.push(row);
        pending.extend(f.children.iter().rev());
    }
    out.add("fields", list(fields));
    if let Some(elements) = s.struct_elements(i, c.limit()) {
        let omitted = s
            .component_count(i)
            .unwrap_or(0)
            .saturating_sub(elements.len());
        let rows = elements
            .into_iter()
            .map(|e| {
                record([
                    ("index", int(e.index)),
                    (
                        "fields",
                        list(e.fields.into_iter().map(|f| {
                            record([
                                ("field", text(property_name(&f.field.name))),
                                ("value", text(f.display)),
                            ])
                        })),
                    ),
                ])
            })
            .collect();
        out.add("elements", Value::List(rows, omitted));
    } else if let Some(elements) = s.reference_elements(i, c.limit()) {
        let omitted = s
            .component_count(i)
            .unwrap_or(0)
            .saturating_sub(elements.len());
        let rows = elements
            .into_iter()
            .map(|e| {
                record([
                    ("index", int(e.index)),
                    ("value", address(e.value)),
                    ("type", optional(e.target.map(|t| s.type_name(t)))),
                ])
            })
            .collect();
        out.add("elements", Value::List(rows, omitted));
    }
    out.add(
        "referrers",
        try_limited(graph.referrer_rows.row(i), c.limit(), |&j| {
            let j = j as usize;
            Ok(record([
                ("address", address(heap.addresses[j])),
                ("state", text(graph.state(j))),
                (
                    "field",
                    optional(s.reference_name(j, i)?.map(|n| property_name(&n))),
                ),
                ("type", text(s.type_name(j))),
            ]))
        })?,
    );
    out.add(
        "rootSources",
        try_limited(graph.sources_of.row(i), usize::MAX, |&source| {
            Ok(text(s.describe_source(source as usize)?))
        })?,
    );
    if graph.live[i] != 0 {
        let (source, path) = graph.path_to_root(i);
        let mut root = record([(
            "root",
            optional(source.map(|k| s.describe_source(k)).transpose()?),
        )]);
        if let Some(&first) = path.first() {
            root.add("rootSlot", optional(s.root_slot_description(first)?));
        }
        root.add(
            "steps",
            try_limited(path.iter().enumerate(), usize::MAX, |(k, &j)| {
                Ok(record([
                    ("address", address(heap.addresses[j])),
                    (
                        "field",
                        optional(if k > 0 {
                            s.reference_name(path[k - 1], j)?.map(|n| property_name(&n))
                        } else {
                            None
                        }),
                    ),
                    ("type", text(s.type_name(j))),
                ]))
            })?,
        );
        out.add("pathToRoot", root);
        let ownership = s.ownership()?;
        let path = ownership.path_to_root(ownership.object_node(i));
        if path
            .iter()
            .any(|&n| matches!(ownership.node(n), OwnershipNode::Chunk(_)))
        {
            out.add("heldThroughNativeMemory", c.ownership_path(&path)?);
        }
    } else {
        let garbage = s.garbage()?;
        out.add(
            "garbage",
            record([
                ("holds", bytes(garbage.retained[i])),
                ("deadReferrers", int(garbage.referrers.row(i).len())),
                (
                    "pathFromUnreferencedDeadObject",
                    list(garbage.path_from_unreferenced(i).into_iter().map(|j| {
                        record([
                            ("address", address(heap.addresses[j])),
                            ("type", text(s.type_name(j))),
                        ])
                    })),
                ),
            ]),
        );
    }
    if c.is_linux() {
        let native = s.native_graph()?;
        if let Some(owned) = native
            .owned_chunks
            .get(&i)
            .filter(|chunks| !chunks.is_empty())
        {
            out.add(
                "native",
                record([
                    (
                        "keepsAlone",
                        bytes(native.native_retained.get(&i).copied().unwrap_or(0)),
                    ),
                    (
                        "chunks",
                        try_limited(owned, c.limit(), |&k| {
                            let k = k as usize;
                            Ok(record([
                                ("address", address(native.chunks[k].user_start())),
                                ("size", bytes(native.chunks[k].size)),
                                ("field", optional(native.owner_field(i, k, s))),
                                ("label", text(native.group_label(k, s)?)),
                            ]))
                        })?,
                    ),
                ]),
            );
        }
    }
    Ok(out)
}

pub fn retained(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let ownership = s.ownership()?;
    let mut node = 0;
    let mut out = record([]);
    if !c.arguments.positional.is_empty() {
        let a = c.address(0)?;
        if let Some(i) = s.heap.object_containing(a) {
            if s.graph.live[i] == 0 {
                return Err(Error(format!(
                    "{} is dead; see dotheap object {} {}",
                    hex(s.heap.addresses[i]),
                    c.core_word(),
                    hex(s.heap.addresses[i])
                )));
            }
            node = ownership.object_node(i);
            out.add("address", address(s.heap.addresses[i]));
            out.add("type", text(s.type_name(i)));
            out.add("size", bytes(s.heap.size(i)));
        } else if c.is_linux()
            && let Some(k) = s
                .native_graph()?
                .chunk(a)
                .filter(|&k| ownership.is_reachable(ownership.chunk_node(k)))
        {
            let native = s.native_graph()?;
            node = ownership.chunk_node(k);
            out.add("address", address(native.chunks[k].user_start()));
            out.add("chunk", text(native.group_label(k, s)?));
            out.add("size", bytes(native.chunks[k].size));
        } else {
            return Err(Error(format!(
                "{} is not a live object or a malloc chunk in use: {}",
                hex(a),
                c.where_is(a)
            )));
        }
    } else {
        out.add(
            "node",
            text("roots: root sources, and what more than one root source reaches"),
        );
    }
    out.add("retained", bytes(ownership.retained(node)));
    out.add("managed", bytes(ownership.retained_managed[node]));
    out.add("native", bytes(ownership.retained_native[node]));
    out.add(
        "children",
        try_limited(s.ownership_groups(node)?.iter(), c.limit(), |g| {
            c.ownership_row(g, node)
        })?,
    );
    Ok(out)
}

pub fn handles(c: &Context<'_>) -> Result<Value> {
    Ok(record([(
        "handles",
        limited(c.snapshot.handle_groups(), c.limit(), |g| {
            record([
                ("count", int(g.count)),
                ("liveTargets", int(g.live_targets)),
                ("targetBytes", bytes(g.target_bytes)),
                ("handleType", text(g.handle_type)),
                ("targetType", text(g.target_type)),
            ])
        }),
    )]))
}
