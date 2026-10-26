import HeapCore
import SwiftUI

/// Network I/O that the heap shows: sockets grouped by state, owner and the other side, HTTP requests in progress,
/// HttpClient connection pools and the socket engines.
struct NetworkView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var text = ""
    @State private var groupOrder = [KeyPathComparator(\SocketGroup.open, order: .reverse), KeyPathComparator(\SocketGroup.count, order: .reverse)]
    @State private var requestOrder: TableSort? = TableSort(column: "Direction", ascending: true)
    @State private var poolOrder: TableSort? = TableSort(column: "Queued requests", ascending: false)
    @State private var ownerPoolOrder = [KeyPathComparator(\HeapSnapshot.HttpClientPool.queued, order: .reverse), KeyPathComparator(\HeapSnapshot.HttpClientPool.active, order: .reverse)]
    @State private var poolGrouping = PoolGrouping.flat
    @State private var engineOrder: TableSort? = TableSort(column: "fd", ascending: true)

    enum Mode: String, CaseIterable, Identifiable {
        case connections = "Connections", requests = "Requests", pools = "HttpClient pools"
        var id: String { rawValue }
    }

    struct SocketGroup: Identifiable {
        let id: String
        let sockets: [HeapSnapshot.SocketInfo]
        var parts: [String] { id.split(separator: "\t", omittingEmptySubsequences: false).map(String.init) }
        var count: Int { sockets.count }
        var kind: String { parts[0] }
        var transport: String { parts[1] }
        var owner: String { parts[2] }
        var peer: String { parts[3] }
        /// Open sockets sort before closed ones.
        var open: Int { kind == "closed" ? 0 : 1 }
        var receives: Int { sockets.reduce(0) { $0 + $1.pendingReceives } }
        var sends: Int { sockets.reduce(0) { $0 + $1.pendingSends } }
    }

    var body: some View {
        Computed(snapshot, key: "network page", label: "Reading the sockets", compute: { try $0.network() }) { model in
            content(model)
        }
    }

    private func content(_ n: HeapSnapshot.Network) -> some View {
        @Bindable var session = session
        let open = n.sockets.filter { $0.state != "closed" }
        func count(_ direction: String) -> Int { n.sockets.filter { $0.direction == direction && $0.state == "connected" }.count }
        let incoming = n.requests.filter(\.incoming).count
        return VStack(alignment: .leading, spacing: 10) {
            Text("Network").font(.title2.weight(.semibold))
            Text("Socket descriptors, endpoints, owners and pending reads/writes; Kestrel and HttpClient requests; connection pools; and socket engines (epoll on Linux, kqueue on macOS). A pending receive waits for peer data and can be an idle connection. Closed sockets can remain referenced by pooled objects.")
                .foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            MetricRow {
                Metric(title: "Sockets", value: n.sockets.count.formatted(), detail: "\(open.count.formatted()) open")
                Metric(title: "Incoming", value: count("incoming").formatted(), detail: "connections a listener accepted", hideZero: true)
                Metric(title: "Outgoing", value: count("outgoing").formatted(), detail: "connections the process opened", hideZero: true)
                Metric(title: "Pending", value: n.sockets.reduce(0) { $0 + $1.pendingReceives }.formatted(),
                       detail: "receives, \(n.sockets.reduce(0) { $0 + $1.pendingSends }) sends", hideZero: true)
                Metric(title: "Requests", value: n.requests.count.formatted(), detail: "\(incoming) incoming, \(n.requests.count - incoming) outgoing", hideZero: true)
                Metric(title: "Socket engines", value: n.engines.count.formatted(),
                       detail: "\(n.engines.reduce(0) { $0 + $1.registered }) sockets, \(n.engines.reduce(0) { $0 + $1.queuedEvents }) queued events",
                       color: n.engines.contains { $0.queuedEvents > 0 } ? Palette.queued : nil, hideZero: true)
            }
            HStack(spacing: 8) {
                Picker("Show", selection: $session.networkMode) { ForEach(Mode.allCases) { Text($0.rawValue).tag($0) } }
                    .labelsHidden().pickerStyle(.segmented).fixedSize()
                TextField("Filter by owner, endpoint, host or path", text: $text).textFieldStyle(.roundedBorder).frame(maxWidth: 320)
            }
            switch session.networkMode {
            case .connections: connections(n)
            case .requests: requests(n)
            case .pools: pools(n)
            }
        }
        .padding(16)
    }

    private func connections(_ n: HeapSnapshot.Network) -> some View {
        @Bindable var session = session
        let groups = Dictionary(grouping: n.sockets, by: HeapSnapshot.socketGroup).map { SocketGroup(id: $0.key, sockets: $0.value) }
            .filter { g in text.isEmpty || g.id.localizedCaseInsensitiveContains(text) || g.sockets.contains { ($0.connectionId ?? "").localizedCaseInsensitiveContains(text) } }
            .sorted { ($0.open, $0.count, $1.id) > ($1.open, $1.count, $0.id) }
            .sorted(using: groupOrder)
        return SplitPanes(axis: .horizontal, firstMin: 280, secondMin: 260, initial: 0.55) {
            Table(groups, selection: $session.networkGroup, sortOrder: $groupOrder) {
                TableColumn("Sockets", value: \SocketGroup.count) { g in Text(g.sockets.count.formatted()).monospacedDigit() }.width(min: 40, ideal: 50)
                TableColumn("Kind", value: \SocketGroup.kind) { g in Text(g.parts[0]).foregroundStyle(Self.color(g.parts[0])) }.width(min: 60, ideal: 80)
                TableColumn("Transport", value: \SocketGroup.transport) { g in Text(g.parts[1]) }.width(min: 50, ideal: 80)
                TableColumn("Owner", value: \SocketGroup.owner) { g in Text(g.parts[2]).lineLimit(2).help(g.parts[2]) }.width(min: 100, ideal: 200)
                TableColumn("Other side", value: \SocketGroup.peer) { g in Text(g.parts[3]).lineLimit(2).help(g.parts[3]) }.width(min: 100, ideal: 180)
                TableColumn("Receives", value: \SocketGroup.receives) { g in Text(g.receives.formatted()).monospacedDigit().foregroundStyle(g.receives > 0 ? .primary : .secondary) }.width(min: 40, ideal: 60)
                TableColumn("Sends", value: \SocketGroup.sends) { g in Text(g.sends.formatted()).monospacedDigit().foregroundStyle(g.sends > 0 ? .primary : .secondary) }.width(min: 40, ideal: 50)
            }
        } second: {
            if let g = groups.first(where: { $0.id == session.networkGroup }) {
                SocketList(snapshot: snapshot, group: g).id(g.id)
            } else {
                Placeholder("Select a row to see its sockets.")
            }
        }
        .onAppear { if session.networkGroup == nil { session.networkGroup = groups.first?.id } }
        .onChange(of: session.networkGroup) { _, id in
            if let id, !groups.contains(where: { $0.id == id }) { text = "" }
        }
    }

    private func requests(_ n: HeapSnapshot.Network) -> some View {
        let list = text.isEmpty ? AnyRandomAccessCollection(n.requests)
            : AnyRandomAccessCollection(n.requests.filter { r in [r.target, r.host ?? "", r.connection ?? "", r.method].contains { $0.localizedCaseInsensitiveContains(text) } })
        return Group {
            if list.isEmpty {
                Text("No HTTP request is in progress: Kestrel processes none, and no HttpClient request waits for its response.").foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                TextTable(rows: list, columns: [
                    .init("Direction", width: 70, minWidth: 60, ascending: { ($0.direction, $0.target) < ($1.direction, $1.target) }, text: \.direction),
                    .init("Method", width: 60, value: \.method, text: \.method),
                    .init("Target", width: 420, minWidth: 160, flexible: true, truncation: .byTruncatingMiddle, value: \.target, text: \.target, help: { $0.target }),
                    .init("Stage", width: 160, minWidth: 80, value: \.stage, text: \.stage),
                    .init("Host or connection", width: 180, minWidth: 80, value: \.hostOrConnection, text: \.hostOrConnection),
                ], sort: $requestOrder, selection: session.selectedObject, select: { if let o = $0 { session.select(object: o) } })
            }
        }
    }

    enum PoolGrouping: String, CaseIterable, Identifiable {
        case flat = "Flat", owner = "By owner"
        var id: String { rawValue }
    }

    private func pools(_ n: HeapSnapshot.Network) -> some View {
        let list = text.isEmpty ? AnyRandomAccessCollection(n.pools)
            : AnyRandomAccessCollection(n.pools.filter { $0.host.localizedCaseInsensitiveContains(text) || $0.handlerOwner.localizedCaseInsensitiveContains(text) })
        let select: (Int?) -> Void = { if let o = $0 { session.select(object: o) } }
        return VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Each pool serves one SocketsHttpHandler, host, connection kind and proxy. Two handlers can therefore have separate pools for the same host. Queued requests wait for a usable connection: connections may be opening, busy or at a limit such as MaxConnectionsPerServer.")
                    .font(.callout).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                Picker("Group", selection: $poolGrouping) { ForEach(PoolGrouping.allCases) { Text($0.rawValue).tag($0) } }
                    .labelsHidden().pickerStyle(.segmented).fixedSize()
            }
            switch poolGrouping {
            case .flat:
                TextTable(rows: list, columns: [
                    .init("Host", width: 220, minWidth: 120, flexible: true, truncation: .byTruncatingMiddle, value: \.host, text: \.host, help: { $0.host }),
                    .init("Kind", width: 70, minWidth: 50, value: \.kind, text: { $0.kind + ($0.proxy.map { " " + $0 } ?? "") }, help: { $0.proxy ?? "" }),
                    .init("Handler held by", width: 240, minWidth: 120, truncation: .byTruncatingMiddle, value: \.handlerOwner, text: \.handlerOwner,
                          help: { $0.handlerOwner }),
                    .init("Connections", width: 120, minWidth: 80, font: .digits, value: \.connections, text: { Self.connections($0) }),
                    .init("Active requests", width: 100, minWidth: 60, font: .digits, value: \.active, text: { $0.active.formatted() }),
                    .init("Queued requests", width: 100, minWidth: 60, font: .digits,
                          ascending: { ($0.queued, $0.active, $0.http11 + $0.http2, $1.host) < ($1.queued, $1.active, $1.http11 + $1.http2, $0.host) },
                          text: { $0.queued.formatted() }, color: { $0.queued > 0 ? NSColor(Palette.dead) : .secondaryLabelColor }),
                ], sort: $poolOrder, selection: session.selectedObject, select: select)
            case .owner:
                let handlers = Dictionary(grouping: list.sorted(using: ownerPoolOrder)) { $0.handler ?? -1 }.sorted { ($0.value.count, $1.key) > ($1.value.count, $0.key) }
                Table(of: HeapSnapshot.HttpClientPool.self, selection: Binding(get: { nil as Int? }, set: select), sortOrder: $ownerPoolOrder) {
                    TableColumn("Host", value: \HeapSnapshot.HttpClientPool.host) { p in Text(p.host).lineLimit(1).truncationMode(.middle).help(p.host) }.width(min: 120, ideal: 260)
                    TableColumn("Kind", value: \HeapSnapshot.HttpClientPool.kind) { p in Text(p.kind + (p.proxy.map { " " + $0 } ?? "")).lineLimit(1).help(p.proxy ?? "") }.width(min: 50, ideal: 70)
                    TableColumn("Connections", value: \HeapSnapshot.HttpClientPool.connections) { p in Text(Self.connections(p)).monospacedDigit() }.width(min: 80, ideal: 120)
                    TableColumn("Active requests", value: \HeapSnapshot.HttpClientPool.active) { p in Text(p.active.formatted()).monospacedDigit() }.width(min: 60, ideal: 100)
                    TableColumn("Queued requests", value: \HeapSnapshot.HttpClientPool.queued) { p in Text(p.queued.formatted()).monospacedDigit().foregroundStyle(p.queued > 0 ? Palette.dead : .secondary) }.width(min: 60, ideal: 100)
                } rows: {
                    ForEach(handlers, id: \.key) { handler, pools in
                        SwiftUI.Section(pools[0].handlerOwner + (handler >= 0 ? ", SocketsHttpHandler " + Format.hex(snapshot.heap.addresses[handler]) : "")) {
                            ForEach(pools) { TableRow($0) }
                        }
                    }
                }
            }
            Text("Socket engines").font(.headline)
            Text("Each engine has one epoll (kqueue on macOS) file descriptor and one thread that reads socket events and queues them for the thread pool.")
                .font(.callout).foregroundStyle(.secondary)
            TextTable(rows: n.engines, columns: [
                .init("Engine", width: 140, minWidth: 100, font: .monospaced, value: \.object, text: { Format.hex(snapshot.heap.addresses[$0.object]) }),
                .init("fd", width: 60, font: .digits, value: \.fdValue, text: { $0.fd.map(String.init) ?? "?" }),
                .init("Registered sockets", width: 120, minWidth: 80, flexible: true, font: .digits, value: \.registered, text: { $0.registered.formatted() }),
                .init("Queued events", width: 120, minWidth: 80, font: .digits, value: \.queuedEvents, text: { $0.queuedEvents.formatted() },
                      color: { $0.queuedEvents > 0 ? NSColor(Palette.queued) : .secondaryLabelColor }),
            ], sort: $engineOrder, selection: session.selectedObject, select: select)
            .frame(height: 110)
        }
    }

    /// "3 HTTP/1.1 (limit 10), 1 HTTP/2, 1 opening".
    static func connections(_ p: HeapSnapshot.HttpClientPool) -> String {
        [p.http11 > 0 || p.http2 == 0 ? "\(p.http11) HTTP/1.1" + (p.http11Limit.map { " (limit \($0))" } ?? "") : nil,
         p.http2 > 0 ? "\(p.http2) HTTP/2" : nil, p.opening > 0 ? "\(p.opening) opening" : nil].compactMap { $0 }.joined(separator: ", ")
    }

    static func color(_ kind: String) -> Color {
        switch kind {
        case "closed", "not connected": .secondary
        case "listening": Palette.live
        default: .primary
        }
    }
}

