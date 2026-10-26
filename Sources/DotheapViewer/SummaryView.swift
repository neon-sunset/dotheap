import HeapCore
import SwiftUI

/// A block of memory in the summary treemap. Children are computed when the block is opened.
final class MemoryNode: Identifiable {
    let id: String
    let name: String
    let detail: String
    let bytes: UInt64
    let color: Color
    /// The object this block is, when it is one.
    let object: Int?
    private let makeChildren: (() throws -> [MemoryNode])?
    private var cached: [MemoryNode]?

    init(id: String, name: String, detail: String = "", bytes: UInt64, color: Color, object: Int? = nil, children: (() throws -> [MemoryNode])? = nil) {
        self.id = id
        self.name = name
        self.detail = detail
        self.bytes = bytes
        self.color = color
        self.object = object
        makeChildren = children
    }

    var canOpen: Bool { makeChildren != nil }

    /// Children largest first; the long tail folds into one "others" block.
    var children: [MemoryNode] {
        get throws {
            if let cached { return cached }
            var list = (try makeChildren?() ?? []).filter { $0.bytes > 0 }.sorted { $0.bytes > $1.bytes }
            if list.count > 300 {
                let rest = Array(list[300...])
                list = Array(list[..<300])
                let restBytes = rest.reduce(0) { $0 + $1.bytes }
                list.append(MemoryNode(id: id + "/others", name: "\(rest.count.formatted()) more", bytes: restBytes, color: .gray, children: { rest }))
            }
            cached = list
            return list
        }
    }
}

/// Builds the memory tree of a snapshot: the process at the top, then the Satori heap and native memory.
enum MemoryTree {
    static func root(_ s: HeapSnapshot) throws -> MemoryNode {
        let (parts, resident) = try s.memo("process memory") { try s.processMemory() }
        let total = parts.reduce(0) { $0 + $1.bytes }
        let detail = resident ? "resident memory (RSS) per smaps, by kind of memory in the dump"
            : s.heap.kind.gc == .none ? "pages with data in the dump; no managed heap (\(s.heap.kind.detection)), so native memory only"
            : s.smaps == nil
            ? "estimated from the dump: \(Format.mb(total - (try s.heapZeroPages)))–\(Format.mb(total)); zero-filled heap pages may be resident after a write or under a huge page; supply smaps to measure RSS"
            : "estimated from the dump, since smaps does not match it; smaps RSS is under Residency"
        return MemoryNode(id: "process", name: "Process", detail: detail, bytes: total, color: .gray) {
            try parts.map { part in
                part.name == s.heap.label ? try heap(s, bytes: resident ? part.bytes : s.heap.regions.reduce(0) { $0 + $1.committedBytes }) : try native(s, part)
            }
        }
    }

