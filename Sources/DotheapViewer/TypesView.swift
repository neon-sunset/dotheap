import HeapCore
import SwiftUI

struct TypeRow: Identifiable, Sendable {
    let stats: TypeStats
    let retained: UInt64
    var id: UInt32 { stats.typeId }
    let name: String
    let shortName: String
    var count: Int { stats.count }
    var liveBytes: UInt64 { stats.liveBytes }
    var pendingBytes: UInt64 { stats.pendingBytes }
    var deadBytes: UInt64 { stats.deadBytes }
    var liveCount: Int { stats.liveCount }
    var gen2Share: Double { stats.bytes > 0 ? Double(stats.gen2Bytes) / Double(stats.bytes) : 0 }
}

struct TypesView: View {
    @Environment(Session.self) private var session
    @Environment(\.typeNameStyle) private var typeNameStyle
    let snapshot: HeapSnapshot
    @State private var sort: TableSort? = TableSort(column: "Retained", ascending: false)
    @State private var filter = ""

    struct Model: Sendable {
        let graph: OwnershipGraph
        let rows: [TypeRow]
        let retained: RustArray<UInt64>

        init(_ snapshot: HeapSnapshot) throws {
            graph = try snapshot.ownership
            retained = try snapshot.typeRetained
            let retained = retained
            rows = snapshot.typeStats.map { TypeRow(stats: $0, retained: retained[Int($0.typeId)], name: $0.name, shortName: TypeNames.short($0.name)) }
        }
    }

    var body: some View {
        Computed(snapshot, key: "types with native retention", label: "Reading type retention", compute: { try Model($0) }) { model in
            let rows = filter.isEmpty ? model.rows : model.rows.filter { $0.name.localizedCaseInsensitiveContains(filter) }
            SplitPanes(axis: .vertical, firstMin: 220, secondMin: 280, initial: 0.5, showSecond: session.selectedType != nil) {
                VStack(spacing: 0) {
                    PageToolbar {
                        TextField("Filter by type name", text: $filter).textFieldStyle(.roundedBorder).frame(maxWidth: 320)
                    }
                    TextTable(rows: rows, columns: [
                        .init("Type", width: 380, minWidth: 220, flexible: true, truncation: .byTruncatingMiddle, value: \.shortName,
                              text: \.name, help: \.name, typeNameStyle: typeNameStyle),
                        .init("Objects", width: 80, font: .digits, value: \.count, text: { $0.count.formatted() }),
                        .init("Live", width: 70, font: .digits, value: \.liveCount, text: { $0.liveCount.formatted() }),
                        .init("Live bytes", width: 85, font: .digits, value: \.liveBytes, text: { Format.bytes($0.liveBytes) }),
                        .init("Pending finalization", width: 120, font: .digits, value: \.pendingBytes, text: { Format.bytes($0.pendingBytes) },
                              color: { $0.pendingBytes > 0 ? NSColor(Palette.pending) : .secondaryLabelColor }),
                        .init("Dead bytes", width: 85, font: .digits, value: \.deadBytes, text: { Format.bytes($0.deadBytes) }),
                        .init("Retained", width: 85, font: .digits, value: \.retained, text: { Format.bytes($0.retained) }),
                        .init("In gen2", width: 60, font: .digits, value: \.gen2Share, text: { $0.gen2Share.formatted(.percent.precision(.fractionLength(0))) }),
                    ], sort: $sort, selection: session.selectedType, select: { session.selectedType = $0 })
                    .padding(PageInsets.content)
                }
            } second: {
                if let t = session.selectedType {
                    TypeDetailView(snapshot: snapshot, graph: model.graph, typeId: t, retained: model.retained[Int(t)]).id(t)
                }
            }
            .onChange(of: session.selectedType) { _, type in
                if let type, !rows.contains(where: { $0.id == type }) { filter = "" }
            }
        }
    }
}

