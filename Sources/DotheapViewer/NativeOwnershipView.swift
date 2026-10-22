import HeapCore
import SwiftUI

/// What holds each glibc malloc chunk: native roots, live managed objects, managed objects pending finalization, dead
/// managed objects, or nothing.
struct NativeOwnershipSection: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var model: Model?
    @State private var reachFilter: NativeGraph.Reach?
    @State private var order: TableSort? = TableSort(column: "Bytes", ascending: false)
    @State private var isWide = false

    struct Model {
        let graph: NativeGraph
        let groups: RustArray<NativeGraph.ChunkGroup>
        let totals: [(reach: NativeGraph.Reach, chunks: Int, bytes: UInt64)]
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("What holds malloc memory").font(.title3.weight(.semibold))
            Text("Managed references come from pointer fields and array elements. Native memory, stacks and registers are scanned conservatively; matching values can be stale or unrelated. Freed tcache and fastbin chunks are excluded. Native roots take priority when both native and managed references exist. Missing layouts and encoded pointers can hide references; unreferenced chunks are leak candidates.")
                .font(.caption).foregroundStyle(.secondary).lineLimit(4).frame(maxWidth: .infinity, alignment: .leading)
            if let m = model {
                content(m)
            } else {
                ProgressView().frame(maxWidth: .infinity, minHeight: 80)
            }
        }
        .onGeometryChange(for: Bool.self) { $0.size.width >= 1100 } action: { isWide = $0 }
        .task {
            let s = snapshot
            do {
                model = try await Task.detached {
                    try s.memo("nativeOwnership") { () throws -> Model in
                        let g = try s.nativeGraph
                        return try Model(graph: g, groups: s.chunkGroups, totals: g.totals())
                    }
                }.value
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }

    private func color(_ r: NativeGraph.Reach) -> Color {
        switch r {
        case .nativeRoots: Palette.native("native libraries")
        case .liveManaged: Palette.live
        case .pendingManaged: Palette.pending
        case .deadManaged: Palette.dead
        case .unreferenced: Color(red: 0.90, green: 0.30, blue: 0.35)
        }
    }

    @ViewBuilder
    private func content(_ m: Model) -> some View {
        @Bindable var session = session
        MetricRow {
            ForEach(m.totals, id: \.reach) { t in
                Metric(title: t.reach.title, value: Format.bytes(t.bytes), detail: "\(t.chunks.formatted()) chunks", color: color(t.reach))
            }
            Metric(title: "Freed, cached by glibc", value: (m.graph.tcacheChunks + m.graph.fastbinChunks).formatted(), detail: "tcache and fastbin chunks")
        }
        let groups = reachFilter == nil ? AnyRandomAccessCollection(m.groups) : AnyRandomAccessCollection(m.groups.filter { $0.reach == reachFilter })
        ViewThatFits(in: .horizontal) {
            reachPicker(short: false)
            reachPicker(short: true)
        }
        .onChange(of: session.selectedChunkGroup) { _, id in
            if let group = m.groups.first(where: { $0.id == id }), reachFilter != nil && reachFilter != group.reach {
                reachFilter = nil
            }
        }
        Text("Chunks grouped by C++ vtable class, the nearest C++ referrer, or referring managed type and field. Sizes are full malloc chunk sizes, not managed object sizes. These rows account for the totals above: \(groups.count.formatted()) groups, \(groups.reduce(0) { $0 + $1.chunks.count }.formatted()) chunks, \(Format.mb(groups.reduce(0) { $0 + $1.bytes })).")
            .font(.caption).foregroundStyle(.secondary).lineLimit(3).frame(maxWidth: .infinity, alignment: .leading)
        let group = session.selectedChunkGroup.flatMap { id in m.groups.first { $0.id == id } }
        // The table, the chunks of the selected group and the selected chunk side by side when the page is wide enough;
        // else the chunks and the chunk below the table.
        if isWide {
            SplitPanes(axis: .horizontal, firstMin: 520, secondMin: session.selectedChunk != nil ? 540 : 220, initial: 0.55, showSecond: group != nil) {
                table(groups)
            } second: {
                chunks(group, m)
            }
            .frame(height: 420)
        } else {
            SplitPanes(axis: .vertical, firstMin: 160, secondMin: 240, firstIdeal: 260, showSecond: group != nil) {
                table(groups)
            } second: {
                chunks(group, m)
            }
            .frame(height: group != nil ? 640 : 420)
        }
    }

    private func reachPicker(short: Bool) -> some View {
        Picker("Show", selection: $reachFilter) {
            Text("All").tag(NativeGraph.Reach?.none)
            ForEach(NativeGraph.Reach.allCases, id: \.self) { Text(short ? $0.shortTitle : $0.title).tag(NativeGraph.Reach?.some($0)) }
        }
        .labelsHidden().pickerStyle(.segmented).fixedSize()
    }

    private func table(_ groups: AnyRandomAccessCollection<NativeGraph.ChunkGroup>) -> some View {
        TextTable(rows: groups, columns: [
            .init("Chunks held as", width: 420, minWidth: 160, flexible: true, truncation: .byTruncatingMiddle, value: \.label, text: \.label, help: { $0.label }),
            .init("Library", width: 110, value: \.libraryText, text: \.libraryText, color: { _ in .secondaryLabelColor }),
            .init("Held by", width: 190, minWidth: 60, value: \.reach.rawValue, text: \.reach.title, color: { NSColor(color($0.reach)) }),
            .init("Chunks", width: 70, font: .digits, value: \.count, text: { $0.chunks.count.formatted() }),
            .init("Bytes", width: 90, font: .digits, value: \.bytes, text: { Format.bytes($0.bytes) }),
        ], sort: $order, selection: session.selectedChunkGroup, select: { session.selectedChunkGroup = $0; session.selectedChunk = nil })
    }

    private func chunks(_ group: NativeGraph.ChunkGroup?, _ m: Model) -> some View {
        @Bindable var session = session
        return SplitPanes(axis: .horizontal, firstMin: 220, secondMin: 320, firstIdeal: 260, showSecond: session.selectedChunk != nil) {
            if let g = group {
                Computed(snapshot, key: "chunk samples \(g.id)", label: "Finding the largest chunks", compute: { _ in
                    try m.graph.largestChunks(g.chunks, limit: 500)
                }) { chunks in
                    let title = if g.chunks.count == 1 {
                        "1 chunk"
                    } else if chunks.count < g.chunks.count {
                        "Largest \(chunks.count.formatted()) of \(g.chunks.count.formatted()) chunks"
                    } else {
                        "\(chunks.count.formatted()) chunks, largest first"
                    }
                    List(selection: $session.selectedChunk) {
                        SwiftUI.Section(title) {
                            ForEach(chunks, id: \.self) { c in
                                let chunk = m.graph.chunks[Int(c)]
                                HStack {
                                    Text(Format.hex(chunk.userStart)).font(.caption.monospaced())
                                    Spacer()
                                    Text(Format.bytes(chunk.size)).font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                                }
                            }
                        }
                    }
                }
            }
        } second: {
            if let c = session.selectedChunk {
                ChunkDetail(snapshot: snapshot, graph: m.graph, chunk: Int(c))
            }
        }
    }
}

