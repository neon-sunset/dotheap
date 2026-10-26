import HeapCore
import SwiftUI

struct RegionRow: Identifiable {
    let id: Int
    let region: Region
    var start: UInt64 { region.start }
    var size: UInt64 { region.size }
    var generation: String { region.generationName }
    var queue: String { region.queue }
    var committed: UInt64 { region.committedBytes }
    var used: UInt64 { region.usedBytes }
    var live: UInt64 { region.liveBytes }
    var pending: UInt64 { region.pendingBytes }
    var dead: UInt64 { region.deadBytes }
    var free: UInt64 { region.freeBytes }
    var liveShare: Double { region.size > 0 ? Double(region.liveBytes) / Double(region.size) : 0 }
    var objects: Int { region.endObject - region.firstObject }
    let kind: String
}

struct RegionsView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var sort: TableSort? = TableSort(column: "Address", ascending: true)
    @State private var filter = ""

    var body: some View {
        let kinds = snapshot.satoriRegionKinds
        let all: [RegionRow] = snapshot.memo("region rows") {
            snapshot.heap.regions.enumerated().map { RegionRow(id: $0.offset, region: $0.element, kind: $0.offset < kinds.count ? kinds[$0.offset] : "") }
        }
        let rows = filter.isEmpty ? all : all.filter {
            $0.queue.localizedCaseInsensitiveContains(filter) || Glossary.queue($0.queue).name.localizedCaseInsensitiveContains(filter) || $0.generation.contains(filter)
                || Format.hex($0.start).contains(filter)
        }
        let satori = snapshot.heap.kind.gc == .satori
        let columns: [TextTable<RegionRow, Int>.Column] = [
            .init("Address", width: 110, font: .monospaced, value: \.start, text: { Format.hex($0.start) }),
            .init("Size", width: 80, font: .digits, value: \.size, text: { Format.mb($0.size) }),
            .init("Gen", width: 80, value: \.generation, text: \.generation, color: { NSColor(Palette.generation($0.generation)) }),
            .init(satori ? "Queue" : "List", width: 150, minWidth: 100, flexible: true, value: \.queue, text: { Glossary.queue($0.queue).name }, help: { r in
                let q = Glossary.queue(r.queue)
                return q.help.isEmpty ? r.queue : "\(q.help)\n\nSatori queue: \(r.queue)"
            }),
        ] + (kinds.isEmpty ? [] : [
            .init("Kind", width: 100, minWidth: 50, value: \.kind, text: \.kind, color: { NSColor(Palette.satoriKind($0.kind)) }),
        ]) + [
            .init("Committed", width: 85, font: .digits, value: \.committed, text: { Format.mb($0.committed) }),
            .init("Used", width: 80, font: .digits, value: \.used, text: { Format.mb($0.used) }),
            .init("Live", width: 80, font: .digits, value: \.live, text: { Format.mb($0.live) }),
            .init("Pending finalization", width: 80, font: .digits, value: \.pending, text: { Format.mb($0.pending) }),
            .init("Dead", width: 80, font: .digits, value: \.dead, text: { Format.mb($0.dead) }),
            .init("Free", width: 80, font: .digits, value: \.free, text: { Format.mb($0.free) }),
            .init("Live / size", width: 120, value: \.liveShare, bar: { OccupancyBar.segments($0.region) }),
            .init("Objects", width: 80, font: .digits, value: \.objects, text: { $0.objects.formatted() }),
        ]
        SplitPanes(axis: .vertical, firstMin: 200, secondMin: 260, initial: 0.45, showSecond: session.selectedRegion != nil) {
            VStack(alignment: .leading, spacing: 0) {
                PageToolbar {
                    TextField(satori ? "Filter by queue, generation or address" : "Filter by list, generation or address", text: $filter)
                        .textFieldStyle(.roundedBorder).frame(maxWidth: 320)
                }
                TextTable(rows: rows, columns: columns, sort: $sort, selection: session.selectedRegion, select: { session.selectedRegion = $0 })
                    .padding(PageInsets.content)
            }
        } second: {
            if let r = session.selectedRegion {
                RegionDetailView(snapshot: snapshot, regionIndex: r)
            }
        }
        .onChange(of: session.selectedRegion) { _, region in
            if let region, !rows.contains(where: { $0.id == region }) { filter = "" }
        }
    }
}

