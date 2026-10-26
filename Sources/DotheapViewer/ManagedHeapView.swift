import Charts
import HeapCore
import SwiftUI

/// One stacked bar segment: a group (generation, queue) and a byte category.
struct ByteSegment: Identifiable {
    var id: String { group + category }
    let group: String
    let category: String
    let bytes: Double
}

enum ByteCategory: String, CaseIterable {
    case live = "Live", pending = "Pending finalization", dead = "Dead", free = "Free gaps", gcStorage = "GC storage", unparsed = "Unparsed", headers = "Region headers",
         committedUnused = "Committed, unused"

    var color: Color {
        switch self {
        case .live: Palette.live
        case .pending: Palette.pending
        case .dead: Palette.dead
        case .free: Palette.free
        case .gcStorage: Palette.gcStorage
        case .unparsed: Palette.unparsed
        case .headers: Palette.headers
        case .committedUnused: Palette.committedUnused
        }
    }

    static func segments(_ stats: RustArray<RegionStats>, queueNames: Bool = false) -> [ByteSegment] {
        stats.flatMap { s -> [ByteSegment] in
            let headers = Double(s.headers)
            let values: [(ByteCategory, Double)] = [
                (.live, Double(s.live)), (.pending, Double(s.pending)), (.dead, Double(s.dead)), (.free, Double(s.free)), (.unparsed, Double(s.unparsed)),
                (.gcStorage, Double(s.gcStorage)), (.headers, min(headers, Double(s.used))), (.committedUnused, Double(s.committedUnused)),
            ]
            let name = queueNames ? Glossary.queue(s.name).name : s.displayName ?? s.name
            return values.map { ByteSegment(group: name, category: $0.0.rawValue, bytes: max(0, $0.1) / 1_048_576) }
        }
    }
}

struct StackedBytesChart: View {
    let stats: RustArray<RegionStats>
    var queueNames = false

    var body: some View {
        Chart(ByteCategory.segments(stats, queueNames: queueNames)) { s in
            BarMark(x: .value("MB", s.bytes), y: .value("Group", s.group))
                .foregroundStyle(by: .value("Category", s.category))
        }
        .chartForegroundStyleScale(domain: ByteCategory.allCases.map(\.rawValue), range: ByteCategory.allCases.map(\.color))
        .chartXAxisLabel("MB")
        .frame(height: CGFloat(max(stats.count, 2)) * 34 + 50)
    }
}

struct RegionStatsTable: View {
    let stats: RustArray<RegionStats>
    var queueNames = false
    var gc = HeapKind.GC.satori

    var body: some View {
        Grid(alignment: .trailing, horizontalSpacing: 14, verticalSpacing: 4) {
            GridRow {
                Text("").gridColumnAlignment(.leading)
                ForEach(["Regions", "Span", "Committed", "Used", "Live", "Pending finalization", "Dead", "Free", "GC storage", "Unparsed", "Live/used", "Satori occupancy"], id: \.self) {
                    Text($0).font(.caption).foregroundStyle(.secondary)
                }
            }
            Divider()
            ForEach(stats) { s in
                GridRow {
                    Group {
                        if queueNames { QueueLabel(raw: s.name) } else { Text(s.name).help(Glossary.generation(s.name, gc: gc)) }
                    }
                    .gridColumnAlignment(.leading)
                    Text("\(s.regions)")
                    Text(Format.mb(s.span)); Text(Format.mb(s.committed)); Text(Format.mb(s.used))
                    Text(Format.mb(s.live)); Text(Format.mb(s.pending)); Text(Format.mb(s.dead)); Text(Format.mb(s.free))
                    Text(Format.mb(s.gcStorage)); Text(Format.mb(s.unparsed))
                    Text(Format.percent(Double(s.live), Double(s.used)))
                    Text(s.satoriOccupancy.map(Format.mb) ?? "–")
                }
                .font(.callout.monospacedDigit())
            }
        }
    }
}

/// The managed heap: its statistics, and its regions as a map or a list. A region selected in one view stays selected
/// in the other.
struct ManagedHeapView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot

    var body: some View {
        @Bindable var session = session
        VStack(spacing: 0) {
            PageToolbar {
                SegmentedPicker(title: "View", values: HeapView.allCases, selection: $session.heapView, label: \.rawValue)
                    .fixedSize()
            }
            switch session.heapView {
            case .statistics: HeapStatisticsView(snapshot: snapshot)
            case .map: HeapMapView(snapshot: snapshot)
            case .list: RegionsView(snapshot: snapshot)
            }
        }
    }
}