    static func heap(_ s: HeapSnapshot, bytes: UInt64) throws -> MemoryNode {
        let t = s.totalStats
        let headers = t.headers
        let detail = s.agreeingResidency != nil
            ? "resident per smaps" + (s.heapResidency.map { ", \(Format.mb($0.hugePages)) of it in huge pages" } ?? "") + "; the blocks inside split the \(Format.mb(t.committed)) of committed region memory"
            : "committed region memory" + (try s.heapResidency.map { "; resident per smaps \(Format.mb($0.resident)), \(Format.mb($0.hugePages)) of it in huge pages" }
                ?? "; resident at most \(Format.mb(try s.processMemoryEstimate().first { $0.name == s.heap.label }?.bytes ?? 0)): pages with data, and zero pages in 2 MB granules committed whole, which a huge page may back")
        return MemoryNode(id: "heap", name: s.heap.label, detail: detail, bytes: bytes, color: Palette.live) {
            [
                MemoryNode(id: "live", name: "Live objects", detail: "\(s.graph.liveCount.formatted()) objects; open to see what holds them",
                           bytes: t.live, color: Palette.live) {
                    try dominated(s, node: s.graph.rootNode, path: "live") { !isPending(s, $0) }
                },
                MemoryNode(id: "pending", name: "Pending finalization", detail: "\(s.graph.pendingCount.formatted()) objects",
                           bytes: t.pending, color: Palette.pending) {
                    try dominated(s, node: s.graph.rootNode, path: "pending") { isPending(s, $0) }
                },
                MemoryNode(id: "dead", name: "Dead, not swept", detail: "unreachable objects still in their regions", bytes: t.dead, color: Palette.dead) { try garbage(s) },
                MemoryNode(id: "free", name: "Free gaps", detail: s.heap.kind.gc == .satori ? "swept dead objects and unused allocation space (m_allocStart to m_allocEnd, including free regions)" : "swept dead objects and unused ends of allocation ranges", bytes: t.free, color: Palette.free) {
                    byGeneration(s, path: "free", color: Palette.free) { $0.free }
                },
                MemoryNode(id: "unused", name: "Committed, unused", detail: s.heap.kind.gc == .satori ? "committed above m_used, excluding GC storage" : "committed beyond allocated objects (heap_segment.allocated)", bytes: t.committedUnused, color: Palette.committedUnused) {
                    byGeneration(s, path: "unused", color: Palette.committedUnused) { $0.committedUnused }
                },
                MemoryNode(id: "gc-storage", name: "GC storage", detail: "committed space reserved for GC work chunks and finalization queue entries, including unused capacity", bytes: t.gcStorage, color: Palette.gcStorage),
                MemoryNode(id: "headers", name: "Region headers", detail: s.heap.kind.gc == .satori ? "mark bitmap and index of each region" : "space the GC leaves before the first object of each region", bytes: headers, color: Palette.headers),
                MemoryNode(id: "unparsed", name: "Unparsed", detail: s.heap.kind.gc == .satori ? "allocation ranges, zero tails and data whose purpose could not be established" : "allocation ranges and zero tails", bytes: t.unparsed, color: Palette.unparsed),
            ]
        }
    }

    static func byGeneration(_ s: HeapSnapshot, path: String, color: Color, _ value: @escaping (RegionStats) -> UInt64) -> [MemoryNode] {
        s.generationStats.map { g in
            MemoryNode(id: path + "/" + g.name, name: g.name, detail: Glossary.generation(g.name, gc: s.heap.kind.gc), bytes: value(g), color: Palette.generation(g.name).opacity(0.8))
        }
    }

    /// Live objects by the dominator tree, siblings of one type grouped into one block.
    /// A dominator tree node that only finalization keeps: an object pending finalization, or a finalization root source.
    static func isPending(_ s: HeapSnapshot, _ node: Int) -> Bool {
        let n = s.heap.count
        if node < n { return s.graph.pending[node] }
        guard node - n < s.graph.sources.count else { return false }
        switch s.graph.sources[node - n].kind {
        case .finalizerQueue, .finalizationRegistered: return true
        default: return false
        }
    }

    static func dominated(_ s: HeapSnapshot, node: Int, path: String, keep: (Int) -> Bool = { _ in true }) throws -> [MemoryNode] {
        let graph = s.graph
        return try s.dominatedGroups(of: node).filter { keep($0.nodes[0]) }.map { g in
            if g.nodes.count == 1 { return try objectNode(s, g.nodes[0], path: path) }
            return MemoryNode(id: path + "/" + g.name, name: "\(g.nodes.count.formatted()) × \(TypeNames.short(g.name))", detail: "retained together", bytes: g.retained,
                              color: Palette.hashed(g.name)) {
                try g.nodes.map { try objectNode(s, $0, path: path + "/" + g.name) }
            }
        }
        .filter { _ in graph.liveBytes > 0 }
    }

    static func objectNode(_ s: HeapSnapshot, _ node: Int, path: String) throws -> MemoryNode {
        let graph = s.graph
        let isObject = node < s.heap.count
        var name = isObject ? (s.staticsArrays.contains(node) ? "static fields" : TypeNames.short(s.typeName(ofObject: node))) : s.nodeName(node)
        if isObject, let label = try s.identifyingText(ofObject: node) { name = "\"\(label)\"  " + name }
        let hasChildren = !graph.dominatedChildrenUnsorted(of: node).isEmpty
        return MemoryNode(id: path + "/\(node)", name: name, detail: isObject ? Format.hex(s.heap.addresses[node]) + ", object itself " + Format.bytes(s.heap.size(node)) : "root source",
                          bytes: graph.retained[node], color: Palette.hashed(isObject ? s.typeName(ofObject: node) : "root"), object: isObject ? node : nil,
                          children: hasChildren ? { try dominated(s, node: node, path: path + "/\(node)") } : nil)
    }

