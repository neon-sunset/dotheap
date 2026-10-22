import HeapCore
import SwiftUI

/// The dominator tree of the ownership graph: each node keeps alive alone everything below it, managed objects and
/// malloc chunks. Children load when a node opens; a chain of single children opens on its own a few levels deep.
struct DominatorsView: View {
    let snapshot: HeapSnapshot
    @AppStorage("dominatorsGroupByType") private var groupByType = true

    var body: some View {
        Computed(snapshot, key: "ownership view", label: "Building the ownership graph", compute: { try $0.ownership }) { (o: OwnershipGraph) in
            VStack(alignment: .leading, spacing: 0) {
                HStack {
                    Toggle("Group children by type", isOn: $groupByType)
                    Spacer()
                }
                .padding(.horizontal, 16).padding(.top, 8)
                HStack {
                    Text("Node").font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    ForEach(["Size", "Managed", "Native"], id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary).frame(width: 90, alignment: .trailing) }
                    Text("% of live").font(.caption).foregroundStyle(.secondary).frame(width: 70, alignment: .trailing)
                }
                .padding(.horizontal, 16).padding(.vertical, 6)
                Divider()
                DominatorTree(snapshot: snapshot, graph: o, groupByType: groupByType).id(groupByType)
                Divider()
                Text("Managed and native sizes are bytes reachable only through this owner. Shared objects appear under their nearest common dominator; those shared across root sources appear directly under the root. A strong GC handle stored in a malloc chunk is attributed to that chunk, connecting native and managed ownership.")
                    .font(.caption).foregroundStyle(.secondary).padding(8)
            }
        }
    }
}

