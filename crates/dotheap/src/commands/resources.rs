use super::*;
use heap_core::{connections::socket_group, symbol_server::DEFAULT_SERVER, symbols::symbol_folder};
use std::collections::BTreeMap;

fn fds(items: impl IntoIterator<Item = i64>) -> String {
    let mut items: Vec<_> = items.into_iter().collect();
    if items.is_empty() {
        return "-".into();
    }
    items.sort_unstable();
    let text = items
        .iter()
        .take(8)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    text + if items.len() > 8 { ", ..." } else { "" }
}

pub fn network(c: &Context<'_>) -> Result<Value> {
    let n = c.snapshot.network()?;
    let object = |i| address(c.snapshot.heap.addresses[i]);
    let mut groups = BTreeMap::<String, Vec<_>>::new();
    for socket in &n.sockets {
        groups.entry(socket_group(socket)).or_default().push(socket);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|(an, a), (bn, b)| {
        (!bn.starts_with("closed\t"), b.len())
            .cmp(&(!an.starts_with("closed\t"), a.len()))
            .then(an.cmp(bn))
    });
    let mut requests: Vec<_> = n.requests.iter().collect();
    requests.sort_by(|a, b| (!a.incoming, &a.target).cmp(&(!b.incoming, &b.target)));
    let mut pools: Vec<_> = n.pools.iter().collect();
    pools.sort_by(|a, b| (b.http11 + b.http2, &b.host).cmp(&(a.http11 + a.http2, &a.host)));
    Ok(record([
        (
            "sockets",
            limited(groups, c.limit(), |(key, sockets)| {
                let parts: Vec<_> = key.split('\t').collect();
                record([
                    ("sockets", int(sockets.len())),
                    (
                        "pendingReceives",
                        int(sockets.iter().map(|s| s.pending_receives).sum::<usize>()),
                    ),
                    (
                        "pendingSends",
                        int(sockets.iter().map(|s| s.pending_sends).sum::<usize>()),
                    ),
                    ("kind", text(parts[0])),
                    ("transport", text(parts[1])),
                    ("owner", text(parts[2])),
                    ("otherSide", text(parts[3])),
                    ("fds", text(fds(sockets.iter().filter_map(|s| s.fd)))),
                    ("example", object(sockets[0].object)),
                ])
            }),
        ),
        (
            "requests",
            limited(requests, c.limit(), |r| {
                record([
                    (
                        "direction",
                        text(if r.incoming { "incoming" } else { "outgoing" }),
                    ),
                    ("method", text(&r.method)),
                    ("target", text(&r.target)),
                    ("stage", text(&r.stage)),
                    (
                        "hostOrConnection",
                        text(r.host.as_deref().or(r.connection.as_deref()).unwrap_or("-")),
                    ),
                    ("object", object(r.object)),
                ])
            }),
        ),
        (
            "httpClientPools",
            limited(pools, c.limit(), |p| {
                record([
                    ("host", text(&p.host)),
                    (
                        "kind",
                        text(format!(
                            "{}{}",
                            p.kind,
                            p.proxy
                                .as_ref()
                                .map(|p| format!(" {p}"))
                                .unwrap_or_default()
                        )),
                    ),
                    ("handlerHeldBy", text(&p.handler_owner)),
                    (
                        "handler",
                        p.handler.map(object).unwrap_or_else(|| text("-")),
                    ),
                    ("http11", int(p.http11)),
                    (
                        "limit",
                        text(
                            p.http11_limit
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "none".into()),
                        ),
                    ),
                    ("http2", int(p.http2)),
                    ("opening", int(p.opening)),
                    ("active", int(p.active)),
                    ("queued", int(p.queued)),
                    ("pool", object(p.object)),
                ])
            }),
        ),
        (
            "socketEngines",
            list(n.engines.iter().map(|e| {
                record([
                    (
                        "fd",
                        text(e.fd.map(|n| n.to_string()).unwrap_or_else(|| "?".into())),
                    ),
                    ("registeredSockets", int(e.registered)),
                    ("queuedEvents", int(e.queued_events)),
                    ("engine", object(e.object)),
                ])
            })),
        ),
    ]))
}

