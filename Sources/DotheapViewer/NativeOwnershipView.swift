import HeapCore
import SwiftUI

struct NativeOwnershipSection: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var reachFilter: NativeGraph.Reach?
    @State private var order: TableSort? = TableSort(column: "Bytes", ascending: false)
    @State private var selectedSize: UInt64?
    @State private var sizeOrder: TableSort? = TableSort(column: "Bytes", ascending: false)
    @State private var chunkOrder: TableSort? = TableSort(column: "Size", ascending: false)

    struct Model {
        let graph: NativeGraph
        let groups: RustArray<NativeGraph.ChunkGroup>
        let totals: [(reach: NativeGraph.Reach, chunks: Int, bytes: UInt64)]

        init(_ snapshot: HeapSnapshot) throws {
            graph = try snapshot.nativeGraph
            groups = try snapshot.chunkGroups
            totals = graph.totals()
        }
    }

    var body: some View {
        Computed(snapshot, key: "nativeOwnership", label: "Reading native allocations", compute: { try Model($0) }) { model in
            content(model)
        }
    }

    private func content(_ model: Model) -> some View {
        let groups = reachFilter == nil ? AnyRandomAccessCollection(model.groups) : AnyRandomAccessCollection(model.groups.filter { $0.reach == reachFilter })
        let group = session.selectedChunkGroup.flatMap { id in model.groups.first { $0.id == id } }
        return VStack(alignment: .leading, spacing: 8) {
            ViewThatFits(in: .horizontal) {
                reachPicker(short: false)
                reachPicker(short: true)
                Picker("Reachable from", selection: $reachFilter) {
                    Text("All").tag(NativeGraph.Reach?.none)
                    ForEach(NativeGraph.Reach.allCases, id: \.self) { Text($0.title).tag(NativeGraph.Reach?.some($0)) }
                }.pickerStyle(.menu).frame(maxWidth: 300)
            }
            HStack(alignment: .top) {
                Text("\(groups.count.formatted()) groups · \(groups.reduce(0) { $0 + $1.chunks.count }.formatted()) chunks · \(Format.bytes(groups.reduce(0) { $0 + $1.bytes }))")
                Spacer()
                Text("Sizes include malloc headers and padding.")
            }
            .font(.caption).foregroundStyle(.secondary)
            SplitPanes(axis: .vertical, firstMin: 160, secondMin: 240, initial: 0.45, showSecond: group != nil) {
                table(groups)
                    .padding(.bottom, group == nil ? 0 : 12)
            } second: {
                if let group {
                    NativeGroupChunks(snapshot: snapshot, graph: model.graph, group: group,
                                      size: $selectedSize, sizeOrder: $sizeOrder, chunkOrder: $chunkOrder)
                        .id(group.id)
                        .padding(.top, 12)
                }
            }
        }
        .onChange(of: session.selectedChunkGroup) { _, id in
            selectedSize = nil
            if let group = model.groups.first(where: { $0.id == id }), reachFilter != nil && reachFilter != group.reach {
                reachFilter = nil
            }
        }
        .onChange(of: reachFilter) { _, filter in
            if let filter, let id = session.selectedChunkGroup,
               model.groups.first(where: { $0.id == id })?.reach != filter {
                session.selectedChunkGroup = nil
            }
        }
    }

    private func reachPicker(short: Bool) -> some View {
        Picker("Reachable from", selection: $reachFilter) {
            Text("All").tag(NativeGraph.Reach?.none)
            ForEach(NativeGraph.Reach.allCases, id: \.self) { Text(short ? $0.shortTitle : $0.title).tag(NativeGraph.Reach?.some($0)) }
        }
        .labelsHidden().pickerStyle(.segmented).fixedSize()
    }

    private func table(_ groups: AnyRandomAccessCollection<NativeGraph.ChunkGroup>) -> some View {
        TextTable(rows: groups, columns: [
            .init("Type or source", width: 340, minWidth: 180, flexible: true, truncation: .byTruncatingMiddle,
                  value: \.label, text: \.displayName, help: { "\($0.label)\n\($0.kind.title)\n\($0.libraryText)\n\($0.reach.title)\n\($0.explanation)" }),
            .init("Chunks", width: 75, minWidth: 60, font: .digits, value: \.count, text: { $0.chunks.count.formatted() }),
            .init("Bytes", width: 90, minWidth: 75, font: .digits, value: \.bytes, text: { Format.bytes($0.bytes) }),
        ], sort: $order, selection: session.selectedChunkGroup, select: { session.selectedChunkGroup = $0 })
    }
}

