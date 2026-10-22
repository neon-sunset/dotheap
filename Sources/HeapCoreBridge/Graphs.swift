import RustHeapCore

public typealias Column<Element> = RustArray<Element>

public struct ManagedHeap: Sendable {
    private let bridge: RustHeapCore.Snapshot
    public let kind: HeapKind
    public let pages: RustArray<Page>
    public let regions: RustArray<Region>
    public let types: RustArray<MethodTableInfo>
    public let addresses: Column<UInt64>
    public let typeIDs: Column<UInt32>
    private let sizes: Column<UInt32>
    private let largeSizes: RustMap<Int, UInt64>
    public let count: Int
    public static let granule: UInt64 = 1 << 21

    init(bridge: RustHeapCore.Snapshot, info: SnapshotInfo) {
        self.bridge = bridge
        kind = bridge.kind()
        pages = bridge.pages()
        regions = bridge.regions()
        types = bridge.types()
        addresses = bridge.addresses()
        typeIDs = bridge.typeIds()
        sizes = bridge.sizes()
        largeSizes = bridge.largeSizes()
        count = info.objectCount
    }

    func objectInfo(_ index: Int) -> ObjectInfo {
        precondition(index >= 0 && index < count, "Object index is out of range")
        return bridge.objectInfo(index: index)!
    }

    public func size(_ index: Int) -> UInt64 {
        let size = sizes[index]
        return size == .max ? largeSizes[index]! : UInt64(size)
    }
    public func methodTable(_ index: Int) -> MethodTableInfo { types[Int(typeIDs[index])] }
    public func region(ofObject index: Int) -> Int {
        precondition((0..<count).contains(index))
        var low = 0
        var high = regions.count
        while low < high {
            let middle = low + (high - low) / 2
            if regions[middle].firstObject <= index { low = middle + 1 } else { high = middle }
        }
        return max(0, low - 1)
    }
    public func objects(inRegion region: Int) -> RustArray<ObjectInfo> {
        precondition(regions.indices.contains(region), "Region index is out of range")
        return bridge.regionObjects(region: region)!
    }
    public func region(containing address: UInt64) -> Int? { bridge.regionContaining(address: address) }
    public func object(containing address: UInt64) -> Int? { bridge.objectContaining(address: address) }
    public var label: String { kind.gc == .satori ? "Satori heap" : "GC heap" }
}

public struct CoreDump: Sendable {
    let bridge: RustHeapCore.Snapshot
    public let platform: Platform
    public let threads: RustArray<Thread>

    init(bridge: RustHeapCore.Snapshot, info: SnapshotInfo) {
        self.bridge = bridge
        platform = info.platform
        threads = bridge.threads()
    }

    public var readError: String? { bridge.readError() }
    public func bytes(_ address: UInt64, _ count: Int) throws -> RustArray<UInt8>? { try bridge.bytes(address: address, count: count) }
}

public struct HeapGraph: Sendable {
    private let bridge: RustHeapCore.Snapshot
    private let count: Int
    public let sources: RustArray<RootSource>
    public let handleCounts: [String: Int]
    public let handles: RustArray<GcHandle>
    public let live: Column<Bool>
    public let pending: Column<Bool>
    public let retained: Column<UInt64>
    public let idom: Column<Int32>
    private let sourceStarts: Column<UInt64>
    private let sourceItems: Column<Int32>
    private let dominatorStarts: Column<UInt64>
    private let dominatorItems: Column<Int32>
    public let liveCount: Int
    public let liveBytes: UInt64
    public let pendingCount: Int
    public let pendingBytes: UInt64
    public var rootNode: Int { count + sources.count }

    public enum ObjectState: String, Sendable {
        case live, pendingFinalization = "pending finalization", dead
    }

    init(bridge: RustHeapCore.Snapshot, info: SnapshotInfo) {
        self.bridge = bridge
        count = info.objectCount
        sources = bridge.sources()
        handleCounts = info.handleCounts
        handles = bridge.handles()
        live = bridge.liveFlags().booleans
        pending = bridge.pendingFlags().booleans
        retained = bridge.retained()
        idom = bridge.dominators()
        sourceStarts = bridge.sourceStarts()
        sourceItems = bridge.sourceItems()
        dominatorStarts = bridge.dominatorStarts()
        dominatorItems = bridge.dominatorItems()
        liveCount = info.liveCount
        liveBytes = info.liveBytes
        pendingCount = info.pendingCount
        pendingBytes = info.pendingBytes
    }

    public func state(_ object: Int) -> ObjectState {
        precondition(object >= 0 && object < count, "Object index is out of range")
        return !live[object] ? .dead : pending[object] ? .pendingFinalization : .live
    }
    public func pathToRoot(_ object: Int) -> RustHeapCore.RootPath {
        precondition(object >= 0 && object < count)
        return bridge.pathToRoot(object: object)!
    }
    public func rootObjects(of source: Int) -> RustArray<Int32> {
        precondition(source >= 0 && source < sources.count)
        return sourceItems.slice(Int(sourceStarts[source])..<Int(sourceStarts[source + 1]))
    }
    public func dominatedChildrenUnsorted(of node: Int) -> RustArray<Int32> {
        precondition(node >= 0 && node <= rootNode)
        return dominatorItems.slice(Int(dominatorStarts[node])..<Int(dominatorStarts[node + 1]))
    }
}