    /// Dead objects by the type of the unreferenced dead object that holds them.
    static func garbage(_ s: HeapSnapshot) throws -> [MemoryNode] {
        let groups: RustArray<HeapSnapshot.GarbageGroup> = try s.memo("garbageByType") { try s.garbageByType() }
        let g = try s.garbage
        return groups.map { group in
            MemoryNode(id: "dead/\(group.typeId)", name: TypeNames.short(group.type), detail: "\(group.unreferenced.formatted()) unreferenced dead objects", bytes: group.retained,
                       color: Palette.dead.opacity(0.85)) {
                let shown = group.samples.map { i in
                    MemoryNode(id: "dead/\(group.typeId)/\(i)", name: TypeNames.short(group.type), detail: Format.hex(s.heap.addresses[i]), bytes: g.retained(i), color: Palette.dead, object: i)
                }
                let rest = group.retained - shown.reduce(0) { $0 + $1.bytes }
                return shown + (rest > 0 ? [MemoryNode(id: "dead/\(group.typeId)/rest", name: "\((group.unreferenced - shown.count).formatted()) smaller", bytes: rest, color: .gray)] : [])
            }
        }
    }

    /// Free memory of the malloc heaps (`bytes` of each heap): arenas whose threads exited, arenas a thread uses, and
    /// the main arena (groups 0, 1 and 2), each by heap; `releases` tells what gives a group's memory back.
    static func mallocFree(_ s: HeapSnapshot, id: String, color: Color, releases: (_ group: Int, _ heaps: [NativeMemory.MallocHeap]) throws -> String,
                           _ bytes: (NativeMemory.MallocHeap) -> UInt64) throws -> [MemoryNode] {
        let groups = try Dictionary(grouping: s.native.mallocHeaps) { h -> Int in
            h.mainArena ? 2 : try NativeMemory.attachedThreads(dump: s.dump, arena: h.arena) == 0 ? 0 : 1
        }
        let names = ["Arenas of threads that exited", "Arenas that threads use", "Main arena"]
        let colors = [Palette.dead.opacity(0.8), color, color.opacity(0.7)]
        return try groups.keys.sorted().compactMap { k in
            let heaps = groups[k]!.filter { bytes($0) > 0 }.sorted { (bytes($0), $1.start) > (bytes($1), $0.start) }
            let total = heaps.reduce(0) { $0 + bytes($1) }
            guard total > 0 else { return nil }
            let shown = heaps.map { h in
                MemoryNode(id: id + "/" + Format.hex(h.start), name: "heap " + Format.hex(h.start),
                           detail: (h.mainArena ? "main arena" : "arena " + Format.hex(h.arena)) + ", \(Format.mb(h.inUseBytes)) in use", bytes: bytes(h), color: colors[k])
            }
            let arenas = Set(heaps.map(\.arena)).count
            let counts = k == 2 ? "" : "\(arenas) \(arenas == 1 ? "arena" : "arenas"), \(heaps.count) \(heaps.count == 1 ? "heap" : "heaps"); "
            return MemoryNode(id: id + "/\(k)", name: names[k], detail: counts + (try releases(k, heaps)),
                              bytes: total, color: colors[k]) { shown }
        }
    }