struct HeapStatisticsView: View {
    let snapshot: HeapSnapshot

    var body: some View {
        let t = snapshot.totalStats
        let heap = snapshot.heap
        let headers = t.headers
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("\(snapshot.dump.platform.rawValue), \(heap.kind.gc.rawValue) GC: \(heap.kind.layout)").font(.headline)
                    Text(heap.kind.detection).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                    if let r = snapshot.runtime {
                        Text("\((r.path as NSString).lastPathComponent): \(r.version ?? "no version string"), build ID \(r.buildId ?? "unknown")").font(.caption).foregroundStyle(.secondary).lineLimit(2).textSelection(.enabled)
                    }
                    let v = snapshot.memo("versions") { snapshot.versions }
                    Text(".NET libraries \(v.netCore ?? "of an unknown version")" + (v.aspNetCore.map { ", ASP.NET Core \($0)" } ?? "")).font(.caption).foregroundStyle(.secondary)
                }
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 190, maximum: 190), spacing: 10, alignment: .topLeading)], alignment: .leading, spacing: 10) {
                    Metric(title: "Committed", value: Format.mb(t.committed), detail: "\(heap.regions.count) regions")
                    Metric(title: "Used", value: Format.mb(t.used))
                    let g = snapshot.graph
                    Metric(title: "Live", value: Format.mb(t.live), detail: "\(g.liveCount.formatted()) objects, \(Format.percent(Double(t.live), Double(t.used))) of used", color: Palette.live)
                    if g.pendingCount > 0 {
                        Metric(title: "Pending finalization", value: Format.mb(t.pending), detail: "\(g.pendingCount.formatted()) objects", color: Palette.pending)
                    }
                    Metric(title: "Dead, not swept", value: Format.mb(t.dead), detail: "\((heap.count - g.liveCount - g.pendingCount).formatted()) objects", color: Palette.dead)
                    Metric(title: "Free gaps", value: Format.mb(t.free), detail: Format.percent(Double(t.free), Double(t.used)) + " of used", color: Palette.free)
                    if t.gcStorage > 0 {
                        Metric(title: "GC storage", value: Format.mb(t.gcStorage), detail: "work chunks and finalization queue; includes unused capacity", color: Palette.gcStorage)
                    }
                    Metric(title: "Region headers", value: Format.mb(headers), color: Palette.headers)
                    if heap.kind.gc == .satori {
                        Metric(title: "Reserved", value: Format.mb(heap.pages.reduce(0) { $0 + $1.end - $1.start }), detail: "\(heap.pages.count) pages of address space")
                    } else {
                        Metric(title: "Reserved", value: Format.mb(heap.regions.reduce(0) { $0 + $1.size }), detail: "by \(heap.regions.count) regions")
                    }
                    Metric(title: "Fragmentation", value: snapshot.fragmentationShare.formatted(.percent.precision(.fractionLength(0))),
                           detail: "(dead + free) / (objects + free) in regions of a generation")
                    Metric(title: "Threads", value: "\(snapshot.dump.threads.count)", detail: "\(snapshot.graph.sources.count) root sources")
                }

                section("By generation", detail: heap.kind.gc == .satori ? "Each region's m_generation: none is outside every generation; immortal is generation 3 and is never collected" : "The generation list that holds each region; LOH and POH are collected with gen2") {
                    StackedBytesChart(stats: snapshot.generationStats)
                    RegionStatsTable(stats: snapshot.generationStats, gc: heap.kind.gc)
                }
                section(heap.kind.gc == .satori ? "By containing queue" : "By GC heap", detail: heap.kind.gc == .satori ? "The Satori queue that holds each region; hover a name for what it means" : "Generation lists of each server GC heap") {
                    StackedBytesChart(stats: snapshot.queueStats, queueNames: true)
                    RegionStatsTable(stats: snapshot.queueStats, queueNames: true)
                }
                section("Live occupancy of 2 MB regions", detail: "Region count per live bytes / region size bucket") {
                    OccupancyChart(histograms: snapshot.occupancyStats)
                }
                section("By region size", detail: "Regions larger than 2 MB hold one object that does not fit in 2 MB, the finalization queue's array, or a free range in the allocator") {
                    RegionStatsTable(stats: snapshot.sizeClassStats)
                }
            }
            .padding(PageInsets.content)
        }
    }

    private func section(_ title: String, detail: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.title2.weight(.semibold))
            Text(detail).font(.callout).foregroundStyle(.secondary)
            content()
        }
    }
}

