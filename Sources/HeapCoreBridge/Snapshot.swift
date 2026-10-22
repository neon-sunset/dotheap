@_exported import RustHeapCore
import Foundation

public final class HeapSnapshot: @unchecked Sendable {
    let bridge: RustHeapCore.Snapshot
    let info: SnapshotInfo
    public let heap: ManagedHeap
    public let graph: HeapGraph
    public let dump: CoreDump
    public let staticsArrays: Set<Int>
    public let typeStats: RustArray<TypeStats>
    public let residency: Residency?
    private let memoLock = NSLock()
    private var memos: [String: Any] = [:]

    public init(path: String, dllFolders: [String] = [], smapsPath: String? = nil,
                options: AnalysisOptions = AnalysisOptions(nativeMemoryRoots: false),
                cache: CacheChoice = .automatic, progress: ((String, Double) -> Void)? = nil) throws {
        let directory: String?
        if case .folder(let path) = cache { directory = path } else { directory = nil }
        bridge = try RustHeapCore.Snapshot.open(path: path, dllFolders: dllFolders, smapsPath: smapsPath,
            options: options, rebuild: cache == .rebuild, cacheDirectory: directory, progress: progress.map { ProgressHandler(update: $0) })
        info = try bridge.info()
        heap = ManagedHeap(bridge: bridge, info: info)
        graph = HeapGraph(bridge: bridge, info: info)
        dump = CoreDump(bridge: bridge, info: info)
        staticsArrays = Set(info.staticsArrays)
        typeStats = bridge.typeStats()
        residency = bridge.residency()
    }

    public func memoized<T>(_ key: String) -> T? {
        memoLock.lock()
        defer { memoLock.unlock() }
        return memos[key] as? T
    }

    public func memo<T>(_ key: String, _ compute: () throws -> T) rethrows -> T {
        if let value: T = memoized(key) { return value }
        let value = try compute()
        memoLock.lock()
        memos[key] = value
        memoLock.unlock()
        return value
    }

    public var native: NativeMemory { NativeMemory(value: bridge.native(), categories: info.nativeCategories) }
    public var fromCache: Bool { info.fromCache }
    public var cacheDirectory: String? { info.cacheDirectory }
    public var smapsPath: String? { info.smapsPath }
    public var smaps: RustArray<SmapsMapping>? { bridge.smaps() }
    public var residencyCategories: RustArray<ResidencyCategory> { info.residencyCategories }
    public var agreeingResidency: Residency? { residency.flatMap { $0.agrees ? $0 : nil } }
    public var versions: Versions { info.versions }
    public var runtime: RuntimeInfo? { info.runtimePath.map { RuntimeInfo(path: $0, version: info.runtimeVersion, buildId: info.versions.runtimeBuildId) } }
    public var totalStats: RegionStats { info.total }
    public var generationStats: RustArray<RegionStats> { info.generations }
    public var queueStats: RustArray<RegionStats> { info.queues }
    public var sizeClassStats: RustArray<RegionStats> { info.sizeClasses }
    public var occupancyStats: RustArray<OccupancyHistogram> { info.occupancy }
    public var fragmentationShare: Double { info.fragmentationShare }
    public var heapResidency: HeapResidency? { bridge.heapResidency() }
    public var heapZeroPages: UInt64 { get throws { try bridge.heapZeroPages() } }

    public var nativeGraph: NativeGraph {
        get throws { try memo("native graph wrapper") { NativeGraph(try bridge.nativeGraph()) } }
    }
    public var ownership: OwnershipGraph {
        get throws { try memo("ownership wrapper") { OwnershipGraph(try bridge.ownership()) } }
    }
    public var garbage: GarbageGraph {
        get throws { try memo("garbage wrapper") { GarbageGraph(try bridge.garbage()) } }
    }
    public var threadStacks: ThreadStacks {
        get throws { try memo("thread stacks wrapper") { ThreadStacks(try bridge.threadStacks()) } }
    }
    public var taskGraphs: TaskGraphs { get throws { try memo("task graphs") { try bridge.taskGraphs().graphs } } }
    public var satoriRegionKinds: RustArray<String> { info.regionKinds }
    public var chunkGroups: RustArray<NativeGraph.ChunkGroup> { get throws { try memo("chunk groups") { try bridge.chunkGroups() } } }
    public var mallocParameters: MallocParameters? { get throws { try memo("malloc parameters") { try bridge.mallocParameters() } } }

