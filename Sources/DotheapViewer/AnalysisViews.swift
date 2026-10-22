import HeapCore
import SwiftUI

/// Memory the dump records, by kind, without smaps.
struct NativeMemoryView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var filter = ""
    @State private var heapOrder: TableSort? = TableSort(column: "In use", ascending: false)
    @State private var mappingOrder: TableSort? = TableSort(column: "Start", ascending: true)

    var body: some View {
        let native = snapshot.native
        let categories = native.categories
        let heaps = native.mallocHeaps
        let mappings = filter.isEmpty ? AnyRandomAccessCollection(native.mappings)
            : AnyRandomAccessCollection(native.mappings.filter { $0.category.localizedCaseInsensitiveContains(filter) || $0.detail.localizedCaseInsensitiveContains(filter) })
        let residency = snapshot.agreeingResidency
        ScrollViewReader { proxy in
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                if let residency {
                    Text("Readable mapped memory in the dump, and the part of it that is resident per smaps, by what it holds.")
                        .font(.callout).foregroundStyle(.secondary)
                    let byKind = Dictionary(grouping: native.mappings, by: \.category)
                    let rss = byKind.mapValues { $0.reduce(0) { $0 + (residency.rss[$1.start] ?? 0) } }
                    // Bytes with data in the dump beyond the resident ones, per mapping.
                    let missing = byKind.mapValues { $0.reduce(UInt64(0)) { t, m in let r = residency.rss[m.start] ?? 0; return t + (m.nonZeroBytes > r ? m.nonZeroBytes - r : 0) } }
                    KindBars(rows: categories.map { ($0.name, $0.bytes, rss[$0.name] ?? 0, missing[$0.name] ?? 0) }, resident: true)
                } else {
                    Text(snapshot.smaps == nil ? "Readable mapped memory in the dump, by what it holds. This is not RSS: untouched stack and heap pages count here. Add smaps for residency."
                         : "Readable mapped memory in the dump, by what it holds. This is not RSS: untouched stack and heap pages count here. The smaps file does not match the dump, so it is not used here.")
                        .font(.callout).foregroundStyle(.secondary)
                    KindBars(rows: categories.map { ($0.name, $0.bytes, 0, 0) }, resident: false)
                }
                if !heaps.isEmpty {
                    Text("glibc " + (native.glibcVersion.map { $0 + " " } ?? "") + "malloc").font(.title3.weight(.semibold))
                    MetricRow {
                        Metric(title: "In use", value: Format.mb(heaps.reduce(0) { $0 + $1.inUseBytes }), detail: "\(heaps.reduce(0) { $0 + $1.inUseChunks }.formatted()) chunks", color: Palette.live)
                        Metric(title: "Free chunks", value: Format.mb(heaps.reduce(0) { $0 + $1.freeResident }), detail: "resident, of \(Format.mb(heaps.reduce(0) { $0 + $1.freeBytes })) in \(heaps.reduce(0) { $0 + $1.freeChunks }.formatted()) chunks", color: Palette.free)
                        Metric(title: "Top chunks", value: Format.mb(heaps.reduce(0) { $0 + $1.topResident }), detail: "resident, of \(Format.mb(heaps.reduce(0) { $0 + $1.topBytes })) at heap ends", color: Palette.committedUnused)
                        Metric(title: "malloc_trim(0) would free", value: Format.mb(heaps.reduce(0) { $0 + $1.trimReleasable }), detail: "free chunk pages; main arena top")
                        Metric(title: "Arenas", value: "\(Set(heaps.map(\.arena)).count)", detail: "\(heaps.count) heaps")
                    }
                    NativeOwnershipSection(snapshot: snapshot).id("ownership")
                    Text("glibc malloc heaps").font(.headline).padding(.top, 8)
                    TextTable(rows: heaps, columns: [
                        .init("Heap", width: 130, font: .monospaced, value: \.start, text: { Format.hex($0.start) }),
                        .init("Arena", width: 90, flexible: true, value: \.arenaName, text: \.arenaName),
                        .init("Size", width: 80, font: .digits, value: \.size, text: { Format.mb($0.size) }),
                    ] + (residency == nil ? [] : [
                        .init("Resident", width: 80, font: .digits, value: { snapshot.resident(from: $0.start, to: $0.end) ?? 0 },
                              text: { Format.mb(snapshot.resident(from: $0.start, to: $0.end) ?? 0) }, help: { _ in "RSS of the heap's pages per smaps" }),
                    ]) + [
                        .init("In use", width: 80, font: .digits, value: \.inUseBytes, text: { Format.mb($0.inUseBytes) }),
                        .init("Free", width: 80, font: .digits, value: \.freeBytes, text: { Format.mb($0.freeBytes) }),
                        .init("Free, resident", width: 90, font: .digits, value: \.freeResident, text: { Format.mb($0.freeResident) }),
                        .init("Trim would free", width: 90, font: .digits, value: \.trimReleasable, text: { Format.mb($0.trimReleasable) }),
                        .init("Largest free", width: 90, font: .digits, value: \.largestFree, text: { Format.bytes($0.largestFree) }),
                        .init("Top chunk", width: 90, font: .digits, value: \.topBytes, text: { Format.mb($0.topBytes) }),
                        .init("Chunks in use", width: 100, font: .digits, value: \.inUseChunks, text: { $0.inUseChunks.formatted() }),
                        .init("Free chunks", width: 90, font: .digits, value: \.freeChunks, text: { $0.freeChunks.formatted() }),
                    ], sort: $heapOrder)
                    .frame(minHeight: 240)
                }
                Text("Mappings").font(.title3.weight(.semibold))
                TextField("Filter by kind or detail", text: $filter).textFieldStyle(.roundedBorder).frame(maxWidth: 320)
                TextTable(rows: mappings, columns: [
                    .init("Start", width: 130, font: .monospaced, value: \.start, text: { Format.hex($0.start) }),
                    .init("Size", width: 80, font: .digits, value: \.size, text: { Format.bytes($0.size) }),
                    .init("Kind", width: 200, minWidth: 140, value: \.category, text: \.category),
                    .init("Detail", width: 300, flexible: true, value: \.detail, text: \.detail, color: { _ in .secondaryLabelColor }, help: { $0.detail }),
                    .init("Access", width: 50, value: \.access, text: \.access, color: { _ in .secondaryLabelColor }),
                ] + (residency.map { r in [
                    .init("Resident", width: 80, font: .digits, value: { r.rss[$0.start] ?? 0 }, text: { Format.bytes(r.rss[$0.start] ?? 0) }),
                    .init("Swap", width: 70, font: .digits, value: { r.swap[$0.start] ?? 0 }, text: { Format.bytes(r.swap[$0.start] ?? 0) }),
                ] } ?? []), sort: $mappingOrder)
                .frame(minHeight: 320)
            }
            .padding(20)
        }
        // A chunk group opened from search: bring the ownership section into view.
        .onChange(of: session.selectedChunkGroup, initial: true) {
            if session.selectedChunkGroup != nil { proxy.scrollTo("ownership", anchor: .top) }
        }
        }
    }
}