public struct NativeGraph: Sendable {
    public typealias Chunk = RustHeapCore.Chunk
    public typealias Reach = RustHeapCore.Reach
    public typealias ChunkGroup = RustHeapCore.ChunkGroup
    private let bridge: NativeGraphView
    private let info: NativeInfo
    public let chunks: RustArray<Chunk>
    public let reach: RustArray<Reach>

    init(_ bridge: NativeGraphView) {
        self.bridge = bridge
        info = bridge.info()
        chunks = bridge.chunks()
        reach = bridge.reaches()
    }

    public var tcacheChunks: Int { info.tcacheChunks }
    public var fastbinChunks: Int { info.fastbinChunks }
    public var cachedBytes: UInt64 { info.cachedBytes }
    public func isRootChunk(_ chunk: Int32) -> Bool { bridge.isRootChunk(chunk: chunk) }
    public func largestChunks(_ chunks: RustArray<Int32>, limit: Int) throws -> RustArray<Int32> {
        try bridge.largestChunks(chunks: chunks, limit: limit)
    }
    public func totals() -> [(reach: Reach, chunks: Int, bytes: UInt64)] { info.totals.map { ($0.reach, $0.chunks, $0.bytes) } }
    public func className(_ chunk: Int) -> String? { bridge.className(chunk: chunk) }
    public func groupLabel(_ chunk: Int, snapshot: HeapSnapshot) throws -> String { try bridge.groupLabel(chunk: chunk) }
    public func ownerField(_ object: Int, chunk: Int, snapshot: HeapSnapshot) throws -> String? { try bridge.ownerField(object: object, chunk: chunk) }
    public func details(_ chunk: Int) -> NativeChunkInfo {
        precondition(chunk >= 0 && chunk < info.count, "Chunk index is out of range")
        return bridge.chunkInfo(index: chunk)!
    }
    public func ownedChunks(_ object: Int) -> RustArray<Int32> { bridge.ownedChunks(object: object) }
    public func nativeRetained(_ object: Int) -> UInt64 { bridge.nativeRetained(object: object) }
    public func owningObject(_ chunk: Int) -> Int? { details(chunk).owner }
    public func library(_ chunk: Int) -> String? { details(chunk).library }
    public func pointers(from chunk: Int) -> RustArray<Int32> { details(chunk).pointers }
    public func referrers(of chunk: Int) -> RustArray<Int32> { details(chunk).referrers }
}

public struct OwnershipGraph: Sendable {
    public typealias Node = OwnershipNode
    private let bridge: OwnershipView
    private let info: OwnershipInfo
    public let idom: Column<Int32>
    public let retainedManaged: Column<UInt64>
    public let retainedNative: Column<UInt64>
    private let childrenStarts: Column<UInt64>

    init(_ bridge: OwnershipView) {
        self.bridge = bridge
        info = bridge.info()
        idom = bridge.dominators()
        retainedManaged = bridge.retainedManaged()
        retainedNative = bridge.retainedNative()
        childrenStarts = bridge.childrenStarts()
    }

    public var rootNode: Int { 0 }
    public func node(_ index: Int) -> Node {
        precondition(index >= 0 && index < info.count, "Ownership node is out of range")
        if index == rootNode { return .root }
        if index < info.objectBase { return .source(index - 1) }
        if index < info.chunkBase { return .object(index - info.objectBase) }
        return .chunk(index - info.chunkBase)
    }
    public func hasChildren(_ index: Int) -> Bool {
        precondition((0..<info.count).contains(index))
        return childrenStarts[index] != childrenStarts[index + 1]
    }
    public func retained(_ index: Int) -> UInt64 { retainedManaged[index] + retainedNative[index] }
    public func children(of index: Int) -> RustArray<Int> { bridge.children(node: index) }
    public func pathToRoot(_ index: Int) -> RustArray<Int> { bridge.pathToRoot(node: index) }
}

public struct GarbageGraph: Sendable {
    let bridge: GarbageView
    let info: GarbageInfo
    private let retainedBytes: Column<UInt64>
    init(_ bridge: GarbageView) {
        self.bridge = bridge
        info = bridge.info()
        retainedBytes = bridge.retained()
        unreferenced = bridge.unreferenced()
    }
    public var deadCount: Int { info.deadCount }
    public var deadBytes: UInt64 { info.deadBytes }
    public let unreferenced: RustArray<Int32>
    public func retained(_ object: Int) -> UInt64 { retainedBytes[object] }
}

public struct ThreadStacks: Sendable {
    public typealias ThreadInfo = RustHeapCore.ThreadInfo
    private let bridge: ThreadStacksView
    public let threads: RustArray<ThreadInfo>
    init(_ bridge: ThreadStacksView) { self.bridge = bridge; threads = bridge.threads() }
    public func thread(_ tid: Int32) -> ThreadInfo? { threads.first { $0.tid == tid } }
    public func describe(_ address: UInt64) -> String { bridge.describe(address: address) }
    public func label(_ tid: Int32) -> String { bridge.label(tid: tid) }
    public func summary(_ thread: ThreadInfo) -> String { bridge.summary(tid: thread.tid) ?? "" }
}