pub fn files(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    let f = s.files()?;
    let mut open: Vec<_> = f.open.iter().collect();
    open.sort_by(|a, b| {
        (a.fd.is_none(), a.path.as_deref().unwrap_or(""))
            .cmp(&(b.fd.is_none(), b.path.as_deref().unwrap_or("")))
    });
    let mut native = BTreeMap::<(&str, &str), Vec<_>>::new();
    for f in &f.native {
        native.entry((&f.class_name, &f.path)).or_default().push(f);
    }
    Ok(record([
        (
            "openFiles",
            limited(open, c.limit(), |f| {
                record([
                    (
                        "fd",
                        text(
                            f.fd.map(|n| n.to_string())
                                .unwrap_or_else(|| "closed".into()),
                        ),
                    ),
                    (
                        "path",
                        text(
                            f.path
                                .clone()
                                .or_else(|| {
                                    f.memory_backing.map(|n| {
                                        format!(
                                            "no file: the memory of a MemoryMappedFile ({}, memfd)",
                                            size(n.max(0) as u64)
                                        )
                                    })
                                })
                                .unwrap_or_else(|| "?".into()),
                        ),
                    ),
                    ("access", text(f.access.as_deref().unwrap_or("-"))),
                    (
                        "position",
                        text(
                            f.position
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "-".into()),
                        ),
                    ),
                    (
                        "unwritten",
                        text(
                            f.unwritten
                                .map(|n| format!("{n} of {}", f.buffer_size.unwrap_or(0)))
                                .unwrap_or_else(|| "-".into()),
                        ),
                    ),
                    (
                        "pendingOrExtraReferences",
                        text(f.pending.clone().unwrap_or_else(|| {
                            if f.extra_references > 0 {
                                f.extra_references.to_string()
                            } else {
                                "-".into()
                            }
                        })),
                    ),
                    ("heldBy", text(&f.owner)),
                    ("handle", address(s.heap.addresses[f.handle])),
                ])
            }),
        ),
        (
            "nativeObjectsWithFilePaths",
            try_limited(native, c.limit(), |((class, path), items)| {
                Ok(record([
                    ("class", text(class)),
                    ("path", text(path)),
                    ("objects", int(items.len())),
                    ("estimatedFd", text(fds(items.iter().filter_map(|f| f.fd)))),
                    (
                        "chunk",
                        address(s.native_graph()?.chunks[items[0].chunk].user_start()),
                    ),
                ]))
            })?,
        ),
        (
            "mappedFiles",
            limited(&f.mapped, c.limit(), |m| {
                record([
                    ("bytes", bytes(m.bytes)),
                    ("mappings", int(m.mappings)),
                    ("kind", text(&m.kind)),
                    ("deleted", Value::Bool(m.deleted)),
                    ("path", text(&m.path)),
                ])
            }),
        ),
        (
            "note",
            text(
                "Native paths come from strings in C++ objects; descriptors are estimated from \
                    the integer after the path. The dump has no kernel file table, so this is not a \
                    complete list of open files. Extra handle references can come from active I/O \
                    or DangerousAddRef, including mappings.",
            ),
        ),
    ]))
}

pub fn symbols(c: &Context<'_>) -> Result<Value> {
    let last = std::cell::RefCell::new(String::new());
    let results = c.snapshot.download_runtime_symbols(
        DEFAULT_SERVER,
        false,
        || false,
        |message, _| {
            if *last.borrow() != message {
                eprintln!("dotheap: {message}");
                *last.borrow_mut() = message.into();
            }
        },
    );
    Ok(record([
        (
            "libraries",
            list(results.iter().map(|r| {
                record([
                    ("library", text(&r.library)),
                    ("buildId", text(&r.build_id)),
                    ("result", text(&r.result)),
                    (
                        "file",
                        optional(r.path.as_ref().map(|p| p.to_string_lossy().into_owned())),
                    ),
                ])
            })),
        ),
        (
            "note",
            text(format!(
                "Matching .NET runtime debug files from Microsoft's symbol server are cached in \
                    {} and reused across dumps to name native frames. For Satori or another custom \
                    runtime, supply its build's debug files with --dlls.",
                symbol_folder().display()
            )),
        ),
    ]))
}