/// Memory by kind, one line each: the bytes in the dump as a bar, and with smaps, its parts: resident, data in the
/// dump that is not resident, and zero pages that are not.
private struct KindBars: View {
    let rows: [(name: String, bytes: UInt64, resident: UInt64, missing: UInt64)]
    let resident: Bool

    var body: some View {
        let most = CGFloat(max(1, rows.map(\.bytes).max() ?? 1))
        Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 3) {
            GridRow {
                Text("Kind")
                Color.clear.gridCellUnsizedAxes([.horizontal, .vertical])
                Text("In the dump").gridColumnAlignment(.trailing)
                if resident {
                    header("Resident", Palette.live)
                    header("Data, not resident", Palette.dead)
                    header("Zero, not resident", Palette.free)
                }
            }
            .font(.caption).foregroundStyle(.secondary)
            ForEach(rows.indices, id: \.self) { i in
                let r = rows[i], resident = min(r.resident, r.bytes), missing = min(r.missing, r.bytes - resident)
                GridRow {
                    Text(r.name).lineLimit(1)
                    GeometryReader { g in
                        ZStack(alignment: .leading) {
                            bar(r.bytes, of: most, in: g.size.width).fill(self.resident ? Palette.free : Palette.hashed(r.name))
                            if self.resident {
                                bar(resident + missing, of: most, in: g.size.width).fill(Palette.dead)
                                bar(resident, of: most, in: g.size.width).fill(Palette.live)
                            }
                        }
                    }
                    .frame(minWidth: 120, maxWidth: .infinity, minHeight: 9, maxHeight: 9)
                    Text(Format.mb(r.bytes)).monospacedDigit()
                    if self.resident {
                        Text(Format.mb(resident)).monospacedDigit()
                        Text(Format.mb(missing)).monospacedDigit().foregroundStyle(missing > 0 ? .primary : .secondary)
                        Text(Format.mb(r.bytes - resident - missing)).monospacedDigit().foregroundStyle(.secondary)
                    }
                }
            }
        }
        .font(.callout)
    }

    private func header(_ title: String, _ color: Color) -> some View {
        HStack(spacing: 4) {
            Circle().fill(color).frame(width: 7, height: 7)
            Text(title)
        }
        .gridColumnAlignment(.trailing)
    }

    /// A bar for `bytes` of the largest `most`, at least a line wide when there are any.
    private func bar(_ bytes: UInt64, of most: CGFloat, in width: CGFloat) -> some Shape {
        RoundedRectangle(cornerRadius: 2).size(width: bytes == 0 ? 0 : max(1.5, width * CGFloat(bytes) / most), height: 9)
    }
}

