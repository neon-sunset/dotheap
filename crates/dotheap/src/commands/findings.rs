use super::*;
use heap_core::{
    checks::{Cell, CheckReport, CheckRow},
    search::SearchTarget,
};

fn key(name: &str) -> String {
    name.split_whitespace()
        .enumerate()
        .map(|(index, word)| {
            if index == 0 {
                word.to_lowercase()
            } else {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    })
                    .unwrap_or_default()
            }
        })
        .collect::<String>()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub fn report(report: &CheckReport, c: &Context<'_>) -> Value {
    let mut out = record([
        ("title", text(&report.title)),
        ("explanation", text(&report.explanation)),
    ]);
    if !report.findings.is_empty() {
        out.add(
            "findings",
            list(
                report
                    .findings
                    .iter()
                    .map(|f| text(format!("{}; see {}", f.text, report.name))),
            ),
        );
    }
    for section in &report.sections {
        let tree = section.rows.iter().any(|r| !r.children.is_empty());
        let tree_column = section
            .rows
            .first()
            .and_then(|r| r.cells.iter().position(|c| matches!(c, Cell::Text(_))))
            .unwrap_or(section.columns.len().saturating_sub(1));
        fn row_value(
            row: &CheckRow,
            columns: &[String],
            tree: bool,
            tree_column: usize,
            c: &Context<'_>,
        ) -> Value {
            let mut out = record([]);
            for (index, name) in columns.iter().enumerate() {
                if tree
                    && index == tree_column
                    && let Some(&first) = row.samples.first()
                {
                    out.add("example", address(c.snapshot.heap.addresses[first]));
                }
                let value = match row.cells.get(index) {
                    Some(Cell::Text(s)) => text(s),
                    Some(Cell::Int(n)) => int(*n),
                    Some(Cell::Bytes(n)) => bytes(*n),
                    Some(Cell::Address(n)) => address(*n),
                    _ => Value::Null,
                };
                out.add(key(name), value);
            }
            if !tree
                && !columns
                    .iter()
                    .any(|c| ["array", "source"].contains(&c.as_str()))
                && let Some(&first) = row.samples.first()
            {
                out.add("example", address(c.snapshot.heap.addresses[first]));
            }
            if !row.children.is_empty() {
                out.add(
                    "children",
                    limited(&row.children, c.limit(), |r| {
                        row_value(r, columns, tree, tree_column, c)
                    }),
                );
            }
            out
        }
        out.add(
            key(&section.title),
            limited(&section.rows, c.limit(), |r| {
                row_value(r, &section.columns, tree, tree_column, c)
            }),
        );
    }
    out
}

pub fn findings(c: &Context<'_>) -> Result<Value> {
    let all = match c.snapshot.all_checks() {
        Ok(all) => all,
        Err(error) => {
            let integrity = c.snapshot.verification_check()?;
            let mut value = report(&integrity, c);
            value.add("analysisError", text(error.to_string()));
            value.add(
                "note",
                text("Other findings are unavailable because their analysis failed."),
            );
            return Ok(value);
        }
    };
    Ok(record([
        (
            "findings",
            list(all.iter().flat_map(|r| {
                r.findings
                    .iter()
                    .map(|f| text(format!("{}; see {}", f.text, r.name)))
            })),
        ),
        (
            "reports",
            list(all.iter().map(|r| {
                record([
                    ("command", text(&r.name)),
                    (
                        "rows",
                        int(r.sections.iter().map(|s| s.rows.len()).sum::<usize>()),
                    ),
                    ("title", text(&r.title)),
                ])
            })),
        ),
    ]))
}

