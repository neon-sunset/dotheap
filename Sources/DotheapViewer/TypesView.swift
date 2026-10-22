import HeapCore
import SwiftUI

struct TypeRow: Identifiable {
    let stats: TypeStats
    let shortName: String
    var id: UInt32 { stats.typeId }
    var name: String { shortName }
    var count: Int { stats.count }
    var liveBytes: UInt64 { stats.liveBytes }
    var pendingBytes: UInt64 { stats.pendingBytes }
    var deadBytes: UInt64 { stats.deadBytes }
    var retained: UInt64 { stats.retained }
    var liveCount: Int { stats.liveCount }
    var gen2Share: Double { stats.bytes > 0 ? Double(stats.gen2Bytes) / Double(stats.bytes) : 0 }
}

struct TypesView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var sort: TableSort? = TableSort(column: "Retained", ascending: false)
    @State private var filter = ""

    var body: some View {
        let all: [TypeRow] = snapshot.memo("type rows") { snapshot.typeStats.map { TypeRow(stats: $0, shortName: TypeNames.short($0.name)) } }
        let rows = filter.isEmpty ? all : all.filter { $0.stats.name.localizedCaseInsensitiveContains(filter) }
        SplitPanes(axis: .vertical, firstMin: 220, secondMin: 280, initial: 0.5, showSecond: session.selectedType != nil) {
            VStack(alignment: .leading, spacing: 8) {
                TextField("Filter by type name", text: $filter).textFieldStyle(.roundedBorder)
                    .frame(maxWidth: 320).padding([.horizontal, .top], 10)
                TextTable(rows: rows, columns: [
                    .init("Type", width: 380, minWidth: 220, flexible: true, truncation: .byTruncatingMiddle, value: \.name, text: \.name, help: { $0.stats.name }),
                    .init("Objects", width: 80, font: .digits, value: \.count, text: { $0.count.formatted() }),
                    .init("Live", width: 70, font: .digits, value: \.liveCount, text: { $0.liveCount.formatted() }),
                    .init("Live bytes", width: 85, font: .digits, value: \.liveBytes, text: { Format.bytes($0.liveBytes) }),
                    .init("Pending finalization", width: 120, font: .digits, value: \.pendingBytes, text: { Format.bytes($0.pendingBytes) },
                          color: { $0.pendingBytes > 0 ? NSColor(Palette.pending) : .secondaryLabelColor }),
                    .init("Dead bytes", width: 85, font: .digits, value: \.deadBytes, text: { Format.bytes($0.deadBytes) }),
                    .init("Retained", width: 85, font: .digits, value: \.retained, text: { Format.bytes($0.retained) }),
                    .init("In gen2", width: 60, font: .digits, value: \.gen2Share, text: { $0.gen2Share.formatted(.percent.precision(.fractionLength(0))) }),
                ], sort: $sort, selection: session.selectedType, select: { session.selectedType = $0 })
            }
        } second: {
            if let t = session.selectedType {
                TypeDetailView(snapshot: snapshot, typeId: t).id(t)
            }
        }
        .onChange(of: session.selectedType) { _, type in
            if let type, !rows.contains(where: { $0.id == type }) { filter = "" }
        }
    }
}

struct TypeDetailView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let typeId: UInt32
    @State private var instances: RustArray<Int> = RustArray()
    @State private var referrers: RustArray<HeapSnapshot.TypeGroup> = RustArray()
    @State private var dominators: RustArray<HeapSnapshot.TypeGroup> = RustArray()

    var body: some View {
        let stats = snapshot.typeStats[Int(typeId)]
        SplitPanes(axis: .horizontal, firstMin: 380, secondMin: 380) {
            VStack(alignment: .leading, spacing: 6) {
                Text(stats.name).font(.headline).textSelection(.enabled).lineLimit(3)
                Text("\(stats.count.formatted()) objects, \(Format.bytes(stats.bytes)); retained \(Format.bytes(stats.retained))")
                    .font(.caption).foregroundStyle(.secondary)
                Text("Largest instances").font(.subheadline.weight(.semibold)).padding(.top, 4)
                List(instances, id: \.self) { i in
                    HStack {
                        ObjectLink(snapshot: snapshot, object: i, showSize: false, showType: false)
                        Spacer()
                        Text(Format.bytes(snapshot.heap.size(i))).monospacedDigit()
                        Text(Format.bytes(snapshot.graph.retained[i])).monospacedDigit().foregroundStyle(.secondary).frame(width: 80, alignment: .trailing)
                    }
                }
            }
            .padding(10)
        } second: {
            VStack(alignment: .leading, spacing: 6) {
                Text("Referenced by (live instances, grouped by referrer type)").font(.subheadline.weight(.semibold))
                GroupList(snapshot: snapshot, groups: referrers, valueLabel: "instance bytes")
                Text("Immediate dominators (grouped by type)").font(.subheadline.weight(.semibold)).padding(.top, 6)
                GroupList(snapshot: snapshot, groups: dominators, valueLabel: "retained")
            }
            .padding(10)
        }
        .task(id: typeId) {
            let s = snapshot, t = typeId
            do {
                let result = try await Task.detached {
                    try s.memo("type detail \(t)") { (s.instances(ofType: t, limit: 500), try s.referrerTypes(ofType: t), try s.dominatorTypes(ofType: t)) }
                }.value
                instances = result.0; referrers = result.1; dominators = result.2
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }
}

/// Groups of objects by type; each group opens into its objects and can switch the Types view to that type.
struct GroupList: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let groups: RustArray<HeapSnapshot.TypeGroup>
    let valueLabel: String

    var body: some View {
        List(groups.prefix(200)) { g in
            DisclosureGroup {
                ForEach(g.samples.prefix(200), id: \.self) { ObjectLink(snapshot: snapshot, object: $0) }
                if g.samples.isEmpty { Text("No objects to show").foregroundStyle(.secondary) }
            } label: {
                HStack {
                    Text(TypeNames.short(g.name)).lineLimit(1).truncationMode(.middle).help(g.name)
                    Spacer()
                    if let typeId = snapshot.typeID(named: g.name) {
                        Button { session.selectedType = typeId } label: { Image(systemName: "arrow.right.circle") }
                            .buttonStyle(.borderless).help("Show this type")
                    }
                    Text("\(g.objects.formatted()) objects").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                    Text(Format.bytes(g.bytes)).monospacedDigit().frame(width: 80, alignment: .trailing).help(valueLabel)
                }
            }
        }
    }
}