extension NativeMemory.MallocHeap {
    var size: UInt64 { end - start }
    var arenaName: String { mainArena ? "main (brk)" : "arena" }
}

extension NativeMemory.Mapping {
    var access: String { writable ? "rw" : "r" }
}

/// Dead objects grouped by the unreferenced dead objects that hold them.
struct GarbageView: View {
    let snapshot: HeapSnapshot
    @State private var selected: String?
    @State private var order: TableSort? = TableSort(column: "Garbage held", ascending: false)

    var body: some View {
        Computed(snapshot, key: "garbage page", label: "Building the dead object graph", compute: { (try $0.garbageByType(), try $0.garbage) }) { groups, g in
            SplitPanes(axis: .horizontal, firstMin: 480, secondMin: 340, initial: 0.65, showSecond: groups.contains { $0.id == selected }) {
                VStack(alignment: .leading, spacing: 8) {
                    MetricRow {
                        Metric(title: "Dead objects", value: Format.mb(g.deadBytes), detail: "\(g.deadCount.formatted()) objects", color: Palette.dead)
                        Metric(title: "Unreferenced dead objects", value: g.unreferenced.count.formatted(), detail: "no dead object references them")
                    }
                    Text("Dead objects are unreachable but have not been swept. An unreferenced dead object has no referrers among other dead objects. Expand it to see the dead objects reachable only through it.")
                        .font(.caption).foregroundStyle(.secondary)
                    TextTable(rows: groups, columns: [
                        .init("Type of the unreferenced dead object", width: 300, flexible: true, truncation: .byTruncatingMiddle, value: \.type,
                              text: { TypeNames.short($0.type) }, help: { $0.type }),
                        .init("Objects", width: 80, font: .digits, value: \.unreferenced, text: { $0.unreferenced.formatted() }),
                        .init("Garbage held", width: 110, font: .digits, value: \.retained, text: { Format.bytes($0.retained) }),
                    ], sort: $order, selection: selected, select: { selected = $0 })
                }
                .padding(12)
            } second: {
                if let key = selected, let group = groups.first(where: { $0.id == key }) {
                    SampleList(snapshot: snapshot, title: "Largest " + TypeNames.short(group.type), samples: group.samples) { "holds " + Format.bytes(g.retained($0)) }
                }
            }
        }
    }
}

enum DiagnosticCheck: String, CaseIterable, Identifiable {
    case finalizers = "Finalizers", collections = "Collection capacity", zeros = "Mostly zero arrays", strings = "Duplicate strings", boxed = "Boxed values"
    var id: String { rawValue }

    var title: String { self == .finalizers ? "Objects with finalizers" : rawValue }

