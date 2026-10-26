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
/// it. Disclosure buttons expand nodes; selecting an allocation opens the inspector.
struct DominatorTree: View {
    @Environment(Session.self) private var session
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
    private func children(_ item: Item) throws -> [Item] {
        switch item {
        case let .node(n):
            guard groupByType else { return graph.children(of: n).map { .node($0) } }
            return try graph.groups(of: n).map { $0.nodes.count == 1 ? .node($0.nodes[0]) : .group($0) }
        case let .group(g): return g.nodes.map { .node($0) }
        case .more: return []
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
                case let .node(n): hasChildren = !graph.children(of: n).isEmpty
                case .group: hasChildren = true
                case .more: hasChildren = false
                }
                out.append(Row(id: id, depth: depth, item: item, hasChildren: hasChildren))
                guard hasChildren, open.contains(id) else { return }
                let list = try children(item), limit = limits[id] ?? 100
                for c in list.prefix(limit) { try walk(c, self.id(c, parent: id), depth + 1) }
                if list.count > limit { out.append(Row(id: id + "/more", depth: depth + 1, item: .more(parent: id, remaining: list.count - limit), hasChildren: false)) }
            }
            try walk(.node(root ?? graph.rootNode), "root", 0)
            return out
        }
    }

    var body: some View {
        ReadView {
            let displayed = try rows
            List(displayed) { row in
                DominatorRow(snapshot: snapshot, graph: graph, row: row, open: open.contains(row.id), wide: wide, compact: compact) {
                    if case let .more(parent, _) = row.item { limits[parent, default: 100] += 200 } else if row.hasChildren { toggle(row) }
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

    private func toggle(_ row: Row) {
        if open.contains(row.id) { open.remove(row.id); return }
        if groupByType, case let .node(node) = row.item {
            let graph = graph
            Task {
                do {
                    _ = try await Task.detached { try graph.groups(of: node) }.value
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
            let list = try children(item)
            guard list.count == 1, case .node = list[0] else { return }
            id = self.id(list[0], parent: id)
            item = list[0]
            open.insert(id)
        }
    }
}

private struct DominatorRow: View {
    @Environment(\.typeNameStyle) private var typeNameStyle
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: OwnerTree
    let row: DominatorTree.Row
    let open: Bool
    var wide = true
    var compact = false
    let action: () -> Void

    var body: some View {
        ReadView {
            let display = try displayValues()
            let total = try graph.entry(graph.rootNode)
            let entry = try nodeEntry()
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Color.clear.frame(width: CGFloat(row.depth) * 16, height: 1)
                Button(action: action) {
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                        .rotationEffect(.degrees(open ? 90 : 0))
                        .frame(width: 18, height: 18)
                }
                .buttonStyle(.plain).disabled(!row.hasChildren).opacity(row.hasChildren ? 1 : 0)
                .accessibilityLabel(open ? "Collapse" : "Expand")
                label(display.name, address: display.address)
                Spacer(minLength: 8)
                switch row.item {
                case .node:
                    let entry = entry!
                    DominatorColumns(total: total.managed + total.native, size: display.size, managed: entry.managed, native: entry.native, nativeOwner: entry.chunk != nil, wide: wide, compact: compact)
                case let .group(g):
                    DominatorColumns(total: total.managed + total.native, size: display.size, managed: g.managed, native: g.native, nativeOwner: g.kind != nil, wide: wide, compact: compact)
                case .more: EmptyView()
                }
            }
            .help("Allocation size: " + (display.size.map(Format.bytes) ?? "—") + "\n" + display.name)
            .contentShape(Rectangle())
            .onTapGesture(perform: select)
        }
    }

    private func nodeEntry() throws -> OwnerEntry? {
        if case let .node(node) = row.item { return try graph.entry(node) }
        return nil
    }

    private func select() {
        do {
            if case let .node(node) = row.item {
                let entry = try graph.entry(node)
                if let object = entry.object { session.select(object: object) }
                else if let chunk = entry.chunk { session.select(chunk: chunk) }
                else { action() }
            } else { action() }
        } catch { session.error = analysisErrorMessage(error) }
    }

    private func displayValues() throws -> (name: String, address: UInt64?, size: UInt64?) {
        switch row.item {
        case let .node(node):
            let entry = try graph.entry(node)
            return (entry.name, nil, entry.object != nil || entry.chunk != nil ? entry.size : nil)
        case let .group(group):
            return (group.kind?.displayName(group.name) ?? group.name, nil, group.size)
        case .more: return ("", nil, nil)
        }
    }

    @ViewBuilder private func label(_ name: String, address: UInt64?) -> some View {
        switch row.item {
        case let .node(node):
            ReadView {
                let entry = try graph.entry(node)
                if let object = entry.object { ObjectLink(snapshot: snapshot, object: object, showSize: false) }
                else if let chunk = entry.chunk { NativeLink(snapshot: snapshot, chunk: chunk, name: name, showSize: false) }
                else {
                    HStack(spacing: 4) {
                        Image(systemName: ownerSymbol(entry.kind))
                        if entry.kind == .type { TypeNameText(name: name, style: typeNameStyle, singleLine: true) }
                        else { Text(name).lineLimit(1).truncationMode(entry.kind == .thread ? .tail : .middle) }
                        if entry.targets.count == 1 {
                            Button { session.select(object: entry.targets[0]) } label: { Image(systemName: "arrow.up.right.square") }
                                .buttonStyle(.borderless).help("Inspect field value")
                        }
                    }
                }
            }
        case let .group(group):
            let count = group.nodes.count.formatted() + (group.kind == nil ? " × " : " chunks · ")
            HStack(spacing: 4) {
                Image(systemName: "square.stack.3d.up")
                Text(count).fixedSize()
                TypeNameText(name: name, style: group.kind == nil ? typeNameStyle : .standard, singleLine: true)
            }
            .help([name, group.kind?.title, group.library].compactMap { $0 }.joined(separator: "\n"))
        case let .more(_, remaining): Text("\(remaining.formatted()) more…").foregroundStyle(Color.accentColor)
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