extension HeapSnapshot.RequestInfo: Identifiable {
    public var id: Int { object }
    var direction: String { incoming ? "incoming" : "outgoing" }
    var hostOrConnection: String { host ?? connection ?? "-" }
}
extension HeapSnapshot.HttpClientPool: Identifiable {
    public var id: Int { object }
    var limitValue: Int64 { http11Limit ?? Int64.max }
    var connections: Int64 { http11 + http2 }
}
extension HeapSnapshot.SocketEngine: Identifiable {
    public var id: Int { object }
    var fdValue: Int64 { fd ?? Int64.max }
}
extension HeapSnapshot.SocketInfo: Identifiable {
    public var id: Int { object }
    var fdValue: Int64 { fd ?? Int64.max }
    var localText: String { local ?? "-" }
    var remoteText: String { remote ?? "-" }
    var connectionText: String { connectionId ?? host ?? "" }
}

/// The sockets of one group, and the selected one with its owner and what keeps it alive.
private struct SocketList: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let group: NetworkView.SocketGroup
    @State private var selected: Int?
    @State private var order: TableSort? = TableSort(column: "fd", ascending: true)

    var body: some View {
        let sockets = group.sockets
        let shown = selected.flatMap { s in sockets.first { $0.object == s } } ?? sockets.min { $0.fdValue < $1.fdValue }
        SplitPanes(axis: .vertical, firstMin: 140, secondMin: 200, initial: 0.4, showSecond: shown != nil) {
            TextTable(rows: sockets, columns: [
                .init("fd", width: 40, minWidth: 30, font: .digits, value: \.fdValue, text: { $0.fd.map(String.init) ?? "-" }),
                .init("Local", width: 150, minWidth: 80, truncation: .byTruncatingMiddle, value: \.localText, text: \.localText),
                .init("Remote", width: 150, minWidth: 80, flexible: true, truncation: .byTruncatingMiddle, value: \.remoteText, text: \.remoteText),
                .init("Connection", width: 120, minWidth: 60, value: \.connectionText, text: \.connectionText),
                .init("Receives", width: 55, minWidth: 30, font: .digits, value: \.pendingReceives, text: { $0.pendingReceives.formatted() }),
                .init("Sends", width: 45, minWidth: 30, font: .digits, value: \.pendingSends, text: { $0.pendingSends.formatted() }),
            ], sort: $order, selection: shown?.object, select: { selected = $0 })
        } second: {
            if let s = shown { SocketDetail(snapshot: snapshot, socket: s).id(s.object) }
        }
    }
}