    var explanation: String {
        switch self {
        case .finalizers: "Live registered objects will need finalization when they become unreachable. Live suppressed or immortal objects have no pending finalizer; Dispose often calls GC.SuppressFinalize. Resurrected objects are reachable but neither registered nor suppressed, indicating finalization has begun or completed. Pending objects are unreachable but await finalization and can retain native resources. GC.ReRegisterForFinalize can keep an object pending after a finalizer run; a dump cannot show that history."
        case .collections: "Live List, Dictionary, HashSet, Queue and Stack instances: slots in their backing arrays against slots in use. Select a row for the instances with the most unused space."
        case .zeros: "Live arrays of primitives of 1 KB or more with at least 90% zero bytes."
        case .strings: "Live strings with equal contents; waste counts the copies beyond the first."
        case .boxed: "Primitive and enum values stored as objects."
        }
    }
}

/// One heap use list (or the finalizable objects); each row opens its objects.
struct HeapUseView: View {
    let snapshot: HeapSnapshot
    let check: DiagnosticCheck
    @State private var selected: String?
    @State private var finalizerOrder: TableSort? = TableSort(column: "Pending finalization", ascending: false)
    @State private var collectionOrder: TableSort? = TableSort(column: "Unused slots", ascending: false)
    @State private var zeroOrder: TableSort? = TableSort(column: "Zero bytes", ascending: false)
    @State private var stringOrder: TableSort? = TableSort(column: "Waste", ascending: false)
    @State private var boxedOrder: TableSort? = TableSort(column: "Dead", ascending: false)

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(check.title).font(.title2.weight(.semibold))
            Text(check.explanation).font(.callout).foregroundStyle(.secondary).lineLimit(4).frame(maxWidth: .infinity, alignment: .leading)
            // A table asks for the height of all its rows; without a bound the page grows past the window and
            // pushes the sidebar and header out of view.
            SplitPanes(axis: .horizontal, firstMin: 520, secondMin: 240, initial: 0.7, showSecond: selected != nil) {
                table
            } second: {
                if selected != nil { detail }
            }
            .frame(minHeight: 200, idealHeight: 500, maxHeight: .infinity)
        }
        .padding(16)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    @ViewBuilder private var table: some View {
        switch check {
        case .finalizers:
            Computed(snapshot, key: "finalizable", label: "Reading the finalization lists", compute: { try $0.finalizableObjects() }) { (rows: RustArray<HeapSnapshot.Finalizable>) in
                TextTable(rows: rows, columns: [
                    .init("Type", width: 260, minWidth: 160, flexible: true, truncation: .byTruncatingMiddle, value: \.type,
                          text: { TypeNames.short($0.type) }, help: { $0.type }),
                    .init("Live, registered", width: 105, font: .digits, value: \.liveRegistered, text: { $0.liveRegistered.formatted() }),
                    .init("Live, suppressed", width: 110, font: .digits, value: \.liveSuppressed, text: { $0.liveSuppressed.formatted() },
                          color: { _ in .secondaryLabelColor }),
                    .init("Resurrected", width: 90, font: .digits, value: \.resurrected, text: { $0.resurrected.formatted() },
                          color: { $0.resurrected > 0 ? nil : .secondaryLabelColor }),
                    .init("Pending finalization", width: 130, font: .digits, value: \.pending, text: { $0.pending.formatted() },
                          color: { $0.pending > 0 ? NSColor(Palette.pending) : nil }),
                    .init("Dead", width: 70, font: .digits, value: \.dead, text: { $0.dead.formatted() }, color: { _ in .secondaryLabelColor }),
                    .init("Finalizer re-registers", width: 120, text: { $0.finalizerCanReRegister ? "can" : "" }),
                ], sort: $finalizerOrder, selection: selected, select: { selected = $0 })
            }
        case .collections:
            Computed(snapshot, key: "collections", label: "Reading collections", compute: { try $0.collectionCapacity() }) { (rows: RustArray<HeapSnapshot.CollectionCapacity>) in
                TextTable(rows: rows, columns: [
                    .init("Collection", width: 160, minWidth: 100, flexible: true, value: \.type, text: \.type),
                    .init("Instances", width: 75, font: .digits, value: \.instances, text: { $0.instances.formatted() }),
                    .init("Empty", width: 60, font: .digits, value: \.empty, text: { $0.empty.formatted() }),
                    .init("Slots", width: 90, font: .digits, value: \.slots, text: { $0.slots.formatted() }),
                    .init("Used", width: 90, font: .digits, value: \.used, text: { $0.used.formatted() }),
                    .init("Storage", width: 85, font: .digits, value: \.storageBytes, text: { Format.bytes($0.storageBytes) }),
                    .init("Unused slots", width: 90, font: .digits, value: \.unusedBytes, text: { Format.bytes($0.unusedBytes) }),
                ], sort: $collectionOrder, selection: selected, select: { selected = $0 })
            }
        case .zeros:
            Computed(snapshot, key: "zeros", label: "Scanning arrays", compute: { try $0.mostlyZeroArrays() }) { (rows: RustArray<HeapSnapshot.ZeroArrays>) in
                TextTable(rows: rows, columns: [
                    .init("Type", width: 260, minWidth: 160, flexible: true, truncation: .byTruncatingMiddle, value: \.type,
                          text: { TypeNames.short($0.type) }, help: { $0.type }),
                    .init("Arrays", width: 70, font: .digits, value: \.arrays, text: { $0.arrays.formatted() }),
                    .init("Bytes", width: 85, font: .digits, value: \.bytes, text: { Format.bytes($0.bytes) }),
                    .init("Zero bytes", width: 85, font: .digits, value: \.zeroBytes, text: { Format.bytes($0.zeroBytes) }),
                ], sort: $zeroOrder, selection: selected, select: { selected = $0 })
            }
        case .strings:
            Computed(snapshot, key: "strings", label: "Hashing strings", compute: { try $0.duplicateStrings() }) { (rows: RustArray<HeapSnapshot.DuplicateString>) in
                TextTable(rows: rows, id: { String($0.sample) }, columns: [
                    .init("Value", width: 300, flexible: true, font: .monospaced, value: \.preview, text: \.preview, help: { $0.preview }),
                    .init("Copies", width: 65, font: .digits, value: \.count, text: { $0.count.formatted() }),
                    .init("Each", width: 70, font: .digits, value: \.bytesEach, text: { Format.bytes($0.bytesEach) }),
                    .init("Waste", width: 75, font: .digits, value: \.wasted, text: { Format.bytes($0.wasted) }),
                ], sort: $stringOrder, selection: selected, select: { selected = $0 })
            }
        case .boxed:
            Computed(snapshot, key: "boxed", label: "Counting boxed values", compute: { try $0.boxedValues() }) { (rows: RustArray<HeapSnapshot.TypeCount>) in
                TextTable(rows: rows, columns: [
                    .init("Type", width: 260, minWidth: 160, flexible: true, truncation: .byTruncatingMiddle, value: \.type, text: \.type, help: { $0.type }),
                    .init("Live", width: 70, font: .digits, value: \.live, text: { $0.live.formatted() }),
                    .init("Dead", width: 70, font: .digits, value: \.dead, text: { $0.dead.formatted() }),
                    .init("Bytes", width: 85, font: .digits, value: \.bytes, text: { Format.bytes($0.bytes) }),
                ], sort: $boxedOrder, selection: selected, select: { selected = $0 })
            }
        }
    }

    @ViewBuilder private var detail: some View {
        if let key = selected {
            switch check {
            case .finalizers:
                if let row = (snapshot.memoized("finalizable") as RustArray<HeapSnapshot.Finalizable>?)?.first(where: { $0.id == key }) {
                    SampleList(snapshot: snapshot, title: "Registered " + TypeNames.short(row.type) + " (dead first)", samples: row.samples)
                }
            case .collections:
                if let row = (snapshot.memoized("collections") as RustArray<HeapSnapshot.CollectionCapacity>?)?.first(where: { $0.type == key }) {
                    SampleList(snapshot: snapshot, title: row.type + " with the most unused slots", samples: row.samples)
                }
            case .zeros:
                if let row = (snapshot.memoized("zeros") as RustArray<HeapSnapshot.ZeroArrays>?)?.first(where: { $0.id == key }) {
                    SampleList(snapshot: snapshot, title: "Largest " + TypeNames.short(row.type), samples: row.samples)
                }
            case .strings:
                if let row = (snapshot.memoized("strings") as RustArray<HeapSnapshot.DuplicateString>?)?.first(where: { String($0.sample) == key }) {
                    SampleList(snapshot: snapshot, title: "\(row.count) copies of \"\(row.preview.prefix(60))\"", samples: row.samples)
                }
            case .boxed:
                if let row = (snapshot.memoized("boxed") as RustArray<HeapSnapshot.TypeCount>?)?.first(where: { $0.id == key }) {
                    SampleList(snapshot: snapshot, title: row.type, samples: row.samples)
                }
            }
        }
    }
}