    /// A native memory kind, split by mapping detail (library, thread, ...) when that tells something.
    static func native(_ s: HeapSnapshot, _ part: HeapSnapshot.MemoryPart) throws -> MemoryNode {
        let mappings = s.native.mappings.filter { $0.category == part.name }
        let byDetail = Dictionary(grouping: mappings) { $0.detail.isEmpty ? "(no detail)" : $0.detail }
        let malloc = part.name == "malloc heap" ? s.native.mallocHeaps : RustArray<NativeMemory.MallocHeap>()
        let resident = s.agreeingResidency != nil
        func bytes(_ list: [NativeMemory.Mapping]) -> UInt64 { list.reduce(0) { $0 + (s.resident($1) ?? $1.nonZeroBytes) } }
        return MemoryNode(id: "native/" + part.name, name: part.name, detail: resident ? "resident per smaps" : "pages that are not all zero", bytes: part.bytes, color: Palette.native(part.name),
                          children: malloc.isEmpty && byDetail.count <= 1 ? nil : {
            if !malloc.isEmpty {
                let inUse = malloc.reduce(0) { $0 + $1.inUseBytes }, free = malloc.reduce(0) { $0 + $1.freeResident }
                let top = malloc.reduce(0) { $0 + $1.topResident }, headers = malloc.reduce(0) { $0 + $1.headerBytes }
                let trim = malloc.reduce(0) { $0 + $1.trimReleasable }
                let arenas = Set(malloc.map(\.arena)).count
                return [
                    MemoryNode(id: "malloc/inuse", name: "In use", detail: "allocated chunks", bytes: inUse, color: Palette.live.opacity(0.7), children: { try mallocInUse(s) }),
                    MemoryNode(id: "malloc/free", name: "Free chunks", detail: "freed memory glibc keeps for reuse, in pages that still hold data; malloc_trim(0) would return about \(Format.mb(trim)) to the OS",
                               bytes: free, color: Palette.free) {
                        try mallocFree(s, id: "malloc/free", color: Palette.free, releases: { k, heaps in
                            // The main arena's trimReleasable also counts its top chunk.
                            k == 2 ? "malloc_trim(0) releases the whole pages inside its free chunks"
                                : "malloc_trim(0) would release about \(Format.mb(heaps.reduce(0) { $0 + $1.trimReleasable })) of it: the whole pages inside the free chunks"
                        }) { $0.freeResident }
                    },
                    MemoryNode(id: "malloc/top", name: "Top chunks and arena headers", detail: "Free ends of \(malloc.count) heaps, counted in pages with data, plus \(Format.bytes(headers)) of heap_info and malloc_state headers for \(arenas) arenas. Freeing memory at a heap's end adds to its top chunk. The trim threshold controls when glibc can release it; malloc_trim shrinks only the main arena's top chunk.",
                               bytes: top + headers, color: Palette.committedUnused) {
                        try mallocFree(s, id: "malloc/top", color: Palette.committedUnused, releases: { k, _ in
                            let threshold = try s.mallocParameters.map { " (\(Format.mb($0.trimThreshold)) now)" } ?? ""
                            return [
                                "no call releases it, and malloc_trim(0) does not shrink it; a new thread that takes one of these arenas allocates from it first",
                                "a heap gives it back at a free() of 64 KB or more into its arena, once its top chunk reaches the trim threshold\(threshold); malloc_trim(0) does not shrink it",
                                "malloc_trim(0) releases it",
                            ][k]
                        }) { $0.topResident }
                            + [MemoryNode(id: "malloc/top/headers", name: "Arena headers", detail: "heap_info and malloc_state of \(arenas) arenas", bytes: headers, color: Palette.headers)]
                    },
                ]
            }
            if part.name == "thread stack" {
                // One block per thread, named by its managed name, with how deep it has called and where it is now.
                let stacks = try s.threadStacks
                return byDetail.map { detail, list in
                    let bytes = bytes(list)
                    if detail.hasPrefix("thread "), let tid = Int32(detail.dropFirst(7)), let t = stacks.thread(tid) {
                        return MemoryNode(id: "native/stack/\(tid)", name: stacks.label(tid), detail: stacks.summary(t), bytes: bytes, color: Palette.hashed(detail))
                    }
                    return MemoryNode(id: "native/stack/" + detail, name: detail, detail: "\(list.count) mappings", bytes: bytes, color: Palette.hashed(detail))
                }
            }
            return byDetail.map { detail, list in
                MemoryNode(id: "native/\(part.name)/\(detail)", name: detail, detail: "\(list.count) mappings", bytes: bytes(list), color: Palette.hashed(detail))
            }
        })
    }
}

extension MemoryTree {
    /// Live managed objects and malloc chunks in use under what keeps them alive alone (the ownership graph).
    static func owners(_ s: HeapSnapshot) throws -> MemoryNode {
        let o = try s.ownership, root = o.rootNode
        return MemoryNode(id: "owners", name: "Live memory by owner",
                          detail: "\(Format.mb(o.retainedManaged[root])) managed, \(Format.mb(o.retainedNative[root])) native; each block is what an object, chunk or root keeps alive alone",
                          bytes: o.retained(root), color: .gray) { try ownerGroups(s, node: root, path: "owners") }
    }

