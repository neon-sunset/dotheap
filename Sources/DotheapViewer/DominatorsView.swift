import HeapCore
import SwiftUI

/// The dominator tree of the ownership graph: each node keeps alive alone everything below it, managed objects and
/// malloc chunks. Children load when a node opens; a chain of single children opens on its own a few levels deep.
struct DominatorsView: View {
    let snapshot: HeapSnapshot
    @AppStorage("dominatorsGroupByType") private var groupByType = true
    @State private var wide = true

    var body: some View {
        let grouped = groupByType
        let wide = wide
        return Computed(snapshot, key: "ownership view \(grouped)", label: "Building the ownership graph", compute: { snapshot in
            let graph = try snapshot.ownerTree()
            if grouped { _ = try graph.groups(of: graph.rootNode) }
            else { _ = graph.children(of: graph.rootNode) }
            return graph
        }) { (o: OwnerTree) in
            VStack(alignment: .leading, spacing: 0) {
                PageToolbar {
                    Toggle("Group by type or referrer", isOn: $groupByType)
                }
                HStack {
                    Text("Node").font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    if wide { Text("Size").font(.caption).foregroundStyle(.secondary).frame(width: 90, alignment: .trailing) }
                    Text("Retained").font(.caption).foregroundStyle(.secondary).frame(width: 230, alignment: .trailing)
                    if wide { Text("% of live").font(.caption).foregroundStyle(.secondary).frame(width: 70, alignment: .trailing) }
                }
                .padding(.horizontal, 16).padding(.vertical, 6)
                Divider()
                DominatorTree(snapshot: snapshot, graph: o, groupByType: groupByType, wide: wide).id(groupByType)
                Divider()
                Text("Retained includes managed and native memory reachable only through this node. Parentheses show the native part for managed objects, or the managed part for native allocations. Shared objects appear under their nearest common dominator; those shared across root sources appear directly under the root. Static fields are grouped by their declaring type. Thread-static fields combine threads, which can be expanded separately. A strong GC handle stored in a malloc chunk connects native and managed ownership.")
                    .font(.caption).foregroundStyle(.secondary).padding(.horizontal, PageInsets.content).padding(.vertical, 8)
            }
        }
        .onGeometryChange(for: Bool.self) { $0.size.width >= 900 } action: { self.wide = $0 }
    }
}

/// The dominator tree as a list of rows: each open node shows its children (grouped by type, or one row each) below
/// it. Clicking a row expands it; its inspect button opens the allocation.
struct DominatorTree: View {
    @Environment(Session.self) private var session
    @Environment(\.typeNameStyle) private var typeNameStyle
    let snapshot: HeapSnapshot
    let graph: OwnerTree
    let groupByType: Bool
    var root: Int? = nil
    let wide: Bool
    var compact = false
    @State private var open: Set<String> = []
    @State private var limits: [String: Int] = [:]
    static let autoOpenDepth = 5

    enum Item {
        case node(Int), group(HeapSnapshot.OwnershipGroup), more(parent: String, remaining: Int)
    }

    struct Row: Identifiable {
        let id: String
        let depth: Int
        let item: Item
        let hasChildren: Bool
    }

    /// The child items of an open row.
    private func children(_ item: Item, limit: Int) throws -> (items: [Item], count: Int) {
        switch item {
        case let .node(n):
            if groupByType {
                let groups = try graph.groups(of: n)
                return (groups.prefix(limit).map { $0.nodes.count == 1 ? .node($0.nodes[0]) : .group($0) }, groups.count)
            }
            let nodes = graph.children(of: n)
            return (nodes.prefix(limit).map { .node($0) }, nodes.count)
        case let .group(g): return (g.nodes.prefix(limit).map { .node($0) }, g.nodes.count)
        case .more: return ([], 0)
        }
    }

    private func id(_ item: Item, parent: String) -> String {
        switch item {
        case let .node(n): parent + "/" + String(n)
        case let .group(g): parent + "/group/" + String(g.nodes[0])
        case let .more(p, _): p + "/more"
        }
    }