struct NativeGroupChunks: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: NativeGraph
    let group: NativeGraph.ChunkGroup
    @Binding var size: UInt64?
    @Binding var sizeOrder: TableSort?
    @Binding var chunkOrder: TableSort?
    @State private var wide = true

    struct Row: Identifiable {
        let id: Int32
        let address: UInt64
        let size: UInt64
    }

    var body: some View {
        let selectedSize = size
        let wide = wide
        return Computed(snapshot, key: "chunk sizes \(group.id)", label: "Counting chunk sizes", compute: { _ in
            try graph.chunkSizes(group.chunks)
        }) { sizes in
            VStack(alignment: .leading, spacing: 10) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(group.label).font(.headline).lineLimit(2).textSelection(.enabled)
                    Text([group.kind.title, group.library, group.reach.title].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary).help(group.explanation)
                    Text("\(chunkCount(group.chunks.count)) · \(Format.bytes(group.bytes))").font(.caption).foregroundStyle(.secondary)
                }
                if sizes.count > 1 {
                    SplitPanes(axis: .horizontal, firstMin: 240, secondMin: 300, firstIdeal: 350) {
                        VStack(alignment: .leading, spacing: 10) {
                            HStack {
                                Text("Sizes").font(.caption.weight(.semibold))
                                Spacer()
                                Button("All sizes") { size = nil }.buttonStyle(.link).disabled(selectedSize == nil)
                            }
                            .frame(height: 20)
                            TextTable(rows: sizes, columns: [
                                .init("Size", width: 105, minWidth: 90, flexible: true, font: .digits, value: \.size, text: { "\($0.size.formatted()) B" },
                                      help: { "\(chunkCount($0.count)) · \(Format.bytes($0.bytes))" }),
                            ] + (wide || sizeOrder?.column == "Chunks" ? [
                                .init("Chunks", width: 70, minWidth: 50, font: .digits, value: \.count, text: { $0.count.formatted() }),
                            ] : []) + (wide || sizeOrder?.column != "Chunks" ? [
                                .init("Bytes", width: 90, minWidth: 65, font: .digits, value: \.bytes, text: { Format.bytes($0.bytes) }),
                            ] : []), sort: $sizeOrder, selection: selectedSize, select: { size = $0 })
                        }
                        .onGeometryChange(for: Bool.self) { $0.size.width >= 300 } action: { self.wide = $0 }
                        .padding(.trailing, 12)
                    } second: {
                        chunks(count: selectedSize.flatMap { size in sizes.first { $0.size == size }?.count } ?? group.chunks.count,
                               uniformSize: selectedSize)
                            .padding(.leading, 12)
                    }
                } else {
                    chunks(count: group.chunks.count, uniformSize: group.minSize)
                }
            }
        }
    }

    private func chunks(count: Int, uniformSize: UInt64?) -> some View {
        let selectedSize = size
        return Computed(snapshot, key: "chunk samples \(group.id) \(selectedSize.map(String.init) ?? "all")", label: "Reading chunks", compute: { _ in
            try graph.largestChunks(group.chunks, size: selectedSize, limit: 500).map { index in
                let chunk = graph.chunks[Int(index)]
                return Row(id: index, address: chunk.userStart, size: chunk.size)
            }
        }) { rows in
            VStack(alignment: .leading, spacing: 10) {
                Text(Self.title(shown: rows.count, total: count, size: uniformSize)).font(.caption).foregroundStyle(.secondary)
                    .frame(height: 20, alignment: .leading)
                TextTable(rows: rows, columns: [
                    .init("Address", width: 170, minWidth: 140, flexible: true, font: .monospaced, value: \.address, text: { Format.hex($0.address) }),
                    .init("Size", width: 90, minWidth: 75, font: .digits, value: \.size, text: { Format.bytes($0.size) }, help: { "\($0.size.formatted()) bytes" }),
                ], sort: $chunkOrder, selection: session.selectedChunk, select: { if let index = $0 { session.select(chunk: Int(index)) } })
            }
        }
    }

    static func title(shown: Int, total: Int, size: UInt64?) -> String {
        let count = chunkCount(total)
        if let size {
            return (shown < total ? "\(shown.formatted()) of \(count)" : count) + " · \(size.formatted()) bytes" + (total == 1 ? "" : " each")
        }
        return shown < total ? "Largest \(shown.formatted()) of \(count)" : count
    }
}