    static func ownerGroups(_ s: HeapSnapshot, node: Int, path: String) throws -> [MemoryNode] {
        try s.ownershipGroups(of: node).map { g in
            if g.nodes.count == 1 { return try ownerNode(s, g.nodes[0], path: path) }
            return MemoryNode(id: path + "/" + g.name, name: "\(g.nodes.count.formatted()) × \(TypeNames.short(g.name))",
                              detail: "\(Format.mb(g.managed)) managed, \(Format.mb(g.native)) native", bytes: g.total, color: try ownerColor(s, g.nodes[0])) {
                try g.nodes.map { try ownerNode(s, $0, path: path + "/" + g.name) }
            }
        }
    }

    static func ownerNode(_ s: HeapSnapshot, _ v: Int, path: String) throws -> MemoryNode {
        let o = try s.ownership
        var name = try TypeNames.short(s.ownershipName(v)), object: Int?, address = ""
        switch o.node(v) {
        case let .object(i):
            object = i
            address = Format.hex(s.heap.addresses[i]) + ", "
            if let label = try s.identifyingText(ofObject: i) { name = "\"\(label)\"  " + name }
        case let .chunk(c): address = try Format.hex(s.nativeGraph.chunks[c].userStart) + ", "
        default: break
        }
        let hasChildren = !o.children(of: v).isEmpty
        return MemoryNode(id: path + "/\(v)", name: name, detail: address + "\(Format.mb(o.retainedManaged[v])) managed, \(Format.mb(o.retainedNative[v])) native",
                          bytes: o.retained(v), color: try ownerColor(s, v), object: object,
                          children: hasChildren ? { try ownerGroups(s, node: v, path: path + "/\(v)") } : nil)
    }

    static func ownerColor(_ s: HeapSnapshot, _ v: Int) throws -> Color {
        switch try s.ownership.node(v) {
        case let .object(i): Palette.hashed(s.typeName(ofObject: i))
        case let .chunk(c): try Palette.native("malloc heap").opacity(s.nativeGraph.className(c) == nil ? 0.7 : 1)
        default: Palette.native("native libraries")
        }
    }

    /// Chunks in use by what holds them: the library of the nearest C++ object, managed owners, or nothing.
    static func mallocInUse(_ s: HeapSnapshot) throws -> [MemoryNode] {
        let g = try s.nativeGraph
        let groups = try s.chunkGroups
        let cached = g.cachedBytes
        func node(_ id: String, _ name: String, _ detail: String, _ list: [NativeGraph.ChunkGroup], _ color: Color) -> MemoryNode {
            MemoryNode(id: "malloc/" + id, name: name, detail: detail, bytes: list.reduce(0) { $0 + $1.bytes }, color: color) {
                list.map { grp in MemoryNode(id: "malloc/" + id + "/" + grp.id, name: grp.label, detail: "\(grp.kind.title), \(grp.chunks.count) chunks, held by " + grp.reach.holder, bytes: grp.bytes, color: color.opacity(0.75)) }
            }
        }
        var nodes: [MemoryNode] = []
        let byLibrary = Dictionary(grouping: groups.filter { $0.library != nil }) { $0.library! }
        for (lib, list) in byLibrary { nodes.append(node("lib/" + lib, lib, "identified types and reference sources in " + lib, list, Palette.hashed(lib))) }
        let rest = groups.filter { $0.library == nil }
        nodes.append(node("managed", "Held by managed objects", "chunks only managed objects point to: live, pending finalization or dead", rest.filter { $0.reach == .liveManaged || $0.reach == .pendingManaged || $0.reach == .deadManaged }, Palette.live))
        nodes.append(node("unreferenced", "Unreferenced", "leak candidates: no pointer to them anywhere in the dump", rest.filter { $0.reach == .unreferenced }, Color(red: 0.90, green: 0.30, blue: 0.35)))
        nodes.append(node("native", "Other native", "reachable from native roots, no C++ object found", rest.filter { $0.reach == .nativeRoots }, Palette.native("native libraries")))
        if cached > 0 { nodes.append(MemoryNode(id: "malloc/cached", name: "Freed, cached by glibc", detail: "tcache and fastbin chunks", bytes: cached, color: Palette.free)) }
        return nodes
    }
}