pub fn search(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let core = c.core_word();
    let results = s.search(c.positional(0, "text")?, usize::MAX)?;
    let rows = try_limited(&results, c.limit(), |r| {
        let mut row = record([
            ("kind", text(enum_text(r.kind).to_lowercase())),
            ("match", text(&r.title)),
        ]);
        if !r.detail.is_empty() {
            row.add("detail", text(&r.detail));
        }
        let next = match &r.target {
            SearchTarget::Finding(see) => see.as_ref().and_then(|see| {
                let mut words = see.split_whitespace();
                let command = words.next()?;
                Some(
                    std::iter::once("dotheap")
                        .chain(std::iter::once(command))
                        .chain(std::iter::once(core.as_str()))
                        .chain(words)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }),
            SearchTarget::Check(name) => Some(format!("dotheap {name} {core}")),
            SearchTarget::Type(t) => Some(format!(
                "dotheap instances {core} {}",
                shell_word(
                    s.type_stats
                        .iter()
                        .find(|s| s.type_id == *t)
                        .map(|s| s.name.as_str())
                        .unwrap_or("")
                )
            )),
            SearchTarget::NativeGroup(_) => Some(format!(
                "dotheap native {core} --match {}",
                shell_word(&r.title)
            )),
            SearchTarget::Object(i) => Some(format!(
                "dotheap object {core} {}",
                hex(s.heap.addresses[*i])
            )),
            SearchTarget::Region(i) => Some(format!(
                "dotheap regions {core} {}",
                hex(s.heap.regions[*i].start)
            )),
            SearchTarget::Chunk(i) => Some(format!(
                "dotheap chunk {core} {}",
                hex(s.native_graph()?.chunks[*i].user_start())
            )),
        };
        if let Some(next) = next {
            row.add("next", text(next));
        }
        Ok(row)
    })?;
    Ok(record([("results", rows)]))
}

pub fn heap_check(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let example =
        |samples: &[usize]| optional_address(samples.first().map(|&i| s.heap.addresses[i]));
    Ok(match c.arguments.command.as_str() {
        "strings" => record([(
            "duplicates",
            limited(s.duplicate_strings(c.limit()), c.limit(), |d| {
                record([
                    ("wasted", bytes(d.wasted())),
                    ("copies", int(d.count)),
                    ("bytesEach", bytes(d.bytes_each)),
                    ("example", address(s.heap.addresses[d.sample])),
                    ("text", text(prefix(&d.preview.replace('\n', "\\n"), 100))),
                ])
            }),
        )]),
        "collections" => record([(
            "collections",
            limited(s.collection_capacity(), c.limit(), |e| {
                record([
                    ("instances", int(e.instances)),
                    ("empty", int(e.empty)),
                    ("slots", int(e.slots)),
                    ("used", int(e.used)),
                    ("storage", bytes(e.storage_bytes)),
                    ("unused", bytes(e.unused_bytes)),
                    ("example", example(&e.samples)),
                    ("type", text(e.kind)),
                ])
            }),
        )]),
        "finalizable" => record([
            (
                "note",
                text(
                    "liveRegistered: reachable and registered; its finalizer runs after it \
                        becomes unreachable. liveSuppressed: reachable but suppressed or in an \
                        immortal region; Dispose often calls GC.SuppressFinalize. resurrected: \
                        reachable, neither registered nor suppressed, indicating finalization has \
                        begun or completed. pendingFinalization: unreachable, awaiting \
                        finalization. dead: unreachable, no finalizer pending. \
                        finalizerCanReRegister means the finalizer calls GC.ReRegisterForFinalize; \
                        a pending object may already have been finalized, and the dump cannot show \
                        that history.",
                ),
            ),
            ("pendingFinalizationObjects", int(s.graph.pending_count)),
            (
                "types",
                limited(s.finalizable_objects(), c.limit(), |e| {
                    record([
                        ("liveRegistered", int(e.live_registered)),
                        ("liveSuppressed", int(e.live_suppressed)),
                        ("resurrected", int(e.resurrected)),
                        ("pendingFinalization", int(e.pending)),
                        (
                            "finalizerCanReRegister",
                            Value::Bool(e.finalizer_can_re_register),
                        ),
                        ("dead", int(e.dead)),
                        ("example", example(&e.samples)),
                        ("type", text(e.kind)),
                    ])
                }),
            ),
        ]),
        "zero-arrays" => record([(
            "arrays",
            limited(s.mostly_zero_arrays(), c.limit(), |e| {
                record([
                    ("arrays", int(e.arrays)),
                    ("bytes", bytes(e.bytes)),
                    ("zero", bytes(e.zero_bytes)),
                    ("example", example(&e.samples)),
                    ("type", text(e.kind)),
                ])
            }),
        )]),
        "boxed" => record([(
            "boxed",
            limited(s.boxed_values(), c.limit(), |e| {
                record([
                    ("live", int(e.live)),
                    ("dead", int(e.dead)),
                    ("bytes", bytes(e.bytes)),
                    ("type", text(e.kind)),
                ])
            }),
        )]),
        _ => unreachable!(),
    })
}