    public func typeName(ofObject object: Int) -> String { typeStats[Int(heap.typeIDs[object])].name }
    public func typeID(named name: String) -> UInt32? { bridge.typeId(name: name) }
    public func instances(ofType type: UInt32, limit: Int) -> RustArray<Int> { bridge.instances(typeId: type, limit: limit) }
    public func referrerTypes(ofType type: UInt32) throws -> RustArray<TypeGroup> { try bridge.referrerTypes(typeId: type) }
    public func dominatorTypes(ofType type: UInt32) throws -> RustArray<TypeGroup> { try bridge.dominatorTypes(typeId: type) }
    public func handleGroups() -> RustArray<HandleGroup> { info.handleGroups }
    public func nodeName(_ node: Int) -> String {
        if node == graph.rootNode { return "roots" }
        if node >= heap.count { return info.sourceNames[node - heap.count] }
        let name = typeName(ofObject: node)
        return staticsArrays.contains(node) ? "static fields (\(name))" : name
    }
    public func describe(source: Int) -> String { info.sourceNames[source] }
    public func dominatedGroups(of node: Int) throws -> RustArray<DominatorGroup> {
        try bridge.dominatedGroups(node: node)
    }
    public func ownershipName(_ node: Int) throws -> String { try bridge.ownershipName(node: node) }
    public func ownershipGroups(of node: Int) throws -> RustArray<OwnershipGroup> {
        try bridge.ownershipGroups(node: node)
    }
    public func identifyingText(ofObject object: Int) throws -> String? { try bridge.identifyingText(object: object) }
    public func referenceName(from parent: Int, to child: Int) throws -> String? { try bridge.referenceName(parent: parent, child: child) }
    public func fieldValues(ofObject object: Int) throws -> RustArray<FieldValue> { try bridge.fieldValues(object: object) }
    public func stateMachineFields(ofBox object: Int) throws -> [(field: FieldValue, objects: RustArray<Int>)] {
        try bridge.stateMachineFields(object: object).map { ($0.value, $0.retainedBy) }
    }
    public func stackHolderText(_ object: Int) throws -> String? { try bridge.stackHolderText(object: object) }
    public func socketText(_ object: Int) throws -> String? { try bridge.socketText(object: object) }
    public func pathToRoot(_ object: Int, avoiding types: Set<String>) throws -> RustHeapCore.RootPath? {
        try bridge.pathAvoiding(object: object, types: Array(types))
    }
    public func granuleMap() -> RustArray<RustArray<Granule>> { bridge.granuleMap() }
    public func regionStrip(_ region: Int, buckets: Int) -> (live: RustArray<Double>, pending: RustArray<Double>, dead: RustArray<Double>) {
        precondition(region >= 0 && region < heap.regions.count && (1...65536).contains(buckets))
        let strip = bridge.regionStrip(region: region, buckets: buckets)!
        return (strip.live, strip.pending, strip.dead)
    }
    public func regionTypes(_ region: Int) throws -> RustArray<TypeGroup> { try bridge.regionTypes(region: region) }
    public func processMemory() throws -> (parts: RustArray<MemoryPart>, resident: Bool) {
        let memory = try bridge.processMemory()
        return (memory.parts, memory.resident)
    }
    public func processMemoryEstimate() throws -> RustArray<MemoryPart> { try bridge.memoryEstimate() }
    public func resident(_ mapping: NativeMemory.Mapping) -> UInt64? { agreeingResidency?.rss[mapping.start] }
    public func resident(from start: UInt64, to end: UInt64) -> UInt64? { bridge.residentRange(start: start, end: end) }
    public func verificationCheck() throws -> CheckReport { try bridge.verificationCheck() }
    public func heapVerification() throws -> HeapVerification { try bridge.heapVerification() }
    public func allChecks() throws -> RustArray<CheckReport> { try bridge.allChecks() }
    public func summaryFindings() throws -> RustArray<SummaryFinding> { try bridge.summaryFindings() }
    public func threadStates() throws -> RustArray<ThreadState> { try bridge.threadStates() }
    public func threadGroups() throws -> RustArray<RustHeapCore.ThreadGroup> { try bridge.threadGroups() }
    public func mergedStacks() throws -> RustArray<StackNode> { try bridge.mergedStacks() }
    public func queuedWorkItems() throws -> RustArray<QueuedWorkItem> { try bridge.queuedWorkItems() }
    public func blockedThreadsCheck() throws -> CheckReport { try bridge.blockedThreadsCheck() }
    public func lockCheck() throws -> CheckReport { try bridge.lockCheck() }
    public func timers() throws -> Timers { try bridge.timers() }
    public func timerPlaces(_ timers: [TimerInfo]) throws -> RustArray<String> { try bridge.timerPlaces(timers: timers) }
    public func network() throws -> Network { try bridge.network() }
    public func files() throws -> Files { try bridge.files() }
    public func fragmentation() throws -> FragmentationReport { try bridge.fragmentation() }
    public func duplicateStrings() throws -> RustArray<DuplicateString> { try bridge.duplicateStrings(limit: 1000) }
    public func collectionCapacity() throws -> RustArray<CollectionCapacity> { try bridge.collectionCapacity() }
    public func mostlyZeroArrays() throws -> RustArray<ZeroArrays> { try bridge.mostlyZeroArrays() }
    public func boxedValues() throws -> RustArray<TypeCount> { try bridge.boxedValues() }
    public func finalizableObjects() throws -> RustArray<Finalizable> { try bridge.finalizableObjects() }
    public func garbageByType() throws -> RustArray<GarbageGroup> { try bridge.garbageByType() }
    public func search(_ query: String, limitPerKind: Int = 100) throws -> RustArray<SearchResult> { try bridge.search(query: query, limitPerKind: limitPerKind) }
    public func inspectObject(_ object: Int) throws -> ObjectDetail { try bridge.inspectObject(object: object) }
    public func downloadRuntimeSymbols(automatic: Bool, isCancelled: @escaping () -> Bool, progress: @escaping (String, Double) -> Void) -> RustArray<SymbolDownload> {
        bridge.downloadSymbols(automatic: automatic, progress: ProgressHandler(update: progress, cancelled: isCancelled))
    }
}

final class ProgressHandler: RustHeapCore.Progress, @unchecked Sendable {
    let handler: (String, Double) -> Void
    let cancellation: () -> Bool

    init(update: @escaping (String, Double) -> Void, cancelled: @escaping () -> Bool = { false }) {
        handler = update
        cancellation = cancelled
    }

    func update(phase: String, fraction: Double) { handler(phase, fraction) }
    func cancelled() -> Bool { cancellation() }
}

public struct RuntimeInfo: Sendable {
    public let path: String
    public let version: String?
    public let buildId: String?
}