/// Squarified treemap layout (Bruls, Huizing, van Wijk): rectangles close to squares, largest first.
enum Treemap {
    static func layout(_ values: [Double], in rect: CGRect) -> [CGRect] {
        let total = values.reduce(0, +)
        guard total > 0, rect.width > 0, rect.height > 0 else { return values.map { _ in .zero } }
        let scale = Double(rect.width * rect.height) / total
        let areas = values.map { $0 * scale }
        var result = [CGRect](repeating: .zero, count: values.count)
        var free = rect
        var row: [Int] = []
        var i = 0

        func worst(_ row: [Int], _ side: Double) -> Double {
            let sum = row.reduce(0) { $0 + areas[$1] }
            guard sum > 0 else { return .infinity }
            let maxA = row.map { areas[$0] }.max()!, minA = row.map { areas[$0] }.min()!
            return max(side * side * maxA / (sum * sum), sum * sum / (side * side * max(minA, 1e-9)))
        }

        func place(_ row: [Int]) {
            let sum = row.reduce(0) { $0 + areas[$1] }
            if free.width >= free.height {
                let w = sum / Double(free.height)
                var y = free.minY
                for k in row {
                    let h = areas[k] / w
                    result[k] = CGRect(x: free.minX, y: y, width: w, height: h)
                    y += h
                }
                free = CGRect(x: free.minX + w, y: free.minY, width: max(0, free.width - w), height: free.height)
            } else {
                let h = sum / Double(free.width)
                var x = free.minX
                for k in row {
                    let w = areas[k] / h
                    result[k] = CGRect(x: x, y: free.minY, width: w, height: h)
                    x += w
                }
                free = CGRect(x: free.minX, y: free.minY + h, width: free.width, height: max(0, free.height - h))
            }
        }

        while i < areas.count {
            let side = Double(min(free.width, free.height))
            if row.isEmpty || worst(row + [i], side) <= worst(row, side) {
                row.append(i); i += 1
            } else {
                place(row); row = []
            }
        }
        if !row.isEmpty { place(row) }
        return result
    }
}

enum SummaryMode: String, CaseIterable, Identifiable {
    case kind = "Kind of memory", owner = "Owner (managed and native)", residency = "Residency"
    var id: String { rawValue }
}