enum FragmentationGrouping: String, CaseIterable, Identifiable {
    case code = "Application code", type = "Survivor type", owner = "Owner"
    var id: String { rawValue }
}

/// Dead objects and unused committed space, attributed to survivors and GC rules.
struct FragmentationView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var reason: String?
    @State private var reasonOrder: TableSort? = TableSort(column: "Free and dead", ascending: false)
    @State private var contributor: String?
    @AppStorage("fragmentationGrouping") private var grouping: FragmentationGrouping = .code
    @State private var order: TableSort? = TableSort(column: "Free and dead", ascending: false)

    var body: some View {
        Computed(snapshot, key: "fragmentation", label: "Attributing fragmentation", compute: { try $0.fragmentation() }) { (report: FragmentationReport) in
            let committed = snapshot.totalStats.committed
            let rows = grouping == .code ? report.byCode : grouping == .type ? report.byType : report.byOwner
            SplitPanes(axis: .vertical, firstMin: 320, secondMin: 200, initial: 0.6) {
                SplitPanes(axis: .horizontal, firstMin: 560, secondMin: 360, initial: 0.6, showSecond: rows.contains { $0.id == contributor }) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("\(Format.mb(report.totalWaste)) of the heap's \(Format.mb(committed)) committed memory is unused or holds dead objects.")
                            .font(.title3.weight(.semibold))
                        Text("Long-lived objects among short-lived ones can keep a region and its free space committed. Rows attribute unused space and dead objects in proportion to the survivors' sizes. GC storage, objects pending finalization and unparsed bytes are excluded.")
                            .font(.callout).foregroundStyle(.secondary).lineLimit(4).frame(maxWidth: .infinity, alignment: .leading)
                        Picker("Group by", selection: $grouping) { ForEach(FragmentationGrouping.allCases) { Text($0.rawValue).tag($0) } }
                            .pickerStyle(.segmented).onChange(of: grouping) { contributor = nil }
                        TextTable(rows: rows, columns: [
                            .init(grouping == .code ? "Code holding the survivors" : grouping == .type ? "Survivor type" : "Owner", width: 300, flexible: true,
                                  truncation: .byTruncatingMiddle, value: \.name, text: { TypeNames.short($0.name) }, help: { $0.name }),
                            .init("Free and dead", width: 105, font: .digits, value: \.waste, text: { Format.bytes(UInt64($0.waste)) }),
                            .init("Survivors", width: 85, font: .digits, value: \.liveBytes, text: { Format.bytes($0.liveBytes) }),
                            .init("Regions", width: 65, font: .digits, value: \.regionCount, text: { $0.regionCount.formatted() }),
                        ], sort: $order, selection: contributor, select: { contributor = $0 })
                    }
                    .padding(12)
                } second: {
                    if let key = contributor, let c = rows.first(where: { $0.id == key }) {
                        ContributorDetail(snapshot: snapshot, contributor: c)
                    }
                }
                .onAppear {
                    // -contributor <name> selects the first row whose name starts with it, for screenshots.
                    if contributor == nil, let name = UserDefaults.standard.string(forKey: "contributor") { contributor = rows.first { $0.name.hasPrefix(name) }?.id }
                }
            } second: {
                SplitPanes(axis: .horizontal, firstMin: 560, secondMin: 320, initial: 0.6, showSecond: report.reasons.contains { $0.name == reason }) {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(snapshot.heap.kind.gc == .satori ? "Why Satori keeps this space" : "Why the GC keeps this space").font(.headline)
                        TextTable(rows: report.reasons, columns: [
                            .init("Rule", width: 300, flexible: true, value: \.name, text: \.name, help: { $0.explanation }),
                            .init("Regions", width: 65, font: .digits, value: \.regions, text: { $0.regions.formatted() }),
                            .init("Free and dead", width: 105, font: .digits, value: \.waste, text: { Format.mb($0.waste) }),
                            .init("Live", width: 80, font: .digits, value: \.live, text: { Format.mb($0.live) }),
                        ], sort: $reasonOrder, selection: reason, select: { reason = $0 })
                        if let r = report.reasons.first(where: { $0.name == reason }) {
                            Text(r.explanation).font(.caption).foregroundStyle(.secondary).lineLimit(5).frame(maxWidth: .infinity, alignment: .leading)
                        }
                        if !report.gen2ByHeap.isEmpty {
                            Gen2ByHeap(heaps: report.gen2ByHeap)
                        }
                    }
                    .padding(12)
                } second: {
                    if let r = report.reasons.first(where: { $0.name == reason }) {
                        List(r.regionIndices.prefix(500), id: \.self) { k in
                            let region = snapshot.heap.regions[k]
                            Button {
                                session.show(region: k)
                            } label: {
                                HStack {
                                    Text(Format.hex(region.start)).monospaced().lineLimit(1).fixedSize()
                                    Text("\(region.generationName), \(Glossary.queue(region.queue).name)").foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
                                    Spacer(minLength: 8)
                                    OccupancyBar(region: region).frame(width: 90)
                                }
                                .contentShape(Rectangle())
                            }.buttonStyle(.plain)
                        }
                    }
                }
            }
        }
    }
}