/// One malloc chunk: what it is, who holds it, what it points to, and its first bytes.
struct ChunkDetail: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: NativeGraph
    let chunk: Int

    struct Model {
        let details: NativeChunkInfo
        let ownership: OwnershipInspection
        let label: String
        let attribution: ChunkAttribution
        let referrers: [Reference]
        let pointers: [Reference]
        let fields: [Int: String]
        let bytes: String
    }

    struct Reference: Identifiable {
        let id: Int
        let address: UInt64
        let label: String
        let exact: Bool
    }

    var body: some View {
        Computed(snapshot, key: "native chunk \(chunk)", label: "Reading the chunk", compute: { s in
            let details = graph.details(chunk)
            var fields: [Int: String] = [:]
            for owner in details.owners.prefix(50) {
                fields[owner] = try graph.ownerField(owner, chunk: chunk, snapshot: s)
            }
            func references(_ chunks: RustArray<Int32>, incoming: Bool) throws -> [Reference] {
                try chunks.prefix(50).map { index in
                    let index = Int(index)
                    return try Reference(id: index, address: graph.chunks[index].userStart,
                                         label: graph.attribution(index).displayName,
                                         exact: graph.pointerIsExact(from: incoming ? index : chunk, to: incoming ? chunk : index))
                }
            }
            let ownership = try s.ownership
            return try Model(details: details, ownership: OwnershipInspection(s, graph: ownership, node: ownership.chunkNode(chunk)),
                             label: graph.groupLabel(chunk, snapshot: s), attribution: graph.attribution(chunk),
                             referrers: references(details.referrers, incoming: true), pointers: references(details.pointers, incoming: false),
                             fields: fields, bytes: hexDump(details.chunk))
        }) { model in
            content(model)
        }
        .id(chunk)
    }

    private func content(_ m: Model) -> some View {
        let c = m.details.chunk
        return ScrollView {
            LazyVStack(alignment: .leading, spacing: 8) {
                Text(m.label).font(.headline).textSelection(.enabled)
                Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 3) {
                    row("Address", Format.hex(c.userStart) + " (header " + Format.hex(c.start) + ")")
                    row("Size", Format.bytes(c.size))
                    row("Retained managed", Format.bytes(m.ownership.managed))
                    row("Retained native", Format.bytes(m.ownership.native))
                    row("Reachable from", m.details.reach.title)
                    if let name = m.details.className { row("C++ type", name) }
                    if m.attribution.kind != .cppType {
                        row("Attributed to", m.attribution.label)
                    }
                    row("Identified by", m.attribution.kind.title)
                    if let l = m.attribution.library ?? m.details.library { row("Library", l) }
                    row("Pointed to by", chunkCount(m.details.referrers.count) + (m.details.rootSources.isEmpty ? "" : ", \(m.details.rootSources.count) root sources"))
                    row("Points to", chunkCount(m.details.pointers.count))
                    if let dominator = m.ownership.dominator {
                        GridRow {
                            Text("Dominator").foregroundStyle(.secondary)
                            OwnershipLink(snapshot: snapshot, reference: dominator)
                        }
                    }
                }
                .font(.callout.monospacedDigit())
                if !m.details.rootSources.isEmpty {
                    DisclosureGroup("Native root sources (\(m.details.rootSources.count))") {
                        ForEach(m.details.rootSources, id: \.self) { source in
                            Text(source).font(.caption).textSelection(.enabled)
                        }
                    }
                    .font(.caption)
                }
                if c.state != .mapping {
                    Button("Show allocation group") { session.showNativeGroup(containing: chunk, in: snapshot) }
                        .buttonStyle(.link).font(.caption)
                }
                Text(m.attribution.kind.explanation).font(.caption).foregroundStyle(.secondary)
                if let source = m.attribution.sourceChunk, source != chunk {
                    Button("Inspect source at " + Format.hex(graph.chunks[source].userStart)) {
                        session.select(chunk: source)
                    }.buttonStyle(.link).font(.caption)
                }
                if let source = m.attribution.sourceObject, source != m.details.owner {
                    ObjectLink(snapshot: snapshot, object: source)
                }
                if !m.ownership.path.isEmpty {
                    Text("Path to root (shortest)").font(.caption.weight(.semibold))
                    ForEach(m.ownership.path) { reference in OwnershipLink(snapshot: snapshot, reference: reference) }
                }
                if !m.referrers.isEmpty || !m.pointers.isEmpty {
                    Text("Native pointers below are conservative address matches.").font(.caption).foregroundStyle(.secondary)
                }
                references("Native referrers", m.referrers, total: m.details.referrers.count)
                references("Native pointers", m.pointers, total: m.details.pointers.count)
                if !m.details.owners.isEmpty {
                    Text("Managed objects that point to it").font(.caption.weight(.semibold))
                    ForEach(m.details.owners.prefix(50), id: \.self) { o in
                        HStack(alignment: .firstTextBaseline, spacing: 6) {
                            ObjectLink(snapshot: snapshot, object: o)
                            if let f = m.fields[o] { Text("." + f).font(.caption.monospaced()).foregroundStyle(.secondary) }
                        }
                    }
                }
                if !m.details.managedReferences.isEmpty {
                    Text("Possible managed references").font(.caption.weight(.semibold))
                    Text("Raw address matches can be stale or unrelated. They do not establish that this chunk keeps these objects alive.")
                        .font(.caption).foregroundStyle(.secondary)
                    ForEach(Array(Set(m.details.managedReferences)).prefix(50), id: \.self) { ObjectLink(snapshot: snapshot, object: $0) }
                }
                if !m.details.handleReferences.isEmpty {
                    Text("GC handles it holds").font(.caption.weight(.semibold))
                    ForEach(Array(Set(m.details.handleReferences)).sorted(), id: \.self) { slot in
                        if let h = snapshot.graph.handles.first(where: { $0.slot == slot }) {
                            HStack(alignment: .firstTextBaseline, spacing: 6) {
                                Text((HandleTables.typeNames[h.type] ?? "type \(h.type)") + " handle " + Format.hex(slot)).font(.caption)
                                if let t = h.target { ObjectLink(snapshot: snapshot, object: t) }
                            }
                        }
                    }
                }
                Text("First bytes").font(.caption.weight(.semibold))
                ScrollView(.horizontal) {
                    Text(m.bytes).font(.caption.monospaced()).textSelection(.enabled)
                }
            }
            .padding(PageInsets.detail)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder private func references(_ title: String, _ rows: [Reference], total: Int) -> some View {
        if !rows.isEmpty {
            Text(title + (total > rows.count ? " (\(rows.count) of \(total.formatted()))" : ""))
                .font(.caption.weight(.semibold))
            ForEach(rows) { reference in
                NativeLink(snapshot: snapshot, chunk: reference.id, name: reference.label,
                           help: reference.label + "\n" + (reference.exact ? "Pointer to chunk start" : "Pointer inside chunk"))
            }
        }
    }

    private func row(_ k: String, _ v: String) -> some View {
        GridRow { Text(k).foregroundStyle(.secondary); Text(v).lineLimit(2).textSelection(.enabled) }
    }

    private func hexDump(_ c: NativeGraph.Chunk) throws -> String {
        let n = Int(min(256, c.userEnd - c.userStart))
        guard let bytes = try snapshot.dump.bytes(c.userStart, n) else { return "not in the dump" }
        return stride(from: 0, to: bytes.count, by: 16).map { o in
            let line = bytes[o..<min(o + 16, bytes.count)]
            let hex = line.map { String(format: "%02x", $0) }.joined(separator: " ")
            let text = String(line.map { (0x20...0x7e).contains($0) ? Character(UnicodeScalar($0)) : "." })
            return String(format: "+%03x  ", o) + hex.padding(toLength: 48, withPad: " ", startingAt: 0) + " " + text
        }.joined(separator: "\n")
    }
}