struct OccupancyChart: View {
    let histograms: RustArray<OccupancyHistogram>

    struct Bar: Identifiable {
        var id: String { generation + bucket }
        let generation: String
        let bucket: String
        let count: Int
    }

    var body: some View {
        let bars = histograms.flatMap { h in
            OccupancyHistogram.labels.enumerated().map { Bar(generation: h.generation, bucket: $0.element, count: h.counts[$0.offset]) }
        }
        Chart(bars) { b in
            BarMark(x: .value("Live occupancy", b.bucket), y: .value("Regions", b.count))
                .foregroundStyle(by: .value("Generation", b.generation))
                .position(by: .value("Generation", b.generation))
        }
        .chartForegroundStyleScale(domain: histograms.map(\.generation), range: histograms.map { Palette.generation($0.generation) })
        .chartXScale(domain: OccupancyHistogram.labels)
        .frame(height: 220)
    }
}

/// Resident memory by mapping kind, from smaps.
struct ResidencyView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var order: TableSort? = TableSort(column: "RSS", ascending: false)

    typealias Row = ResidencyCategory

    var body: some View {
        if snapshot.smaps != nil {
            let heap = snapshot.heap
            let rows = snapshot.residencyCategories
            let processRss = rows.reduce(0) { $0 + $1.rss }
            let heapRss = rows.first { $0.name == heap.label + " pages" }?.rss ?? 0
            let t = snapshot.totalStats
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    MetricRow {
                        Metric(title: "Process RSS", value: Format.mb(processRss))
                        Metric(title: "Heap RSS", value: Format.mb(heapRss), detail: Format.percent(Double(heapRss), Double(t.committed)) + " of committed")
                        Metric(title: "Live objects", value: Format.mb(t.live), detail: Format.percent(Double(t.live), Double(heapRss)) + " of heap RSS, " + Format.percent(Double(t.live), Double(processRss)) + " of process RSS", color: Palette.live)
                    }
                    Chart(rows.filter { $0.rss > 1 << 20 }) { r in
                        BarMark(x: .value("MB", Double(r.huge) / 1_048_576), y: .value("Mapping", r.name))
                            .foregroundStyle(by: .value("Pages", "Huge pages"))
                        BarMark(x: .value("MB", Double(r.rss - min(r.rss, r.huge)) / 1_048_576), y: .value("Mapping", r.name))
                            .foregroundStyle(by: .value("Pages", "Regular pages"))
                    }
                    .chartForegroundStyleScale(["Huge pages": Color.blue.opacity(0.7), "Regular pages": Palette.free])
                    .chartXAxisLabel("RSS MB")
                    .frame(height: CGFloat(rows.filter { $0.rss > 1 << 20 }.count) * 30 + 50)
                    TextTable(rows: rows, columns: [
                        .init("Mapping", width: 300, flexible: true, value: \.name, text: \.name, help: { $0.name }),
                        .init("VMAs", width: 60, font: .digits, value: \.vmas, text: { "\($0.vmas)" }),
                        .init("Size", width: 100, font: .digits, value: \.size, text: { Format.mb($0.size) }),
                        .init("RSS", width: 100, font: .digits, value: \.rss, text: { Format.mb($0.rss) }),
                        .init("THP", width: 100, font: .digits, value: \.huge, text: { Format.mb($0.huge) }),
                        .init("Swap", width: 100, font: .digits, value: \.swap, text: { Format.mb($0.swap) }),
                    ], sort: $order)
                    .frame(minHeight: 320)
                    Text("smaps is read from \(snapshot.smapsPath ?? "") and joined with the mappings in the dump.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        } else {
            VStack(spacing: 12) {
                Text("No smaps file").font(.title2)
                Text("The dump has no residency data. Add /proc/<pid>/smaps of the dumped process to see RSS, huge pages and the share of each mapping.")
                    .foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 480)
                Button("Add smaps File…") { OpenPanel.chooseSmaps(session) }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}