/// Stock GC: gen2 free gaps and dead objects of each heap against the share at which a blocking gen2 GC compacts.
struct Gen2ByHeap: View {
    let heaps: RustArray<FragmentationReport.Gen2Heap>

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("By heap: a blocking gen2 GC compacts when free and dead space in gen0, gen1 and gen2 is at least \(Int(StockGC.gen2FragmentationBurden * 100))% of their size on one heap; with server GC, all heaps then compact. Background gen2 GCs never compact.")
                .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            Grid(alignment: .trailing, horizontalSpacing: 14, verticalSpacing: 2) {
                GridRow {
                    ForEach(["Heap", "Gen0-gen2", "Free and dead", "Share"], id: \.self) { Text($0).font(.caption).foregroundStyle(.secondary) }
                }
                ForEach(heaps.prefix(64)) { h in
                    GridRow {
                        Text(h.heap < 0 ? "workstation" : "\(h.heap)")
                        Text(Format.mb(h.size))
                        Text(Format.mb(h.fragmentation))
                        Text(Format.percent(Double(h.fragmentation), Double(h.size))).foregroundStyle(h.aboveThreshold ? Palette.dead : .primary)
                    }
                    .font(.caption.monospacedDigit())
                }
            }
        }
    }
}

/// What a contributor's survivors are, what died around them, and the objects themselves.
struct ContributorDetail: View {
    let snapshot: HeapSnapshot
    let contributor: FragmentationReport.Contributor