/// Live, pending finalization, dead and free bytes of a region as one horizontal bar.
struct OccupancyBar: View {
    let region: Region

    var body: some View {
        GeometryReader { geo in
            HStack(spacing: 0) {
                ForEach(Array(Self.segments(region).enumerated()), id: \.offset) { _, s in
                    Color(nsColor: s.color).frame(width: geo.size.width * s.share)
                }
                Spacer(minLength: 0)
            }
            .background(Palette.uncommitted)
            .clipShape(RoundedRectangle(cornerRadius: 2))
        }
        .frame(height: 10)
    }

    /// The parts of the bar as shares of the region's size, from the left.
    static func segments(_ region: Region) -> [(share: Double, color: NSColor)] {
        let size = Double(max(region.size, 1))
        return [(region.liveBytes, Palette.live), (region.pendingBytes, Palette.pending), (region.deadBytes, Palette.dead),
                (region.freeBytes, Palette.free), (region.gcStorageBytes, Palette.gcStorage), (region.unparsedBytes, Palette.unparsed),
                (region.committedUnusedBytes, Palette.committedUnused)]
            .map { (Double($0.0) / size, NSColor($0.1)) }
    }
}

struct RegionDetailView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let regionIndex: Int
    @State private var objectSort: TableSort? = TableSort(column: "Address", ascending: true)

    struct ObjectRow: Identifiable {
        let id: Int
        let address: UInt64
        let type: String
        let size: UInt64
        let objectState: HeapGraph.ObjectState
        let retained: UInt64
        var state: String { objectState.rawValue }
        var stateOrder: Int {
            switch objectState {
            case .live: 0
            case .pendingFinalization: 1
            case .dead: 2
            }
        }
    }

    var body: some View {
        let r = snapshot.heap.regions[regionIndex]
        let heap = snapshot.heap
        let satori = heap.kind.gc == .satori
        SplitPanes(axis: .horizontal, firstMin: 320, secondMin: 360, firstIdeal: 380) {
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    Text("Region \(Format.hex(r.start))").font(.title3.weight(.semibold))
                    RegionStrip(snapshot: snapshot, regionIndex: regionIndex).frame(height: 36)
                    ColorLegend(items: [("Live", Palette.live), ("Pending finalization", Palette.pending), ("Dead", Palette.dead), ("Gaps", Palette.free),
                                        ("GC storage", Palette.gcStorage), ("Header", Palette.headers), (satori ? "Committed, unused (above m_used, all zero)" : "Committed, above allocated", Palette.committedUnused), ("Uncommitted", Palette.uncommitted)])
                    Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 3) {
                        row("Size", Format.mb(r.size))
                        row("Generation", r.generationName)
                        GridRow {
                            Text(satori ? "Queue" : "List").foregroundStyle(.secondary)
                            QueueLabel(raw: r.queue)
                        }
                        if satori, regionIndex < snapshot.satoriRegionKinds.count { row("Kind", snapshot.satoriRegionKinds[regionIndex]) }
                        if satori { row("Reusable for", ["none", "gen1", "gen0"][min(Int(r.reusableFor), 2)]) }
                        if r.ownerThreadTag != 0 { row("Escape tracking for", r.ownerThread.map { "thread \($0)" } ?? Format.hex(r.ownerThreadTag)) }
                        if let thread = r.allocatingThread { row("Allocating thread", "thread \(thread)") }
                        else if r.attachmentPoint != 0 { row("Allocation context", Format.hex(r.attachmentPoint)) }
                        row("Committed", Format.mb(r.committedBytes))
                        row(satori ? "Used (m_used)" : "Allocated", Format.mb(r.usedBytes))
                        row("Live", Format.mb(r.liveBytes))
                        row("Pending finalization", Format.mb(r.pendingBytes))
                        row("Dead", Format.mb(r.deadBytes))
                        row("Free gaps", Format.mb(r.freeBytes))
                        if let storage = r.gcStorage {
                            row("GC storage", "\(Format.mb(r.gcStorageBytes)), \(storage)")
                                .help("Committed space reserved for GC data, including unused capacity. It is unavailable for managed object allocation.")
                        }
                        row("Unparsed", Format.mb(r.unparsedBytes))
                        if satori {
                            row("Satori occupancy", r.hasGeneration ? Format.mb(r.occupancy) : "–")
                                .help("Bytes Satori counts as occupied, including active allocation ranges. Objects that died since collection remain counted until collected.")
                            row("Satori object count", r.hasGeneration ? "\(r.objCount)" : "–")
                            row("Allocation range", r.allocEnd > r.allocStart ? "\(Format.hex(r.allocStart)) – \(Format.hex(r.allocEnd))" : "none")
                        }
                        if r.walkStopped { row("Walk", "stopped early") }
                    }
                    .font(.callout.monospacedDigit())
                    Text("Types").font(.headline).padding(.top, 6)
                    Computed(snapshot, key: "region types \(regionIndex)", label: "Counting types", compute: { try $0.regionTypes(regionIndex) }) { (types: RustArray<HeapSnapshot.TypeGroup>) in
                        ForEach(types.prefix(12)) { t in
                            HStack {
                                Text(TypeNames.short(t.name)).lineLimit(1).truncationMode(.middle)
                                Spacer()
                                Text("\(t.objects) × \(Format.bytes(t.bytes))").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                            }
                        }
                    }
                    .id(regionIndex)
                }
                .padding(PageInsets.detail)
            }
        } second: {
            Computed(snapshot, key: "region rows \(regionIndex)", label: "Reading the objects", compute: { s in
                (r.firstObject..<r.endObject).map {
                    ObjectRow(id: $0, address: heap.addresses[$0], type: TypeNames.short(s.typeName(ofObject: $0)), size: heap.size($0),
                              objectState: s.graph.state($0), retained: s.graph.retained[$0])
                }
            }) { (all: [ObjectRow]) in
                TextTable(rows: all, columns: [
                    .init("Address", width: 125, font: .monospaced, value: \.address, text: { Format.hex($0.address) }),
                    .init("Type", width: 200, flexible: true, value: \.type, text: \.type),
                    .init("Size", width: 80, font: .digits, value: \.size, text: { Format.bytes($0.size) }),
                    .init("Retained", width: 80, font: .digits, value: \.retained, text: { Format.bytes($0.retained) }),
                    .init("State", width: 45, value: \.stateOrder, text: \.state, color: { NSColor(Palette.state($0.objectState)) }),
                ], sort: $objectSort, selection: session.selectedObject, select: { if let i = $0 { session.select(object: i) } })
            }
            .id(regionIndex)
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }
}