private struct SocketDetail: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let socket: HeapSnapshot.SocketInfo

    var body: some View {
        Computed(snapshot, key: "socket \(socket.object)", label: "Reading the socket", compute: { [object = socket.object] s in
            (text: try s.socketText(object), inTasks: try s.taskGraphs.graph(containing: object) != nil)
        }) { m in
            content(text: m.text, inTasks: m.inTasks)
        }
    }

    private func content(text: String?, inTasks: Bool) -> some View {
        let s = socket
        return ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                Text(s.state == "closed" ? "Closed socket" : text.map { "Socket " + $0 } ?? "Socket").font(.headline).textSelection(.enabled)
                Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                    GridRow { Text("Socket").foregroundStyle(.secondary); ObjectLink(snapshot: snapshot, object: s.object) }
                    row("State", [s.state, s.direction].compactMap { $0 }.joined(separator: ", ") + ", " + s.transport)
                    row("File descriptor", s.fd.map(String.init) ?? "none (the handle is closed)")
                    if let l = s.local { row("Local", l) }
                    if let r = s.remote { row("Remote", r) }
                    if let id = s.connectionId { row("Kestrel connection", id) }
                    if let h = s.host { row("HttpClient host", h) }
                    GridRow {
                        Text("Held by").foregroundStyle(.secondary)
                        HStack(spacing: 8) {
                            Text(s.owner)
                            if let o = s.ownerObject { ObjectLink(snapshot: snapshot, object: o, showSize: false) }
                        }
                    }
                    row("Pending", "\(s.pendingReceives) receives, \(s.pendingSends) sends")
                    if s.state != "closed" { row("epoll", s.registered ? "registered with a socket engine" : "not registered") }
                    if inTasks {
                        GridRow {
                            Text("Tasks").foregroundStyle(.secondary)
                            Button("Open the tasks that wait on it") { session.show(taskOf: s.object, in: snapshot) }
                        }
                    }
                }
                RootPath(snapshot: snapshot, object: s.ownerObject ?? s.object, avoiding: HeapSnapshot.socketEngineTypes, avoidingText: "the socket engines")
            }
            .padding(PageInsets.detail)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func row(_ title: String, _ value: String) -> some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }
}