/// One malloc chunk: what it is, who holds it, what it points to, and its first bytes.
struct ChunkDetail: View {
    let snapshot: HeapSnapshot
    let graph: NativeGraph
    let chunk: Int

    struct Model {
        let details: NativeChunkInfo
        let label: String
        let fields: [Int: String]
        let bytes: String
    }

    var body: some View {
        Computed(snapshot, key: "native chunk \(chunk)", label: "Reading the chunk", compute: { s in
            let details = graph.details(chunk)
            var fields: [Int: String] = [:]
            for owner in details.owners.prefix(50) {
                fields[owner] = try graph.ownerField(owner, chunk: chunk, snapshot: s)
            }
            return try Model(details: details, label: graph.groupLabel(chunk, snapshot: s), fields: fields, bytes: hexDump(details.chunk))
        }) { model in
            content(model)
        }
        .id(chunk)
    }

    private func content(_ m: Model) -> some View {
        let c = m.details.chunk
        return ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                Text(m.label).font(.headline).lineLimit(3).textSelection(.enabled)
                Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 3) {
                    row("Address", Format.hex(c.userStart) + " (header " + Format.hex(c.start) + ")")
                    row("Size", Format.bytes(c.size))
                    row("Held by", m.details.reach.title)
                    if let l = m.details.library { row("Library", l) }
                    row("Pointed to by", "\(m.details.referrers.count) chunks" + (graph.isRootChunk(Int32(chunk)) ? ", " + (m.details.rootSource ?? "a native root") : ""))
                    row("Points to", "\(m.details.pointers.count) chunks")
                    if let o = m.details.owner {
                        GridRow {
                            Text("Kept by").foregroundStyle(.secondary)
                            ObjectLink(snapshot: snapshot, object: o)
                        }
                    }
                }
                .font(.callout.monospacedDigit())
                if !m.details.owners.isEmpty {
                    Text("Managed objects that point to it").font(.caption.weight(.semibold))
                    ForEach(m.details.owners.prefix(50), id: \.self) { o in
                        HStack(spacing: 6) {
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
                            HStack(spacing: 6) {
                                Text((HandleTables.typeNames[h.type] ?? "type \(h.type)") + " handle " + Format.hex(slot)).font(.caption)
                                if let t = h.target { ObjectLink(snapshot: snapshot, object: t) }
                            }
                        }
                    }
                }
                Text("First bytes").font(.caption.weight(.semibold))
                Text(m.bytes).font(.caption.monospaced()).textSelection(.enabled)
            }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
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

extension NativeGraph.ChunkGroup {
    var libraryText: String { library ?? "" }
    var count: Int { chunks.count }
}