extension NativeGraph.Reach {
    /// The title in a tab when the full titles do not fit.
    var shortTitle: String {
        switch self {
        case .nativeRoots: "Native"
        case .liveManaged: "Live"
        case .pendingManaged: "Pending"
        case .deadManaged: "Dead"
        case .unreferenced: "Unreferenced"
        }
    }
}

private func chunkCount(_ count: Int) -> String {
    count == 1 ? "1 chunk" : "\(count.formatted()) chunks"
}

extension NativeGraph.ChunkGroup {
    var libraryText: String { library ?? "" }
    var count: Int { chunks.count }
    var displayName: String { kind.displayName(label) }
    var explanation: String {
        kind == .cppReferrer
            ? "Chunks reached from this C++ class through conservative pointers. Select a chunk to see whether the path includes interior pointers or a dominator."
            : kind.explanation
    }
}

extension ChunkAttributionKind {
    func displayName(_ name: String) -> String {
        switch self {
        case .cppReferrer, .cppInteriorReferrer, .managedReferrer: "Referenced by " + name
        case .cppDominator, .managedDominator: "Dominated by " + name
        default: name
        }
    }

    var title: String {
        switch self {
        case .cppType: "C++ type"
        case .cppDominator: "C++ dominator"
        case .cppReferrer: "C++ referrer"
        case .cppInteriorReferrer: "C++ referrer (interior pointers)"
        case .managedDominator: "Managed dominator"
        case .managedReferrer: "Managed referrer"
        case .nativeRoot: "Native root"
        case .symbol: "Symbol pointer"
        case .library: "Library pointer"
        case .text: "Text"
        case .unknown: "Unknown"
        }
    }

    var explanation: String {
        switch self {
        case .cppType: "The chunk starts with a pointer into this class's vtable."
        case .cppDominator: "All detected paths from roots to this chunk pass through this C++ object. Native pointers are scanned conservatively."
        case .cppReferrer: "A C++ object reaches this chunk through pointers to chunk starts. This does not establish ownership or the allocation site."
        case .cppInteriorReferrer: "A C++ object reaches this chunk through a path that includes interior pointers. These matches can be unrelated."
        case .managedDominator: "All detected paths from roots to this chunk pass through this managed object. Native links on the path are conservative."
        case .managedReferrer: "A managed object has a pointer field or array element that reaches this chunk. The pointer can be stale."
        case .nativeRoot: "Reachable from a stack, register, library or anonymous memory through conservative pointer matches."
        case .symbol: "The first word points into this native symbol. This does not identify the chunk's type."
        case .library: "The first word points into this library. This does not identify the chunk's type."
        case .text: "The chunk starts with readable text. Its type and allocation site are unknown."
        case .unknown: "No type or reference source was identified. Missing layouts and encoded pointers can hide references."
        }
    }
}

extension ChunkAttribution {
    var displayName: String { kind.displayName(label) }
}
