use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding},
    connections::{RequestInfo, request_path},
};
use std::{collections::BTreeMap, sync::Arc};

impl HeapSnapshot {
    pub fn connections_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let network = self.network()?;
            let mut report = CheckReport::new(
                "connections",
                "Connections and requests",
                "Incoming Kestrel requests, outgoing HttpClient requests awaiting responses or \
                    connections, and socket events queued for a thread pool worker. The network \
                    command and Network page also show sockets and connection pools.",
            );
            let mut incoming = CheckSection::new(
                "Incoming requests by stage",
                "Requests that Kestrel processes now.",
                &["requests", "stage"],
            );
            let mut outgoing = CheckSection::new(
                "Outgoing requests",
                "HttpClient requests awaiting a response or a connection, grouped by host, \
                    method and path. Query strings are omitted.",
                &["requests", "host", "version", "method", "path"],
            );
            let mut by_stage: BTreeMap<String, Vec<&RequestInfo>> = BTreeMap::new();
            let mut by_request: BTreeMap<[String; 4], Vec<&RequestInfo>> = BTreeMap::new();
            let mut by_host: BTreeMap<&str, usize> = BTreeMap::new();
            for request in &network.requests {
                if request.incoming {
                    by_stage
                        .entry(request.stage.clone())
                        .or_default()
                        .push(request);
                } else {
                    let host = request.host.as_deref().unwrap_or("?");
                    by_request
                        .entry([
                            host.into(),
                            request.stage.clone(),
                            request.method.clone(),
                            request_path(&request.target).into(),
                        ])
                        .or_default()
                        .push(request);
                    if !request.stage.ends_with("waits for a connection") {
                        *by_host.entry(host).or_default() += 1;
                    }
                }
            }
            let mut by_stage: Vec<_> = by_stage.into_iter().collect();
            by_stage.sort_by(|(a, av), (b, bv)| bv.len().cmp(&av.len()).then_with(|| a.cmp(b)));
            for (stage, requests) in by_stage {
                incoming.rows.push(CheckRow::new(
                    vec![Cell::int(requests.len()), Cell::text(stage)],
                    requests.iter().take(50).map(|r| r.object).collect(),
                ));
            }
            let mut by_request: Vec<_> = by_request.into_iter().collect();
            by_request.sort_by(|(a, av), (b, bv)| bv.len().cmp(&av.len()).then_with(|| a.cmp(b)));
            for (key, requests) in by_request {
                outgoing.rows.push(CheckRow::new(
                    std::iter::once(Cell::int(requests.len()))
                        .chain(key.into_iter().map(Cell::text))
                        .collect(),
                    requests.iter().take(50).map(|r| r.object).collect(),
                ));
            }
            let mut pools = CheckSection::new(
                "Requests that wait for a connection",
                "HttpClient pools with queued requests. Connections may be opening, busy or at a \
                    configured limit such as MaxConnectionsPerServer.",
                &[
                    "queued",
                    "host",
                    "HTTP/1.1 connections",
                    "limit",
                    "HTTP/2 connections",
                    "opening",
                ],
            );
            let mut queued: Vec<_> = network.pools.iter().filter(|p| p.queued > 0).collect();
            queued.sort_by_key(|p| std::cmp::Reverse(p.queued));
            for pool in queued {
                pools.rows.push(CheckRow::new(
                    vec![
                        Cell::int(pool.queued),
                        Cell::text(&pool.host),
                        Cell::Int(pool.http11),
                        Cell::text(
                            pool.http11_limit
                                .map(|v| v.to_string())
                                .unwrap_or("none".into()),
                        ),
                        Cell::Int(pool.http2),
                        Cell::Int(pool.opening),
                    ],
                    vec![pool.object],
                ));
            }
            let mut events = CheckSection::new(
                "Socket events that wait for a thread",
                "SocketAsyncEngine completions read from epoll (kqueue on macOS) and queued for \
                    the thread pool. No worker has processed these events yet.",
                &["events", "engine", "fd", "registered sockets"],
            );
            for engine in network.engines.iter().filter(|e| e.queued_events > 0) {
                events.rows.push(CheckRow::new(
                    vec![
                        Cell::int(engine.queued_events),
                        Cell::Address(self.heap.addresses[engine.object]),
                        Cell::text(engine.fd.map(|v| v.to_string()).unwrap_or("?".into())),
                        Cell::int(engine.registered),
                    ],
                    vec![engine.object],
                ));
            }
            report.sections = vec![incoming, outgoing, pools, events];
            let in_progress = network.requests.iter().filter(|r| r.incoming).count();
            if in_progress >= 100 {
                let application = network
                    .requests
                    .iter()
                    .filter(|r| r.incoming && r.stage == "application runs")
                    .count();
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!(
                        "{in_progress} incoming requests are in progress, \
                        {application} of them in the application"
                    ),
                });
            }
            for (host, count) in by_host.into_iter().filter(|(_, count)| *count >= 50) {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("{count} outgoing requests to {host} wait for a response"),
                });
            }
            for pool in network.pools.iter().filter(|p| p.queued >= 10) {
                let limit = pool
                    .http11_limit
                    .map(|v| format!(" (limit {v} connections)"))
                    .unwrap_or_default();
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!(
                        "{} requests wait for a connection to {}{limit}",
                        pool.queued, pool.host
                    ),
                });
            }
            let queued_events: usize = network.engines.iter().map(|e| e.queued_events).sum();
            if queued_events >= 100 {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("{queued_events} socket events wait for a thread pool thread"),
                });
            }
            Ok(report)
        };
        self.saved("check connections", compute)
    }
}