struct TypeDetailView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: OwnershipGraph
    let typeId: UInt32
    let retained: UInt64
    @State private var instances: RustArray<Int> = RustArray()
    @State private var referrers: RustArray<HeapSnapshot.TypeGroup> = RustArray()
    @State private var dominators: RustArray<HeapSnapshot.OwnershipTypeGroup> = RustArray()

    var body: some View {
        let stats = snapshot.typeStats[Int(typeId)]
        SplitPanes(axis: .horizontal, firstMin: 380, secondMin: 380) {
            VStack(alignment: .leading, spacing: 6) {
                Text(stats.name).font(.headline).textSelection(.enabled).lineLimit(3)
                Text("\(stats.count.formatted()) objects, \(Format.bytes(stats.bytes)); retained \(Format.bytes(retained))")
                    .font(.caption).foregroundStyle(.secondary)
                Text("Largest instances").font(.subheadline.weight(.semibold)).padding(.top, 4)
                List(instances, id: \.self) { i in
                    HStack {
                        ObjectLink(snapshot: snapshot, object: i, showSize: false, showType: false)
                        Spacer()
                        Text(Format.bytes(snapshot.heap.size(i))).monospacedDigit()
                        Text(Format.bytes(graph.retained(graph.objectNode(i)))).monospacedDigit().foregroundStyle(.secondary).frame(width: 80, alignment: .trailing)
                    }
                }
            }
            .padding(PageInsets.detail)
        } second: {
            VStack(alignment: .leading, spacing: 6) {
                Text("Referenced by (live instances, grouped by referrer type)").font(.subheadline.weight(.semibold))
                GroupList(snapshot: snapshot, groups: referrers, valueLabel: "instance bytes")
                Text("Immediate dominators (grouped by type)").font(.subheadline.weight(.semibold)).padding(.top, 6)
                List(dominators.indices, id: \.self) { index in
                    let group = dominators[index]
                    DisclosureGroup {
                        ForEach(group.nodes, id: \.self) { node in
                            ReadView {
                                OwnershipLink(snapshot: snapshot, reference: try OwnershipReference(snapshot, graph: graph, index: node))
                            }
                        }
                    } label: {
                        TypeGroupLabel(name: group.name, typeId: group.typeId, objects: group.objects, bytes: group.bytes, valueLabel: "retained")
                    }
                }
            }
            .padding(PageInsets.detail)
        }
        .task(id: typeId) {
            let s = snapshot, t = typeId
            do {
                let result = try await Task.detached {
                    try s.memo("type detail with native retention \(t)") {
                        (try s.ownershipInstances(ofType: t, limit: 500), try s.referrerTypes(ofType: t), try s.ownershipDominatorTypes(ofType: t))
                    }
                }.value
                instances = result.0; referrers = result.1; dominators = result.2
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }
}

struct GroupList: View {
    let snapshot: HeapSnapshot
    let groups: RustArray<HeapSnapshot.TypeGroup>
    let valueLabel: String

    var body: some View {
        List(groups.prefix(200)) { g in
            DisclosureGroup {
                ForEach(g.samples.prefix(200), id: \.self) { ObjectLink(snapshot: snapshot, object: $0) }
                if g.samples.isEmpty { Text("No objects to show").foregroundStyle(.secondary) }
            } label: {
                TypeGroupLabel(name: g.name, typeId: snapshot.typeID(named: g.name), objects: g.objects, bytes: g.bytes, valueLabel: valueLabel)
            }
        }
    }
}

private struct TypeGroupLabel: View {
    @Environment(Session.self) private var session
    @Environment(\.typeNameStyle) private var typeNameStyle
    let name: String
    let typeId: UInt32?
    let objects: Int
    let bytes: UInt64
    let valueLabel: String

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            if typeId != nil {
                TypeNameText(name: name, style: typeNameStyle)
            } else {
                Text(name).lineLimit(1).truncationMode(.middle).help(name)
            }
            Spacer()
            if let typeId {
                Button { session.selectedType = typeId } label: { Image(systemName: "arrow.right.circle") }
                    .buttonStyle(.borderless).help("Show this type")
            }
            Text("\(objects.formatted()) objects").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            Text(Format.bytes(bytes)).monospacedDigit().frame(width: 80, alignment: .trailing).help(valueLabel)
        }
    }
}