/// The region's address range left to right: live, pending finalization and dead objects, gaps, committed and
/// uncommitted space.
struct RegionStrip: View {
    let snapshot: HeapSnapshot
    let regionIndex: Int

    var body: some View {
        let r = snapshot.heap.regions[regionIndex]
        Canvas { ctx, size in
            let buckets = max(1, Int(size.width))
            let (live, pending, dead) = snapshot.regionStrip(regionIndex, buckets: buckets)
            let bytesPerPixel = Double(r.size) / Double(buckets)
            for b in 0..<buckets {
                let lo = Double(b) * bytesPerPixel
                let x = CGFloat(b)
                let color: Color
                if lo >= Double(r.committedBytes) { color = Palette.uncommitted }
                else if lo < Double(r.headerBytes) { color = Palette.headers }
                else if r.gcStorage != nil { color = Palette.gcStorage }
                else if lo >= Double(r.usedBytes) { color = Palette.committedUnused }
                else { color = Palette.free }
                ctx.fill(Path(CGRect(x: x, y: 0, width: 1, height: size.height)), with: .color(color))
                var top = size.height
                for (share, color) in [(live[b], Palette.live), (pending[b], Palette.pending), (dead[b], Palette.dead)] where share > 0 {
                    let h = size.height * share
                    ctx.fill(Path(CGRect(x: x, y: top - h, width: 1, height: h)), with: .color(color))
                    top -= h
                }
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 3))
        .help("Address range of the region. Bar height is the share of each pixel's bytes held by live, pending finalization and dead objects.")
    }
}
