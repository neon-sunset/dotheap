import HeapCore
import SwiftUI

enum MapColoring: String, CaseIterable, Identifiable {
    case bytes = "Bytes", generation = "Generation", queue = "Queue", kind = "Kind"
    var id: String { rawValue }

    /// The modes for a GC: for the stock GC the third mode is the GC heap that holds the region (Satori's is the queue),
    /// and the stock GC has no region kinds of Satori.
    static func modes(_ gc: HeapKind.GC) -> [MapColoring] { gc == .satori ? allCases : [.bytes, .generation, .queue] }
    func title(_ gc: HeapKind.GC) -> String { self == .queue && gc != .satori ? "Heap" : rawValue }

    /// The name the queue mode colors a region by: its Satori queue, or for the stock GC its GC heap.
    static func queueKey(_ r: Region, _ kind: HeapKind) -> String {
        if kind.gc == .satori { return r.queue }
        if r.gcHeap >= 0 { return "heap \(r.gcHeap)" }
        // Workstation GC has one heap; the server GC keeps some free regions in global lists of no heap.
        return kind.layout.hasPrefix("server") ? "no heap (global free list)" : "heap 0"
    }
}

extension Palette {
    static func satoriKind(_ kind: String) -> Color {
        switch kind {
        case "huge": generation("immortal")
        case "large objects": generation("LOH")
        case "free": Palette.uncommitted
        case "mark work chunks", "finalization queue": Palette.gcStorage
        case "unassigned": Palette.unparsed
        default: Color(white: 0.55)
        }
    }

    /// The generation names of a GC, in the order of the legend.
    static func generations(_ gc: HeapKind.GC) -> [String] {
        gc == .satori ? ["gen0", "gen1", "gen2", "immortal", "none"] : ["gen0", "gen1", "gen2", "LOH", "POH", "NonGC heap", "free"]
    }
}