/// The whole process as nested blocks: area is bytes; click a block to open it, the path at the top leads back. Above
/// it, the largest findings, each a link to the page that shows more.
struct SummaryView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var hover: MemoryNode?
    private var kindRoot: MemoryNode? { session.memoryTree }
    private var ownerRoot: MemoryNode? { session.ownerTree }

    private var mode: SummaryMode { session.summaryMode }

    /// The open blocks, from the session's block ids (Back and Forward move through them).
    private func path(_ root: MemoryNode) throws -> [MemoryNode] {
        var nodes: [MemoryNode] = [], node = root
        for id in session.summaryPath {
            guard let next = try node.children.first(where: { $0.id == id }) else { break }
            nodes.append(next)
            node = next
        }
        return nodes
    }

    private func setPath(_ nodes: [MemoryNode]) { session.summaryPath = nodes.map(\.id) }

    var body: some View {
        @Bindable var session = session
        VStack(alignment: .leading, spacing: 8) {
            FindingsList(findings: session.summaryFindings)
            Picker("Show", selection: Binding(get: { session.summaryMode }, set: { session.summaryMode = $0; session.summaryPath = []; hover = nil })) {
                ForEach(SummaryMode.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented).labelsHidden().fixedSize()
            switch mode {
            case .kind:
                if let kindRoot { map(kindRoot) } else { ProgressView("Counting memory by kind").frame(maxWidth: .infinity, maxHeight: .infinity) }
            case .owner:
                if let ownerRoot { map(ownerRoot) } else { ProgressView("Building the ownership graph").frame(maxWidth: .infinity, maxHeight: .infinity) }
            case .residency: ResidencyView(snapshot: snapshot)
            }
        }
        .padding(16)
        .task(id: mode) {
            let s = snapshot
            do {
                switch mode {
                case .kind where kindRoot == nil:
                    let root = try await Task.detached { try MemoryTree.root(s) }.value
                    if session.snapshot === s { session.memoryTree = root }
                case .owner where ownerRoot == nil:
                    let root = try await Task.detached { try MemoryTree.owners(s) }.value
                    if session.snapshot === s { session.ownerTree = root }
                default: break
                }
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }

    @ViewBuilder
    private func map(_ root: MemoryNode) -> some View {
        ReadView {
            let path = try path(root)
            let current = path.last ?? root
            let children = try current.children
            VStack(alignment: .leading, spacing: 8) {
                breadcrumb(root, path)
                HStack(alignment: .firstTextBaseline) {
                    Text(current.name).font(.title2.weight(.semibold))
                    Text(Format.mb(current.bytes)).font(.title2.monospacedDigit()).foregroundStyle(.secondary)
                    if !current.detail.isEmpty { Text(current.detail).foregroundStyle(.secondary).lineLimit(2) }
                    Spacer()
                }
                TreemapCanvas(nodes: children, total: root.bytes, hover: $hover) { node in open(node, path) }
                    .frame(minHeight: 300)
                // Blocks with a long explanation get it in full below the map.
                let explained = children.filter { $0.detail.count > 60 }.prefix(6)
                if !explained.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(Array(explained), id: \.id) { n in
                            (Text(n.name + ": ").fontWeight(.semibold) + Text(n.detail).foregroundColor(.secondary))
                                .font(.callout).lineLimit(4).frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                }
                statusLine(current: current, root: root)
            }
            .onAppear {
                // -summaryPath "Satori heap>Live objects" opens a level at startup (block names or ids).
                guard session.summaryPath.isEmpty, let spec = UserDefaults.standard.string(forKey: "summaryPath") else { return }
                do {
                    var node = root, nodes: [MemoryNode] = []
                    for part in spec.split(separator: ">").map({ $0.trimmingCharacters(in: .whitespaces) }) {
                        guard let next = try node.children.first(where: { $0.id == part || $0.name == part || $0.name.hasSuffix(part) }) else { break }
                        nodes.append(next)
                        node = next
                    }
                    setPath(nodes)
                } catch { session.error = analysisErrorMessage(error) }
            }
        }
    }

    private func open(_ node: MemoryNode, _ path: [MemoryNode]) {
        do {
            if try node.canOpen && !node.children.isEmpty {
                setPath(path + [node])
                hover = nil
            } else if let object = node.object {
                session.select(object: object)
            }
        } catch { session.error = analysisErrorMessage(error) }
    }

    /// A name for the path: without namespaces, and at most 40 characters with the middle left out.
    static func crumb(_ name: String) -> String {
        let short = TypeNames.withoutNamespaces(name)
        return short.count <= 40 ? short : short.prefix(19) + "…" + short.suffix(20)
    }

    /// The levels above the current one on one line: names without namespaces, and the middle levels in a menu when
    /// the path is long.
    private func breadcrumb(_ root: MemoryNode, _ path: [MemoryNode]) -> some View {
        let shown = 3
        let hidden = path.count > shown + 1 ? Array(path.indices.dropLast(shown)) : []
        return HStack(spacing: 4) {
            Button(root.name) { setPath([]) }.buttonStyle(.link).fixedSize()
            if !hidden.isEmpty {
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary)
                Menu("…") {
                    ForEach(hidden, id: \.self) { k in Button(Self.crumb(path[k].name)) { setPath(Array(path.prefix(k + 1))) } }
                }
                .menuStyle(.borderlessButton).fixedSize()
            }
            ForEach(path.indices.filter { !hidden.contains($0) }, id: \.self) { k in
                Image(systemName: "chevron.right").font(.caption).foregroundStyle(.secondary)
                Button(Self.crumb(path[k].name)) { setPath(Array(path.prefix(k + 1))) }.buttonStyle(.link).fixedSize().help(path[k].name)
            }
            Spacer()
            Button { setPath(Array(path.dropLast())) } label: { Label("Up", systemImage: "arrow.up") }
                .keyboardShortcut(.upArrow, modifiers: .command).disabled(path.isEmpty)
        }
    }

    private func statusLine(current: MemoryNode, root: MemoryNode) -> some View {
        HStack {
            if let h = hover {
                Text("\(h.name): \(Format.mb(h.bytes)), \(Format.percent(Double(h.bytes), Double(current.bytes))) of \(current.name), \(Format.percent(Double(h.bytes), Double(root.bytes))) of the process")
                    .font(.callout.monospacedDigit())
                if !h.detail.isEmpty { Text("·  " + h.detail).font(.callout).foregroundStyle(.secondary) }
                Text(h.canOpen ? "·  click to open" : h.object != nil ? "·  click to inspect" : "").font(.callout).foregroundStyle(.secondary)
            } else {
                Text("Area is bytes. Click a block to open it; objects open in the inspector.").font(.callout).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .lineLimit(1)
    }
}

/// The largest summary findings; each opens the page that shows more.
struct FindingsList: View {
    @Environment(Session.self) private var session
    let findings: RustArray<HeapSnapshot.SummaryFinding>?
    @State private var showAll = false
    static let shown = 4

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("Findings").font(.headline)
                Spacer()
                if let findings, findings.count > Self.shown {
                    Button(showAll ? "Show fewer" : "Show all \(findings.count)") { showAll.toggle() }.buttonStyle(.link)
                }
            }
            if let findings {
                if findings.isEmpty { Text("Nothing found.").font(.callout).foregroundStyle(.secondary) }
                ForEach(Array(findings.prefix(showAll ? findings.count : Self.shown).enumerated()), id: \.offset) { _, f in
                    Button { session.show(see: f.see) } label: {
                        HStack(alignment: .firstTextBaseline, spacing: 6) {
                            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Palette.dead)
                            Text(f.text).lineLimit(1).truncationMode(.tail).help(f.text)
                            Spacer(minLength: 0)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain).font(.callout)
                }
            } else {
                HStack(spacing: 6) { ProgressView().controlSize(.small); Text("Preparing findings").font(.callout).foregroundStyle(.secondary) }
            }
        }
    }
}

