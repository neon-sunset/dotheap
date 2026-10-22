use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Owner, SOCKET_ENGINE_TYPES},
    type_names::without_namespaces,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketInfo {
    pub object: usize,
    pub fd: Option<i64>,
    pub transport: String,
    pub state: String,
    pub direction: Option<String>,
    pub local: Option<String>,
    pub remote: Option<String>,
    pub owner: String,
    pub owner_object: Option<usize>,
    pub host: Option<String>,
    pub connection_id: Option<String>,
    pub pending_receives: usize,
    pub pending_sends: usize,
    pub registered: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestInfo {
    pub object: usize,
    pub incoming: bool,
    pub method: String,
    pub target: String,
    pub stage: String,
    pub host: Option<String>,
    pub connection: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpClientPool {
    pub object: usize,
    pub host: String,
    pub kind: String,
    pub proxy: Option<String>,
    pub handler: Option<usize>,
    pub handler_owner: String,
    pub handler_owner_object: Option<usize>,
    pub http11: i64,
    pub http11_limit: Option<i64>,
    pub http2: i64,
    pub opening: i64,
    pub queued: usize,
    pub active: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketEngine {
    pub object: usize,
    pub fd: Option<i64>,
    pub registered: usize,
    pub queued_events: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Network {
    pub sockets: Vec<SocketInfo>,
    pub requests: Vec<RequestInfo>,
    pub pools: Vec<HttpClientPool>,
    pub engines: Vec<SocketEngine>,
}

#[derive(Default)]
struct PendingOperations {
    receives: usize,
    sends: usize,
    connects: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SocketEndpoints {
    pub state: String,
    pub direction: Option<String>,
    pub local: Option<String>,
    pub remote: Option<String>,
}

impl HeapSnapshot {
    fn connection_pools(
        &self,
        requests: &mut Vec<RequestInfo>,
        active: &HashMap<usize, usize>,
    ) -> Result<Vec<HttpClientPool>> {
        let kinds = [
            "http",
            "https",
            "http through a proxy",
            "WebSocket through a proxy tunnel",
            "https through a proxy tunnel",
            "proxy CONNECT",
            "http through a SOCKS proxy",
            "https through a SOCKS proxy",
        ];
        let mut handlers: HashMap<usize, Option<usize>> = HashMap::new();
        let mut owners: HashMap<usize, Option<Owner>> = HashMap::new();
        let mut pools = Vec::new();
        for object in self
            .objects(&self.type_ids(|s| s == "System.Net.Http.HttpConnectionPool"))
            .into_iter()
            .filter(|&o| self.graph.live[o] != 0)
        {
            let manager = self.target(object, "_poolManager");
            let handler = manager.and_then(|m| {
                *handlers
                    .entry(m)
                    .or_insert_with(|| self.sockets_http_handler(m))
            });
            let start = handler.or(manager).unwrap_or(object);
            if let std::collections::hash_map::Entry::Vacant(entry) = owners.entry(start) {
                entry.insert(self.holder_on_root_path(start, &SOCKET_ENGINE_TYPES, |s| {
                    s.starts_with("System.Net.Http.")
                })?);
            }
            let owner = owners.get(&start).and_then(|o| o.as_ref());
            let authority = self.target(object, "_originAuthority");
            let name = self
                .target(authority, "<IdnHost>k__BackingField")
                .and_then(|o| self.string_value(o, 200))
                .unwrap_or("?".into());
            let host = name
                + &self
                    .integer(authority, "<Port>k__BackingField")
                    .map(|p| format!(":{p}"))
                    .unwrap_or_default();
            let sent: HashSet<_> = requests.iter().map(|r| r.object).collect();
            let queue: Vec<_> = self
                .queued_requests(object)
                .into_iter()
                .filter(|(r, _)| !sent.contains(r))
                .collect();
            for &(request, version) in &queue {
                requests.push(self.outgoing_request(
                    request,
                    &format!("{version}, waits for a connection"),
                    Some(host.clone()),
                ));
            }
            let limit = self
                .integer(object, "_maxHttp11Connections")
                .unwrap_or(i32::MAX as i64);
            let kind = self
                .integer(object, "_kind")
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|v| kinds.get(v))
                .copied()
                .unwrap_or("?");
            pools.push(HttpClientPool {
                object,
                host,
                kind: kind.into(),
                proxy: self
                    .target(self.target(object, "_proxyUri"), "_string")
                    .and_then(|o| self.string_value(o, 300)),
                handler,
                handler_owner: owner
                    .map(|o| self.owner_text(o))
                    .unwrap_or("only socket engines and callbacks".into()),
                handler_owner_object: owner.and_then(|o| usize::try_from(o.object).ok()),
                http11: self
                    .integer(object, "_associatedHttp11ConnectionCount")
                    .unwrap_or(0),
                http11_limit: (limit < i32::MAX as i64).then_some(limit),
                http2: self
                    .integer(object, "_associatedHttp2ConnectionCount")
                    .unwrap_or(0),
                opening: self
                    .integer(object, "_pendingHttp11ConnectionCount")
                    .unwrap_or(0)
                    .saturating_add(self.integer(object, "_pendingHttp2Connection").unwrap_or(0)),
                queued: queue.len(),
                active: active.get(&object).copied().unwrap_or(0),
            });
        }
        Ok(pools)
    }

    pub fn network(&self) -> Result<Arc<Network>> {
        let compute = || {
            let (mut requests, active) = self.network_requests();
            let sockets = self
                .objects(&self.type_ids(|s| s == "System.Net.Sockets.Socket"))
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
                .map(|o| self.socket_info(o))
                .collect::<Result<_>>()?;
            let pools = self.connection_pools(&mut requests, &active)?;
            let mut registered = HashMap::<usize, usize>::new();
            for context in
                self.objects(&self.type_ids(|s| s == "System.Net.Sockets.SocketAsyncContext"))
            {
                if self.graph.live[context] != 0
                    && let Some(engine) = self.target(context, "_asyncEngine")
                {
                    *registered.entry(engine).or_default() += 1;
                }
            }
            let engines = self
                .objects(&self.type_ids(|s| s == "System.Net.Sockets.SocketAsyncEngine"))
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
                .map(|object| SocketEngine {
                    object,
                    fd: self.integer(object, "_port"),
                    registered: registered.get(&object).copied().unwrap_or(0),
                    queued_events: self
                        .concurrent_queue_items(self.target(object, "_eventQueue"))
                        .len(),
                })
                .collect();
            Ok(Network {
                sockets,
                requests,
                pools,
                engines,
            })
        };
        self.saved("network", compute)
    }

    fn outgoing_request(&self, object: usize, stage: &str, host: Option<String>) -> RequestInfo {
        let method = self
            .target(self.target(object, "_method"), "_method")
            .and_then(|o| self.string_value(o, 20))
            .unwrap_or("?".into());
        let target = self
            .target(self.target(object, "_requestUri"), "_string")
            .and_then(|o| self.string_value(o, 300))
            .unwrap_or_default();
        RequestInfo {
            object,
            incoming: false,
            method,
            target,
            stage: stage.into(),
            host,
            connection: None,
        }
    }

    fn network_requests(&self) -> (Vec<RequestInfo>, HashMap<usize, usize>) {
        let mut requests = Vec::new();
        let mut active = HashMap::new();
        for (name, version) in [
            ("System.Net.Http.HttpConnection", "HTTP/1.1"),
            ("System.Net.Http.Http2Connection", "HTTP/2"),
        ] {
            for connection in self
                .objects(&self.type_ids(|s| s == name))
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
            {
                let objects: Vec<_> = if version == "HTTP/1.1" {
                    self.target(connection, "_currentRequest")
                        .into_iter()
                        .collect()
                } else {
                    self.dictionary_values(self.target(connection, "_httpStreams"))
                        .into_iter()
                        .filter_map(|s| self.target(s, "_request"))
                        .collect()
                };
                for request in objects {
                    requests.push(self.outgoing_request(
                        request,
                        version,
                        self.connection_host(connection),
                    ));
                    if let Some(pool) = self.target(connection, "_pool") {
                        *active.entry(pool).or_default() += 1;
                    }
                }
            }
        }
        let stages = [
            "parsing the request line",
            "parsing headers",
            "application runs",
            "response headers committed",
            "response headers sent",
        ];
        let methods = [
            "GET", "PUT", "DELETE", "POST", "HEAD", "TRACE", "PATCH", "CONNECT", "OPTIONS",
        ];
        for object in self
            .objects(&self.types_with_field("_requestProcessingStatus"))
            .into_iter()
            .filter(|&o| self.graph.live[o] != 0)
        {
            let Some(status) = self
                .integer(object, "_requestProcessingStatus")
                .filter(|s| (1..=5).contains(s))
            else {
                continue;
            };
            let method = self
                .integer(object, "<Method>k__BackingField")
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|v| methods.get(v))
                .map(|s| (*s).into())
                .or_else(|| {
                    self.target(object, "_methodText")
                        .and_then(|o| self.string_value(o, 20))
                })
                .unwrap_or("?".into());
            let text = |name| {
                self.target(object, name)
                    .or_else(|| self.target(object, &format!("<{name}>k__BackingField")))
                    .and_then(|o| self.string_value(o, 300))
            };
            requests.push(RequestInfo {
                object,
                incoming: true,
                method,
                target: text("Path").unwrap_or_default() + &text("QueryString").unwrap_or_default(),
                stage: stages[status as usize - 1].into(),
                host: None,
                connection: text("ConnectionIdFeature"),
            });
        }
        (requests, active)
    }

    fn socket_info(&self, object: usize) -> Result<SocketInfo> {
        let mut endpoints = self.socket_endpoints(object);
        let mut owner = "nothing else".into();
        let mut owner_object = None;
        let mut host = None;
        let mut connection_id = None;
        let streams = [
            "System.Net.Sockets.NetworkStream",
            "System.Net.Security.SslStream",
            "System.Net.Sockets.TcpClient",
            "System.Net.Sockets.UdpClient",
            "System.IO.BufferedStream",
            "System.IO.StreamReader",
            "System.IO.StreamWriter",
            "System.Net.Sockets.TcpListener",
            "System.Net.Sockets.Socket+AwaitableSocketAsyncEventArgs",
        ];
        if let Some(holder) = self.nearest_holder(object, 6, |s| {
            s.ends_with(".SocketConnection")
                || s == "System.Net.Http.HttpConnection"
                || s == "System.Net.Http.Http2Connection"
        }) {
            owner_object = Some(holder);
            if self.type_name(holder).ends_with(".SocketConnection") {
                owner = "Kestrel".into();
                connection_id = self
                    .target(holder, "_connectionId")
                    .and_then(|o| self.string_value(o, 100));
            } else {
                owner = "HttpClient".into();
                host = self.connection_host(holder);
            }
        } else if let Some(holder) = self.owner(object, 500, |s| streams.contains(&s))? {
            owner = self.owner_text(&holder);
            owner_object = usize::try_from(holder.object).ok();
        }
        let handle = self.target(object, "_handle");
        let context = self.target(handle, "_asyncContext");
        let operations = self.pending_operations(context);
        let family = self.integer(object, "_addressFamily").unwrap_or(0);
        let kind = self.integer(object, "_socketType").unwrap_or(0);
        let protocol = self.integer(object, "_protocolType").unwrap_or(0);
        let transport = if family == 1 {
            if kind == 2 {
                "Unix datagram"
            } else {
                "Unix stream"
            }
        } else if protocol == 6 || kind == 1 {
            "TCP"
        } else if protocol == 17 || kind == 2 {
            "UDP"
        } else if protocol == 1 {
            "ICMP"
        } else if protocol == 58 {
            "ICMPv6"
        } else {
            "raw"
        };
        let open = self.integer(handle, "_state").unwrap_or(1) & 1 == 0;
        if endpoints.state == "not connected" && operations.connects > 0 {
            endpoints.state = "connecting".into();
        }
        Ok(SocketInfo {
            object,
            fd: if open {
                self.integer(handle, "handle")
            } else {
                None
            },
            transport: transport.into(),
            state: endpoints.state,
            direction: endpoints.direction,
            local: endpoints.local,
            remote: endpoints.remote,
            owner,
            owner_object,
            host,
            connection_id,
            pending_receives: operations.receives,
            pending_sends: operations.sends,
            registered: self.target(context, "_asyncEngine").is_some(),
        })
    }

    pub fn dictionary_values(&self, dictionary: Option<usize>) -> Vec<usize> {
        let Some(entries) = self.target(dictionary, "_entries") else {
            return Vec::new();
        };
        let Some(elements) = self.struct_elements(entries, 1 << 20) else {
            return Vec::new();
        };
        let count = self
            .integer(dictionary, "_count")
            .unwrap_or(elements.len() as i64)
            .max(0) as usize;
        elements
            .into_iter()
            .take(count)
            .filter_map(|element| {
                if self
                    .integer_field(element.fields.iter().find(|f| f.field.name == "next"))
                    .unwrap_or(0)
                    < -1
                {
                    return None;
                }
                element
                    .fields
                    .into_iter()
                    .find(|f| f.field.name == "value")?
                    .target
            })
            .collect()
    }

    fn queued_requests(&self, pool: usize) -> Vec<(usize, &'static str)> {
        let mut result = Vec::new();
        for (name, version) in [
            ("_http11RequestQueue", "HTTP/1.1"),
            ("_http2RequestQueue", "HTTP/2"),
            ("_http3RequestQueue", "HTTP/3"),
        ] {
            let Some(array) = self
                .field(pool, name)
                .and_then(|f| self.dump.u64(f.address))
                .and_then(|v| self.heap.object_at(v))
            else {
                continue;
            };
            if !self.type_name(array).contains("RequestQueue`1+QueueItem") {
                continue;
            }
            for element in self.struct_elements(array, 1 << 20).unwrap_or_default() {
                let request = element
                    .fields
                    .iter()
                    .find(|f| f.field.name == "Request")
                    .and_then(|f| f.target);
                let waiter = element
                    .fields
                    .iter()
                    .find(|f| f.field.name == "Waiter")
                    .and_then(|f| f.target);
                if let (Some(request), Some(waiter)) = (request, waiter)
                    && self
                        .integer(self.target(waiter, "_task"), "m_stateFlags")
                        .unwrap_or(0)
                        & 0x1600000
                        == 0
                {
                    result.push((request, version));
                }
            }
        }
        result
    }

    pub fn nearest_holder(
        &self,
        object: usize,
        depth: usize,
        accept: impl Fn(&str) -> bool,
    ) -> Option<usize> {
        let mut level = vec![object];
        let mut seen = HashSet::from([object]);
        for _ in 0..depth {
            let mut next = Vec::new();
            for current in level {
                for &parent in self.graph.referrer_rows.row(current) {
                    let parent = parent as usize;
                    if self.graph.live[parent] != 0 && seen.insert(parent) {
                        if accept(&self.type_name(parent)) {
                            return Some(parent);
                        }
                        next.push(parent);
                    }
                }
            }
            if next.is_empty() || next.len() > 10_000 {
                return None;
            }
            level = next;
        }
        None
    }

    fn sockets_http_handler(&self, manager: usize) -> Option<usize> {
        let mut level = vec![manager];
        let mut seen = HashSet::from([manager]);
        for _ in 0..8 {
            let mut next = Vec::new();
            for current in level {
                for &parent in self.graph.referrer_rows.row(current) {
                    let parent = parent as usize;
                    if self.graph.live[parent] == 0 || !seen.insert(parent) {
                        continue;
                    }
                    let name = self.type_name(parent);
                    if name == "System.Net.Http.SocketsHttpHandler" {
                        return Some(parent);
                    }
                    if name.starts_with("System.Net.Http.") {
                        next.push(parent);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            level = next;
        }
        None
    }

    fn pending_operations(&self, context: Option<usize>) -> PendingOperations {
        let mut result = PendingOperations::default();
        let Some(context) = context else {
            return result;
        };
        let cached: HashSet<_> = self
            .field_values(context)
            .into_iter()
            .filter(|f| f.field.name.starts_with("_cached"))
            .filter_map(|f| f.target)
            .collect();
        let mut seen = HashSet::new();
        for tail in self.graph.references(context) {
            let mut operation = Some(tail as usize);
            while let Some(current) = operation {
                if cached.contains(&current) || !seen.insert(current) {
                    break;
                }
                let name = self.type_name(current);
                if !name.starts_with("System.Net.Sockets.SocketAsyncContext+")
                    || !name.ends_with("Operation")
                {
                    break;
                }
                if name.contains("Connect") {
                    result.connects += 1;
                } else if name.contains("Send") {
                    result.sends += 1;
                } else {
                    result.receives += 1;
                }
                operation = self.target(current, "Next");
            }
        }
        result
    }

    fn connection_host(&self, connection: usize) -> Option<String> {
        let authority = self.target(self.target(connection, "_pool"), "_originAuthority");
        let name = self
            .target(authority, "<IdnHost>k__BackingField")
            .and_then(|o| self.string_value(o, 200))?;
        Some(
            name + &self
                .integer(authority, "<Port>k__BackingField")
                .map(|port| format!(":{port}"))
                .unwrap_or_default(),
        )
    }

    pub fn endpoint(&self, endpoint: Option<usize>) -> Option<String> {
        let endpoint = endpoint?;
        if let Some(path) = self
            .target(endpoint, "_path")
            .and_then(|o| self.string_value(o, 200))
        {
            return Some(if path.is_empty() {
                "unnamed Unix socket".into()
            } else {
                path
            });
        }
        let (Some(address), Some(port)) = (
            self.target(endpoint, "_address"),
            self.integer(endpoint, "_port"),
        ) else {
            return Some(without_namespaces(&self.type_name(endpoint)));
        };
        if let Some(numbers) = self.target(address, "_numbers") {
            if self.component_count(numbers) != Some(8) {
                return None;
            }
            let mut groups = [0; 8];
            for (index, group) in groups.iter_mut().enumerate() {
                *group = self
                    .dump
                    .u16(self.heap.addresses[numbers] + 16 + index as u64 * 2)?;
            }
            return Some(ipv6_endpoint(
                groups,
                self.integer(address, "_addressOrScopeId").unwrap_or(0) as u32,
                port,
            ));
        }
        let value = self.integer(address, "_addressOrScopeId")? as u32;
        Some(format!("{}:{port}", Ipv4Addr::from(value.to_le_bytes())))
    }

    pub fn socket_endpoints(&self, socket: usize) -> SocketEndpoints {
        let right = self.endpoint(self.target(socket, "_rightEndPoint"));
        let local = self.endpoint(self.target(socket, "_localEndPoint"));
        let mut remote = self.endpoint(self.target(socket, "_remoteEndPoint"));
        let disposed = self.integer(socket, "_disposed").unwrap_or(0) != 0;
        let listening = self.integer(socket, "_isListening").unwrap_or(0) != 0;
        let connected = self.integer(socket, "_isConnected").unwrap_or(0) != 0
            && self.integer(socket, "_isDisconnected").unwrap_or(0) == 0;
        if disposed || listening || !connected {
            return SocketEndpoints {
                state: if disposed {
                    "closed"
                } else if listening {
                    "listening"
                } else {
                    "not connected"
                }
                .into(),
                direction: None,
                local: local.or(right),
                remote: if listening && !disposed { None } else { remote },
            };
        }
        if remote.is_some() && right.is_some() && remote != right {
            return SocketEndpoints {
                state: "connected".into(),
                direction: Some("incoming".into()),
                local: local.or(right),
                remote,
            };
        }
        if remote.is_none() {
            remote = right.filter(|s| !is_wildcard(s));
        }
        SocketEndpoints {
            state: "connected".into(),
            direction: Some("outgoing".into()),
            local,
            remote,
        }
    }

    pub fn socket_text(&self, socket: usize) -> Option<String> {
        let endpoints = self.socket_endpoints(socket);
        match (endpoints.state.as_str(), endpoints.direction.as_deref()) {
            ("listening", _) => endpoints.local.map(|s| format!("listening on {s}")),
            (_, Some("incoming")) => {
                let mut parts = Vec::new();
                if let Some(local) = endpoints.local {
                    parts.push(format!("on {local}"));
                }
                if let Some(remote) = endpoints.remote.filter(|s| !s.starts_with("unnamed")) {
                    parts.push(format!("from {}", endpoint_address(&remote)));
                }
                Some(parts.join(" "))
            }
            (_, Some("outgoing")) => {
                if let Some(remote) = endpoints.remote {
                    let port = endpoints
                        .local
                        .as_deref()
                        .filter(|s| !s.starts_with('/'))
                        .and_then(|s| s.rsplit_once(':'))
                        .map(|(_, port)| format!(" from port {port}"))
                        .unwrap_or_default();
                    Some(format!("to {remote}{port}"))
                } else {
                    endpoints.local.map(|s| format!("on {s}"))
                }
            }
            _ => endpoints
                .remote
                .map(|s| format!("to {s}"))
                .or_else(|| endpoints.local.map(|s| format!("on {s}"))),
        }
    }
}

pub fn ipv6_endpoint(groups: [u16; 8], scope: u32, port: i64) -> String {
    let address = Ipv6Addr::from(groups);
    if let Some(v4) = address.to_ipv4_mapped() {
        return format!("{v4}:{port}");
    }
    let scope = if scope == 0 {
        String::new()
    } else {
        format!("%{scope}")
    };
    format!("[{address}{scope}]:{port}")
}

pub fn endpoint_address(endpoint: &str) -> &str {
    if endpoint.starts_with('/') || endpoint.starts_with("unnamed") {
        endpoint
    } else {
        endpoint
            .rsplit_once(':')
            .map(|(address, _)| address)
            .unwrap_or(endpoint)
    }
}

pub fn is_wildcard(endpoint: &str) -> bool {
    endpoint.starts_with("0.0.0.0:") || endpoint.starts_with("[::]:") || endpoint.ends_with(":0")
}

pub fn socket_group(socket: &SocketInfo) -> String {
    let kind = socket.direction.as_deref().unwrap_or(&socket.state);
    let peer = match kind {
        "incoming" => socket.remote.as_deref().map(|r| {
            if r.starts_with("unnamed") {
                socket.local.as_deref().unwrap_or(r)
            } else {
                endpoint_address(r)
            }
        }),
        "outgoing" => socket.host.as_deref().or(socket.remote.as_deref()),
        "closed" => socket
            .remote
            .as_deref()
            .map(endpoint_address)
            .or(socket.local.as_deref()),
        _ => socket.local.as_deref(),
    };
    [kind, &socket.transport, &socket.owner, peer.unwrap_or("-")].join("\t")
}

pub fn request_path(target: &str) -> &str {
    let no_query = target.split('?').next().unwrap_or(target);
    match no_query.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|i| &rest[i..]).unwrap_or(""),
        None => no_query,
    }
}