    private var rows: [Row] {
        get throws {
            var out: [Row] = []
            func walk(_ item: Item, _ id: String, _ depth: Int) throws {
                let hasChildren: Bool
                switch item {
                case let .node(n): hasChildren = graph.hasChildren(n)
                case .group: hasChildren = true
                case .more: hasChildren = false
                }
                out.append(Row(id: id, depth: depth, item: item, hasChildren: hasChildren))
                guard hasChildren, open.contains(id) else { return }
                let limit = limits[id] ?? 100, list = try children(item, limit: limit)
                for c in list.items { try walk(c, self.id(c, parent: id), depth + 1) }
                if list.count > limit { out.append(Row(id: id + "/more", depth: depth + 1, item: .more(parent: id, remaining: list.count - limit), hasChildren: false)) }
            }
            try walk(.node(root ?? graph.rootNode), "root", 0)
            return out
        }
    }

    var body: some View {
        ReadView {
            let displayed = try rows
            DominatorTable(snapshot: snapshot, graph: graph, rows: displayed, open: open,
                           wide: wide, compact: compact, typeNameStyle: typeNameStyle,
                           inspect: inspect, action: { row in
                               if case let .more(parent, _) = row.item { limits[parent, default: 100] += 200 }
                               else if row.hasChildren { toggle(row) }
                               else if case let .node(node) = row.item {
                                   do { inspect(try graph.entry(node)) }
                                   catch { session.error = analysisErrorMessage(error) }
                               }
                           }, failed: { session.error = analysisErrorMessage($0) })
        }
        .onAppear {
            do {
                if open.isEmpty { try openChain() }
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }

    private func inspect(_ entry: OwnerEntry) {
        if let object = entry.object ?? entry.targets.first { session.select(object: object) }
        else if let chunk = entry.chunk { session.select(chunk: chunk) }
    }

    private func toggle(_ row: Row) {
        if open.contains(row.id) { open.remove(row.id); return }
        if case let .node(node) = row.item {
            let graph = graph
            let grouped = groupByType
            Task {
                do {
                    try await Task.detached {
                        if grouped { _ = try graph.groups(of: node) }
                        else { _ = graph.children(of: node) }
                    }.value
                    open.insert(row.id)
                } catch { session.error = analysisErrorMessage(error) }
            }
        } else { open.insert(row.id) }
    }

    /// Opens the root and a chain of single children below it.
    private func openChain() throws {
        var item = Item.node(root ?? graph.rootNode), id = "root"
        open.insert(id)
        for _ in 0..<Self.autoOpenDepth {
            let list = try children(item, limit: 2)
            guard list.count == 1, case .node = list.items[0] else { return }
            id = self.id(list.items[0], parent: id)
            item = list.items[0]
            open.insert(id)
        }
    }
}

/// Size, retained managed and native bytes, and the share of all live memory.
struct DominatorColumns: View {
    let total: UInt64
    let size: UInt64?
    let managed: UInt64
    let native: UInt64
    let nativeOwner: Bool
    let wide: Bool
    var compact = false

    var body: some View {
        if wide { Text(size.map(Format.bytes) ?? "").monospacedDigit().frame(width: 90, alignment: .trailing) }
        Text(Format.retained(managed: managed, native: native, nativeOwner: nativeOwner))
            .monospacedDigit().frame(width: compact ? 130 : 230, alignment: .trailing)
            .help("Managed: " + Format.bytes(managed) + "\nNative: " + Format.bytes(native))
        if wide {
            Text(Format.percent(Double(managed + native), Double(total))).monospacedDigit().foregroundStyle(.secondary).frame(width: 70, alignment: .trailing)
        }
    }
}

func ownerSymbol(_ kind: OwnerKind) -> String {
    switch kind {
    case .root: "circle.hexagongrid"
    case .staticFields, .threadStatics: "curlybraces.square"
    case .type: "curlybraces"
    case .field: "f.square"
    case .thread: "arrow.triangle.branch"
    case .storage: "internaldrive"
    default: "arrow.up.to.line"
    }
}