struct TreemapCanvas: View {
    let nodes: [MemoryNode]
    let total: UInt64
    @Binding var hover: MemoryNode?
    let open: (MemoryNode) -> Void

    var body: some View {
        GeometryReader { geo in
            let rects = Treemap.layout(nodes.map { Double($0.bytes) }, in: CGRect(origin: .zero, size: geo.size))
            Canvas { ctx, _ in
                for (k, node) in nodes.enumerated() {
                    let r = rects[k].insetBy(dx: 1, dy: 1)
                    guard r.width > 0.5, r.height > 0.5 else { continue }
                    let highlighted = hover?.id == node.id
                    ctx.fill(Path(roundedRect: r, cornerRadius: 3), with: .color(node.color.opacity(highlighted ? 1 : 0.82)))
                    if highlighted { ctx.stroke(Path(roundedRect: r, cornerRadius: 3), with: .color(.white.opacity(0.9)), lineWidth: 2) }
                    guard r.width > 60, r.height > 28 else { continue }
                    // One line per text: the name is cut to the width (about 7 points per character).
                    let fontSize: CGFloat = r.width > 180 && r.height > 60 ? 13 : 11
                    let maxChars = max(3, Int((r.width - 12) / (fontSize * 0.62)))
                    let name = node.name.count > maxChars ? String(node.name.prefix(maxChars - 1)) + "…" : node.name
                    let label = Text(name).font(.system(size: fontSize, weight: .semibold)).foregroundColor(.black.opacity(0.85))
                    let size = Text(Format.mb(node.bytes)).font(.system(size: 11).monospacedDigit()).foregroundColor(.black.opacity(0.7))
                    let inner = r.insetBy(dx: 6, dy: 4)
                    ctx.draw(label, at: CGPoint(x: inner.minX, y: inner.minY), anchor: .topLeading)
                    if r.height > 44 { ctx.draw(size, at: CGPoint(x: inner.minX, y: inner.minY + fontSize + 5), anchor: .topLeading) }
                }
            }
            .contentShape(Rectangle())
            .onContinuousHover { phase in
                if case let .active(p) = phase, let k = rects.firstIndex(where: { $0.contains(p) }) { hover = nodes[k] } else { hover = nil }
            }
            .onTapGesture { p in
                if let k = rects.firstIndex(where: { $0.contains(p) }) { open(nodes[k]) }
            }
        }
    }
}