/// The dominator tree as a list of rows: each open node shows its children (grouped by type, or one row each) below
/// it. A click on a row opens or closes it. Children load when a node opens; a chain of single children opens on its
/// own a few levels deep.
struct DominatorTree: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: OwnershipGraph
    let groupByType: Bool
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
    private func children(_ item: Item) throws -> [Item] {
        switch item {
        case let .node(n):
            guard groupByType else { return graph.children(of: n).map { .node($0) } }
            return try snapshot.ownershipGroups(of: n).map { $0.nodes.count == 1 ? .node($0.nodes[0]) : .group($0) }
        case let .group(g): return g.nodes.map { .node($0) }
        case .more: return []
        }
    }

    private func id(_ item: Item, parent: String) -> String {
        switch item {
        case let .node(n): parent + "/" + String(n)
        case let .group(g): parent + "/" + g.name
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
                let list = try children(item), limit = limits[id] ?? 100
                for c in list.prefix(limit) { try walk(c, self.id(c, parent: id), depth + 1) }
                if list.count > limit { out.append(Row(id: id + "/more", depth: depth + 1, item: .more(parent: id, remaining: list.count - limit), hasChildren: false)) }
            }
            try walk(.node(graph.rootNode), "root", 0)
            return out
        }
    }

    var body: some View {
        ReadView {
            let displayed = try rows
            List(displayed) { row in
                DominatorRow(snapshot: snapshot, graph: graph, row: row, open: open.contains(row.id)) {
                    if case let .more(parent, _) = row.item { limits[parent, default: 100] += 200 } else if row.hasChildren { toggle(row.id) }
                }
                .listRowInsets(EdgeInsets(top: 1, leading: 8, bottom: 1, trailing: 8))
            }
            .listStyle(.plain)
        }
        .onAppear {
            do {
                if open.isEmpty { try openChain() }
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }

    private func toggle(_ id: String) {
        if open.contains(id) { open.remove(id) } else { open.insert(id) }
    }

    /// Opens the root and a chain of single children below it.
    private func openChain() throws {
        var item = Item.node(graph.rootNode), id = "root"
        open.insert(id)
        for _ in 0..<Self.autoOpenDepth {
            let list = try children(item)
            guard list.count == 1, case .node = list[0] else { return }
            id = self.id(list[0], parent: id)
            item = list[0]
            open.insert(id)
        }
    }
}

private struct DominatorRow: View {
    let snapshot: HeapSnapshot
    let graph: OwnershipGraph
    let row: DominatorTree.Row
    let open: Bool
    let action: () -> Void

    var body: some View {
        ReadView {
            let display = try displayValues()
            HStack(spacing: 4) {
                Color.clear.frame(width: CGFloat(row.depth) * 16, height: 1)
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    .rotationEffect(.degrees(open ? 90 : 0))
                    .frame(width: 18, height: 18)
                    .opacity(row.hasChildren ? 1 : 0)
                label(display.name, address: display.address)
                Spacer(minLength: 8)
                switch row.item {
                case let .node(n):
                    DominatorColumns(graph: graph, size: display.size, managed: graph.retainedManaged[n], native: graph.retainedNative[n])
                case let .group(g):
                    DominatorColumns(graph: graph, size: display.size, managed: g.managed, native: g.native)
                case .more: EmptyView()
                }
            }
            .contentShape(Rectangle())
            .onTapGesture(perform: action)
        }
    }

    private func displayValues() throws -> (name: String, address: UInt64?, size: UInt64?) {
        switch row.item {
        case let .node(node):
            let name = try snapshot.ownershipName(node)
            switch graph.node(node) {
            case let .object(i): return (name, nil, snapshot.heap.size(i))
            case let .chunk(c):
                let chunk = try snapshot.nativeGraph.chunks[c]
                return (name, chunk.userStart, chunk.size)
            default: return (name, nil, nil)
            }
        case let .group(group):
            return (group.name, nil, group.size)
        case .more: return ("", nil, nil)
        }
    }

    @ViewBuilder private func label(_ name: String, address: UInt64?) -> some View {
        switch row.item {
        case let .node(node):
            switch graph.node(node) {
            case let .object(i): ObjectLink(snapshot: snapshot, object: i, showSize: false)
            case .chunk:
                Label(TypeNames.short(name) + "  " + (address.map(Format.hex) ?? ""), systemImage: "memorychip")
                    .lineLimit(1).truncationMode(.middle).help(name)
            case .root: Label("roots", systemImage: "circle.hexagongrid")
            case .source: Label(name, systemImage: "arrow.up.to.line").lineLimit(1)
            }
        case let .group(group):
            Label("\(group.nodes.count.formatted()) × \(TypeNames.short(name))", systemImage: "square.stack.3d.up")
                .lineLimit(1).truncationMode(.middle).help(name)
        case let .more(_, remaining): Text("\(remaining.formatted()) more…").foregroundStyle(Color.accentColor)
        }
    }

}

/// Size, retained managed and native bytes, and the share of all live memory.
struct DominatorColumns: View {
    let graph: OwnershipGraph
    let size: UInt64?
    let managed: UInt64
    let native: UInt64

    var body: some View {
        Text(size.map(Format.bytes) ?? "").monospacedDigit().frame(width: 90, alignment: .trailing)
        Text(managed > 0 ? Format.bytes(managed) : "").monospacedDigit().frame(width: 90, alignment: .trailing)
        Text(native > 0 ? Format.bytes(native) : "").monospacedDigit().frame(width: 90, alignment: .trailing)
        Text(Format.percent(Double(managed + native), Double(graph.retained(graph.rootNode)))).monospacedDigit().foregroundStyle(.secondary).frame(width: 70, alignment: .trailing)
    }
}

/// Root sources: where the conservative scan found references into the heap.
struct RootsView: View {
    let snapshot: HeapSnapshot
    @State private var selected: Int?
    @State private var order: TableSort? = TableSort(column: "Retained only by this source", ascending: false)
    @State private var handleOrder: TableSort? = TableSort(column: "Handles", ascending: false)

    struct Row: Identifiable {
        let id: Int
        let name: String
        let objects: Int
        let retained: UInt64
    }

    var body: some View {
        let graph = snapshot.graph
        let rows: [Row] = snapshot.memo("root rows") {
            graph.sources.indices.filter { !graph.rootObjects(of: $0).isEmpty }.map { source in
                Row(id: source, name: snapshot.describe(source: source), objects: graph.rootObjects(of: source).count,
                    retained: graph.retained[snapshot.heap.count + source])
            }
        }
        return content(rows, snapshot.handleGroups())
    }

    private func content(_ rows: [Row], _ handles: RustArray<HeapSnapshot.HandleGroup>) -> some View {
        let graph = snapshot.graph
        return SplitPanes(axis: .horizontal, firstMin: 520, secondMin: 360, initial: 0.65, showSecond: selected != nil) {
            SplitPanes(axis: .vertical, firstMin: 200, secondMin: 160, initial: 0.6) {
                TextTable(rows: rows, columns: [
                    .init("Source", width: 300, flexible: true, truncation: .byTruncatingMiddle, value: \.name, text: \.name, help: { $0.name }),
                    .init("Objects referenced", width: 120, font: .digits, value: \.objects, text: { $0.objects.formatted() }),
                    .init("Retained only by this source", width: 170, font: .digits, value: \.retained, text: { Format.bytes($0.retained) }),
                ], sort: $order, selection: selected, select: { selected = $0 })
            } second: {
                VStack(alignment: .leading, spacing: 4) {
                    Text("GC handles: " + graph.handleCounts.sorted { $0.value > $1.value }.map { "\($0.key) \($0.value.formatted())" }.joined(separator: ", "))
                        .font(.caption).foregroundStyle(.secondary).padding(.horizontal, 8).padding(.top, 6)
                    TextTable(rows: handles, columns: [
                        .init("Handle type", width: 110, value: \.handleType, text: \.handleType),
                        .init("Target type", width: 260, flexible: true, truncation: .byTruncatingMiddle, value: \.targetType,
                              text: { TypeNames.short($0.targetType) }, help: { $0.targetType }),
                        .init("Handles", width: 70, font: .digits, value: \.count, text: { $0.count.formatted() }),
                        .init("Live targets", width: 80, font: .digits, value: \.liveTargets, text: { $0.liveTargets.formatted() }),
                        .init("Target bytes", width: 90, font: .digits, value: \.targetBytes, text: { Format.bytes($0.targetBytes) }),
                    ], sort: $handleOrder)
                }
            }
        } second: {
            if let s = selected {
                SampleList(snapshot: snapshot, title: snapshot.describe(source: s), samples: Array(graph.rootObjects(of: s).prefix(2000)).map(Int.init)) {
                    Format.bytes(graph.retained[$0])
                }
            }
        }
    }
}
