import HeapCore
import SwiftUI

struct RootsView: View {
    let snapshot: HeapSnapshot
    @State private var selected: Int?
    @State private var order: TableSort? = TableSort(column: "Native bytes", ascending: false)
    @State private var handleOrder: TableSort? = TableSort(column: "Handles", ascending: false)

    var body: some View {
        Computed(snapshot, key: "ownership root sources", label: "Reading root sources", compute: { snapshot in
            let graph = try snapshot.ownership
            let tree = try snapshot.ownerTree()
            return (graph, tree, try tree.roots)
        }) { graph, tree, roots in
            SplitPanes(axis: .horizontal, firstMin: 320, secondMin: 240, initial: 0.65, showSecond: selected != nil) {
                SplitPanes(axis: .vertical, firstMin: 200, secondMin: 160, initial: 0.6) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Roots include static fields grouped by declaring type, stacks, registers, GC handles and native memory. Retained bytes are reachable only through that source; shared allocations appear under their common dominator.")
                            .font(.caption).foregroundStyle(.secondary)
                        TextTable(rows: roots, id: { $0.id }, columns: [
                            .init("Source", width: 340, minWidth: 110, flexible: true, truncation: .byTruncatingMiddle, value: \.name, text: { $0.name + ($0.threadStatic ? " (thread statics)" : "") },
                                  help: { $0.name }),
                            .init("Managed bytes", width: 105, minWidth: 65, font: .digits, value: \.managed, text: { Format.bytes($0.managed) }),
                            .init("Native bytes", width: 95, minWidth: 65, font: .digits, value: \.native, text: { Format.bytes($0.native) }),
                        ], sort: $order, selection: selected, select: { selected = $0 })
                    }
                    .padding(.bottom, PageInsets.detail)
                } second: {
                    handleTable.padding(.top, PageInsets.detail)
                }
                .padding(.trailing, selected == nil ? 0 : PageInsets.detail)
            } second: {
                if let selected, let source = roots.first(where: { $0.id == selected }) {
                    VStack(alignment: .leading, spacing: 8) {
                        if source.kind == .source, let original = graph.roots.first(where: { $0.node == selected }) {
                            RootReferences(snapshot: snapshot, graph: graph, source: original)
                        } else {
                            Text(source.name).font(.headline).textSelection(.enabled)
                            DominatorTree(snapshot: snapshot, graph: tree, groupByType: true, root: selected, wide: false, compact: true)
                        }
                    }
                    .id(selected).padding(.leading, PageInsets.detail)
                }
            }
            .padding(PageInsets.content)
        }
    }

    private var handleTable: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("GC handles: " + snapshot.graph.handleCounts.sorted { $0.value > $1.value }.map { "\($0.key) \($0.value.formatted())" }.joined(separator: ", "))
                .font(.caption).foregroundStyle(.secondary)
            TextTable(rows: snapshot.handleGroups(), columns: [
                .init("Handle type", width: 110, value: \.handleType, text: \.handleType),
                .init("Target type", width: 260, flexible: true, truncation: .byTruncatingMiddle, value: \.targetType,
                      text: { TypeNames.short($0.targetType) }, help: { $0.targetType }),
                .init("Handles", width: 70, font: .digits, value: \.count, text: { $0.count.formatted() }),
                .init("Live targets", width: 80, font: .digits, value: \.liveTargets, text: { $0.liveTargets.formatted() }),
                .init("Target bytes", width: 90, font: .digits, value: \.targetBytes, text: { Format.bytes($0.targetBytes) }),
            ], sort: $handleOrder)
        }
    }
}

struct RootReferences: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: OwnershipGraph
    let source: OwnershipRoot
    @State private var limit = 2000
    @State private var order: TableSort? = TableSort(column: "Retained", ascending: false)
    @State private var wide = true
    @State private var compact = false

    struct Row: Identifiable {
        let id: Int
        let node: OwnershipNode
        let name: String
        let address: UInt64
        let size: UInt64
        let retained: UInt64
        var kind: String { if case .object = node { "Managed" } else { "Native" } }
    }

    var body: some View {
        let limit = limit
        let wide = wide, compact = compact
        return Computed(snapshot, key: "root references \(source.node) \(limit)", label: "Reading root references", compute: { snapshot in
            let native = try snapshot.nativeGraph
            return try graph.rootReferences(source.node).prefix(limit).map { index in
                let index = Int(index), node = graph.node(index)
                let address: UInt64, size: UInt64
                switch node {
                case let .object(object): address = snapshot.heap.addresses[object]; size = snapshot.heap.size(object)
                case let .chunk(chunk): address = native.chunks[chunk].userStart; size = native.chunks[chunk].size
                default: address = 0; size = 0
                }
                let name: String
                if case let .chunk(chunk) = node {
                    let attribution = try native.attribution(chunk)
                    name = attribution.kind.displayName(attribution.label)
                } else { name = try snapshot.ownershipName(index) }
                return Row(id: index, node: node, name: name, address: address, size: size, retained: graph.retained(index))
            }
        }) { rows in
            VStack(alignment: .leading, spacing: 10) {
                Text(source.name).font(.headline).textSelection(.enabled)
                HStack {
                    Text("Managed objects: \(source.objects.formatted()) · Native allocations: \(source.chunks.formatted())").font(.caption).foregroundStyle(.secondary)
                    Spacer()
                    if rows.count < source.objects + source.chunks {
                        Button("Show more (\(rows.count.formatted()) shown)") { self.limit += 2000 }.buttonStyle(.link)
                    }
                }
                let selected = rows.first { row in
                    switch row.node {
                    case let .object(index): session.selectedObject == index
                    case let .chunk(index): session.selectedChunk == Int32(index)
                    default: false
                    }
                }?.id
                TextTable(rows: rows, columns: [
                    .init("Reference", width: 340, minWidth: compact ? 80 : 140, flexible: true, truncation: .byTruncatingMiddle,
                          value: \.name, text: { TypeNames.short($0.name) }, help: { $0.name + "\n" + $0.kind + " · " + Format.hex($0.address) }),
                ] + (wide || order?.column == "Kind" ? [
                    .init("Kind", width: 65, minWidth: 55, value: \.kind, text: \.kind),
                ] : []) + (wide || order?.column == "Address" ? [
                    .init("Address", width: 140, minWidth: 90, font: .monospaced, truncation: .byTruncatingMiddle, value: \.address, text: { Format.hex($0.address) }),
                ] : []) + (!compact || order?.column == "Size" ? [
                    .init("Size", width: 90, minWidth: 65, font: .digits, value: \.size, text: { Format.bytes($0.size) }),
                ] : []) + (!compact || order?.column == "Retained" ? [
                    .init("Retained", width: 90, minWidth: 65, font: .digits, value: \.retained, text: { Format.bytes($0.retained) }),
                ] : []), sort: $order, selection: selected, select: { id in
                    guard let row = rows.first(where: { $0.id == id }) else { return }
                    switch row.node {
                    case let .object(index): session.select(object: index)
                    case let .chunk(index): session.select(chunk: index)
                    default: break
                    }
                })
            }
        }
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: {
            self.wide = $0 >= 650
            self.compact = $0 < 360
        }
    }
}