    var body: some View {
        ScrollView {
            content.frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var content: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(contributor.name).font(.headline).lineLimit(3)
            if !contributor.survivorTypes.isEmpty {
                Text("Survivors").font(.subheadline.weight(.semibold))
                ForEach(contributor.survivorTypes, id: \.name) { t in
                    HStack { Text(TypeNames.short(t.name)).lineLimit(1).truncationMode(.middle); Spacer(); Text(Format.bytes(t.bytes)).monospacedDigit() }.font(.callout)
                }
            }
            if !contributor.deadAround.isEmpty {
                Text("Dead around them, not swept yet (\(Format.bytes(UInt64(contributor.deadAroundBytes))))").font(.subheadline.weight(.semibold))
                ForEach(contributor.deadAround, id: \.name) { t in
                    HStack { Text(TypeNames.short(t.name)).lineLimit(1).truncationMode(.middle); Spacer(); Text(Format.bytes(UInt64(t.bytes))).monospacedDigit() }.font(.callout)
                }
            }
            Text("Survivors keeping the most free space").font(.subheadline.weight(.semibold))
            if contributor.samples.isEmpty { Text("No objects").foregroundStyle(.secondary) }
            ForEach(contributor.samples, id: \.self) { ObjectLink(snapshot: snapshot, object: $0) }
        }
        .padding(10)
    }
}

extension FragmentationReport.Contributor {
    var regionCount: Int { regions.count }
}