struct HeapMapView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var granules: RustArray<RustArray<Granule>> = RustArray()
    @AppStorage("heapMapColoring") private var coloring: MapColoring = .bytes
    @AppStorage("heapMapCell") private var cell: Double = 14
    @State private var hoverRegion: Int?
    @State private var showEmpty = false

    var body: some View {
        SplitPanes(axis: .vertical, firstMin: 260, secondMin: 260, initial: 0.55, showSecond: session.selectedRegion != nil) {
            VStack(spacing: 0) {
                controls
                Divider()
                // Cells wrap to the window width; the slider only sets the cell size.
                GeometryReader { geo in
                    let columns = max(8, Int((geo.size.width - 28) / cell))
                    ScrollViewReader { proxy in
                        ScrollView(.vertical) {
                            let selectedPage = session.selectedRegion.flatMap(page(of:)), hoverPage = hoverRegion.flatMap(page(of:))
                            LazyVStack(alignment: .leading, spacing: 14) {
                                ForEach(Array(snapshot.heap.pages.enumerated()), id: \.offset) { p, page in
                                    if p < granules.count, showEmpty || granules[p].contains(where: { $0.committed > 0 }) {
                                        PageGrid(snapshot: snapshot, page: page, cells: granules[p], coloring: mode, cell: cell, columns: columns,
                                                 selected: selectedPage == p ? session.selectedRegion : nil, hover: hoverPage == p ? hoverRegion : nil,
                                                 setHover: { hoverRegion = $0 }, select: { session.selectedRegion = $0 })
                                            .equatable()
                                            .id(p)
                                    }
                                }
                            }
                            .padding(14)
                        }
                        // A region opened from elsewhere: bring its page into view.
                        .onChange(of: session.selectedRegion, initial: true) {
                            guard let r = session.selectedRegion, let p = snapshot.heap.pages.firstIndex(where: { snapshot.heap.regions[r].start >= $0.start && snapshot.heap.regions[r].start < $0.end }) else { return }
                            withAnimation { proxy.scrollTo(p, anchor: .top) }
                        }
                        .onChange(of: granules.count) {
                            guard let r = session.selectedRegion, let p = snapshot.heap.pages.firstIndex(where: { snapshot.heap.regions[r].start >= $0.start && snapshot.heap.regions[r].start < $0.end }) else { return }
                            proxy.scrollTo(p, anchor: .top)
                        }
                    }
                }
                Divider()
                statusBar
            }
        } second: {
            if let r = session.selectedRegion {
                RegionDetailView(snapshot: snapshot, regionIndex: r)
            }
        }
        .task {
            if granules.isEmpty {
                let s = snapshot
                granules = await Task.detached { s.memo("granules") { s.granuleMap() } }.value
            }
        }
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 16) {
                Picker("Color", selection: $coloring) { ForEach(MapColoring.modes(gc)) { Text($0.title(gc)).tag($0) } }
                    .pickerStyle(.segmented).fixedSize()
                Slider(value: $cell, in: 6...30) { Text("Cell") }.frame(width: 180)
                Toggle(gc == .satori ? "Show empty pages" : "Show empty ranges", isOn: $showEmpty)
                Spacer()
            }
            switch mode {
            case .bytes:
                legend([("Live", Palette.live), ("Pending finalization", Palette.pending), ("Dead", Palette.dead), ("Gaps", Palette.free), ("Header", Palette.headers),
                        ("GC storage", Palette.gcStorage), ("Committed, unused", Palette.committedUnused), ("Uncommitted", Palette.uncommitted)])
            case .generation:
                HStack(spacing: 16) {
                    legend(Palette.generations(gc).map { ($0, Palette.generation($0)) })
                    HStack(spacing: 4) {
                        LinearGradient(colors: [Palette.generation("gen2").opacity(0.35), Palette.generation("gen2")], startPoint: .leading, endPoint: .trailing)
                            .frame(width: 40, height: 10).clipShape(RoundedRectangle(cornerRadius: 2))
                        Text("dim to bright: share of the granule that is live").font(.caption).fixedSize()
                    }
                    HStack(spacing: 4) {
                        RoundedRectangle(cornerRadius: 2).fill(Palette.uncommitted).frame(width: 10, height: 10)
                        Text("no region or uncommitted").font(.caption).fixedSize()
                    }
                }
            case .queue:
                legend(Array(Set(snapshot.heap.regions.map { MapColoring.queueKey($0, snapshot.heap.kind) })).sorted { $0.localizedStandardCompare($1) == .orderedAscending }
                    .map { (gc == .satori ? Glossary.queue($0).name : $0, Palette.hashed($0)) })
            case .kind:
                legend(["regular", "large objects", "huge", "free", "mark work chunks", "finalization queue", "unassigned"].map { ($0, Palette.satoriKind($0)) })
                Text("Regular regions hold objects below 32 KB. Objects from 32 KB to about 2 MB use large-object regions. Each larger object has its own huge region spanning multiple 2 MB granules.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 14).padding(.vertical, 8)
    }

    private func legend(_ items: [(String, Color)]) -> some View { ColorLegend(items: items) }

    private func page(of region: Int) -> Int? {
        let start = snapshot.heap.regions[region].start
        return snapshot.heap.pages.firstIndex { start >= $0.start && start < $0.end }
    }

    private var gc: HeapKind.GC { snapshot.heap.kind.gc }
    /// The coloring, or bytes when the saved one does not apply to this GC.
    private var mode: MapColoring { MapColoring.modes(gc).contains(coloring) ? coloring : .bytes }
    private var kinds: RustArray<String> { snapshot.satoriRegionKinds }

    private var statusBar: some View {
        HStack {
            if let i = hoverRegion {
                let r = snapshot.heap.regions[i]
                Text("\(Format.hex(r.start))  \(Format.mb(r.size))  \(r.generationName)  \(Glossary.queue(r.queue).name)\(i < kinds.count ? "  " + kinds[i] : "")  used \(Format.mb(r.usedBytes))  live \(Format.mb(r.liveBytes))\(r.pendingBytes > 0 ? "  pending finalization " + Format.mb(r.pendingBytes) : "")  dead \(Format.mb(r.deadBytes))  free \(Format.mb(r.freeBytes))  \(r.endObject - r.firstObject) objects")
                    .font(.caption.monospacedDigit())
            } else {
                Text("Each cell is a 2 MB granule. Click a region for its layout and objects.").font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(.horizontal, 14).padding(.vertical, 6)
    }
}

/// One page of the map. Equal when what it draws is equal, so a hover or a selection on another page does not redraw
/// it: the closures are left out of the comparison.
struct PageGrid: View, Equatable {
    let snapshot: HeapSnapshot
    let page: Page
    let cells: RustArray<Granule>
    let coloring: MapColoring
    let cell: CGFloat
    let columns: Int
    /// The selected and the hovered region, when they are on this page.
    let selected: Int?
    let hover: Int?
    let setHover: (Int?) -> Void
    let select: (Int) -> Void

    static func == (a: PageGrid, b: PageGrid) -> Bool {
        a.snapshot === b.snapshot && a.page.start == b.page.start && a.cells.count == b.cells.count && a.coloring == b.coloring
            && a.cell == b.cell && a.columns == b.columns && a.selected == b.selected && a.hover == b.hover
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            let committed = cells.reduce(0) { $0 + UInt64($1.committed) }
            Text("\(snapshot.heap.kind.gc == .satori ? "Page" : "Range") \(Format.hex(page.start)) – \(Format.mb(page.end - page.start)), committed \(Format.mb(committed))")
                .font(.caption.weight(.medium)).foregroundStyle(.secondary)
            Canvas { ctx, _ in draw(ctx, columns: columns) }
                .frame(width: CGFloat(columns) * cell, height: CGFloat((cells.count + columns - 1) / columns) * cell)
                .onContinuousHover { phase in
                    if case let .active(p) = phase { setHover(region(at: p, columns: columns)) } else { setHover(nil) }
                }
                .onTapGesture { p in if let r = region(at: p, columns: columns) { select(r) } }
        }
    }


    private func region(at p: CGPoint, columns: Int) -> Int? {
        let k = Int(p.y / cell) * columns + Int(p.x / cell)
        guard p.x >= 0, k >= 0, k < cells.count, cells[k].region >= 0 else { return nil }
        return Int(cells[k].region)
    }

    private func draw(_ ctx: GraphicsContext, columns: Int) {
        let g = Double(ManagedHeap.granule)
        let kinds = coloring == .kind ? snapshot.satoriRegionKinds : RustArray<String>()
        for (k, c) in cells.enumerated() {
            let x = CGFloat(k % columns) * cell, y = CGFloat(k / columns) * cell
            let rect = CGRect(x: x + 0.5, y: y + 0.5, width: cell - 1, height: cell - 1)
            guard c.region >= 0 else {
                ctx.fill(Path(rect), with: .color(.secondary.opacity(0.05)))
                continue
            }
            let r = snapshot.heap.regions[Int(c.region)]
            ctx.fill(Path(rect), with: .color(Palette.uncommitted))
            switch coloring {
            case .bytes:
                var top = rect.maxY
                for (bytes, color) in [(c.live, Palette.live), (c.pending, Palette.pending), (c.dead, Palette.dead), (c.gaps, Palette.free), (c.header, Palette.headers),
                                       (c.gcStorage, Palette.gcStorage), (c.committedUnused, Palette.committedUnused)] where bytes > 0 {
                    let h = rect.height * CGFloat(Double(bytes) / g)
                    ctx.fill(Path(CGRect(x: rect.minX, y: top - h, width: rect.width, height: h)), with: .color(color))
                    top -= h
                }
            case .generation:
                let occupancy = Double(c.live) / g
                ctx.fill(Path(rect), with: .color(Palette.generation(r.generationName).opacity(c.committed > 0 ? 0.35 + 0.65 * occupancy : 0.12)))
            case .queue:
                ctx.fill(Path(rect), with: .color(Palette.hashed(MapColoring.queueKey(r, snapshot.heap.kind)).opacity(c.committed > 0 ? 1 : 0.2)))
            case .kind:
                let kind = Int(c.region) < kinds.count ? kinds[Int(c.region)] : "regular"
                ctx.fill(Path(rect), with: .color(Palette.satoriKind(kind).opacity(c.committed > 0 ? 1 : 0.2)))
            }
            if c.isRegionStart {
                ctx.fill(Path(CGRect(x: rect.minX, y: rect.minY, width: 1.5, height: rect.height)), with: .color(.primary.opacity(0.6)))
            }
            if Int(c.region) == selected || Int(c.region) == hover {
                ctx.stroke(Path(rect), with: .color(Int(c.region) == selected ? .accentColor : .primary.opacity(0.5)), lineWidth: 1.5)
            }
        }
    }
}

/// Places subviews left to right and wraps to a new line when the width runs out.
struct FlowLayout: Layout {
    var spacing: CGFloat = 8
    var lineSpacing: CGFloat?

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = arrange(width: proposal.width ?? .infinity, subviews: subviews)
        return CGSize(width: proposal.width ?? rows.map(\.width).max() ?? 0, height: rows.reduce(0) { $0 + $1.height } + (lineSpacing ?? spacing / 2) * CGFloat(max(rows.count - 1, 0)))
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var y = bounds.minY
        for row in arrange(width: bounds.width, subviews: subviews) {
            var x = bounds.minX
            for item in row.items {
                subviews[item.index].place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(item.size))
                x += item.size.width + spacing
            }
            y += row.height + (lineSpacing ?? spacing / 2)
        }
    }

    private func arrange(width: CGFloat, subviews: Subviews) -> [(items: [(index: Int, size: CGSize)], width: CGFloat, height: CGFloat)] {
        var rows: [(items: [(index: Int, size: CGSize)], width: CGFloat, height: CGFloat)] = []
        var current: (items: [(index: Int, size: CGSize)], width: CGFloat, height: CGFloat) = ([], 0, 0)
        for i in subviews.indices {
            let ideal = subviews[i].sizeThatFits(.unspecified)
            let size = ideal.width > width ? subviews[i].sizeThatFits(ProposedViewSize(width: width, height: nil)) : ideal
            if !current.items.isEmpty && current.width + spacing + size.width > width {
                rows.append(current); current = ([], 0, 0)
            }
            current.width += (current.items.isEmpty ? 0 : spacing) + size.width
            current.height = max(current.height, size.height)
            current.items.append((i, size))
        }
        if !current.items.isEmpty { rows.append(current) }
        return rows
    }
}

struct ColorLegend: View {
    let items: [(String, Color)]

    var body: some View {
        FlowLayout(spacing: 12) {
            ForEach(items, id: \.0) { name, color in
                HStack(spacing: 4) { RoundedRectangle(cornerRadius: 2).fill(color).frame(width: 10, height: 10); Text(name).font(.caption).fixedSize() }
            }
        }
    }
}
