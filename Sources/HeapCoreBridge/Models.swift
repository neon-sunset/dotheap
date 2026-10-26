import RustHeapCore

extension AnalysisOptions {
    public init() { self.init(nativeMemoryRoots: false) }
}

extension HeapSnapshot {
    public typealias CollectionCapacity = RustHeapCore.CollectionCapacity
    public typealias DuplicateString = RustHeapCore.DuplicateString
    public typealias FieldValue = RustHeapCore.FieldValue
    public typealias Files = RustHeapCore.Files
    public typealias Finalizable = RustHeapCore.Finalizable
    public typealias GarbageGroup = RustHeapCore.GarbageGroup
    public typealias HandleGroup = RustHeapCore.HandleGroup
    public typealias HttpClientPool = RustHeapCore.HttpClientPool
    public typealias MappedFile = RustHeapCore.MappedFile
    public typealias MemoryPart = RustHeapCore.MemoryPart
    public typealias NativeFileName = RustHeapCore.NativeFileName
    public typealias Network = RustHeapCore.Network
    public typealias OpenFile = RustHeapCore.OpenFile
    public typealias OwnershipGroup = RustHeapCore.OwnershipGroup
    public typealias OwnershipTypeGroup = RustHeapCore.OwnershipTypeGroup
    public typealias RequestInfo = RustHeapCore.RequestInfo
    public typealias SocketEngine = RustHeapCore.SocketEngine
    public typealias SocketInfo = RustHeapCore.SocketInfo
    public typealias SummaryFinding = RustHeapCore.SummaryFinding
    public typealias SymbolDownload = RustHeapCore.SymbolDownload
    public typealias TextPreview = RustHeapCore.TextPreview
    public typealias ThreadState = RustHeapCore.ThreadState
    public typealias TimerInfo = RustHeapCore.TimerInfo
    public typealias Timers = RustHeapCore.Timers
    public typealias TypeCount = RustHeapCore.TypeCount
    public typealias TypeGroup = RustHeapCore.TypeGroup
    public typealias Word = RustHeapCore.Word
    public typealias ZeroArrays = RustHeapCore.ZeroArrays
    public static let timerQueueTypes: Set<String> = ["System.Threading.TimerQueueTimer", "System.Threading.TimerQueue", "System.Threading.TimerQueue[]"]
    public static let socketEngineTypes: Set<String> = ["System.Net.Sockets.SocketAsyncContext", "System.Net.Sockets.SocketAsyncContext[]"]
    public static func duration(_ milliseconds: Int64) -> String { RustHeapCore.timerDuration(milliseconds: milliseconds) }
    public static func timerKey(_ timer: TimerInfo) -> String { RustHeapCore.timerKey(timer: timer) }
    public static func socketGroup(_ socket: SocketInfo) -> String { RustHeapCore.socketGroup(socket: socket) }
}

extension CheckReport {
    public typealias Cell = RustHeapCore.Cell
    public typealias Row = CheckRow
    public typealias Section = CheckSection
}

extension HeapKind { public typealias GC = Gc }
extension RootSource { public typealias Kind = RootKind }
extension SearchResult {
    public typealias Kind = SearchKind
    public typealias Target = SearchTarget
}
extension FragmentationReport {
    public typealias Contributor = RustHeapCore.Contributor
    public typealias Gen2Heap = RustHeapCore.Gen2Heap
    public var serverVote: Bool { gen2ByHeap.count > 1 && gen2ByHeap.contains(where: \.aboveThreshold) }
}

extension Platform {
    public var isLinux: Bool { self != .macOSArm64 }
    public var rawValue: String {
        switch self {
        case .linuxX64: "Linux x64"
        case .linuxArm64: "Linux arm64"
        case .macOSArm64: "macOS arm64"
        }
    }
}

extension Gc {
    public var rawValue: String {
        switch self { case .satori: "Satori"; case .coreClr: "CoreCLR"; case .none: "none" }
    }
}

extension TypeStats: Identifiable {
    public var id: UInt32 { typeId }
    public var deadCount: Int { count - liveCount - pendingCount }
    public var deadBytes: UInt64 { bytes - liveBytes - pendingBytes }
}

extension RegionStats: Identifiable {
    public var id: String { name }
    public var dead: UInt64 { objects - live - pending }
    public var liveOfUsed: Double { used > 0 ? Double(live) / Double(used) : 0 }
}

extension ResidencyCategory: Identifiable { public var id: String { name } }

extension OccupancyHistogram: Identifiable {
    public var id: String { generation }
    public static let labels = ["<5%", "5-10%", "10-25%", "25-50%", "50-75%", "75-90%", "90-100%"]
}

extension Region: Identifiable {
    public var id: UInt64 { start }
    public var size: UInt64 { end - start }
    public var headerBytes: UInt64 { objectStart - start }
    public var usedBytes: UInt64 { used > start ? min(used, end) - start : 0 }
    public var committedBytes: UInt64 { committed > start ? min(committed, end) - start : 0 }
    public var gcStorageBytes: UInt64 { gcStorage == nil ? 0 : committedBytes - min(committedBytes, headerBytes) }
    public var committedUnusedBytes: UInt64 { gcStorage == nil ? committedBytes - min(committedBytes, usedBytes) : 0 }
    public var deadBytes: UInt64 { objectBytes - liveBytes - pendingBytes }
    public var isLarge: Bool { size > ManagedHeap.granule }
    public var hasGeneration: Bool { (0...3).contains(generation) }
}

extension Granule {
    public var gaps: UInt32 { used > header + live + pending + dead + gcStorage ? used - header - live - pending - dead - gcStorage : 0 }
    public var committedUnused: UInt32 { gcStorage == 0 && committed > used ? committed - used : 0 }
}

extension Mapping: Identifiable {
    public var id: UInt64 { start }
    public var size: UInt64 { end - start }
}
extension MallocHeap: Identifiable { public var id: UInt64 { start } }
extension Category: Identifiable { public var id: String { name } }
extension GcHandle { public var type: UInt8 { kind } }
extension Word: Identifiable { public var id: Int { offset } }
extension FieldValue: Identifiable { public var id: String { path } }
extension FieldInfo { public var typeName: String { RustHeapCore.fieldTypeName(field: self) } }
extension TypeGroup: Identifiable { public var id: String { name } }
extension HandleGroup: Identifiable { public var id: String { handleType + "/" + targetType } }
extension OwnershipGroup {
    public var total: UInt64 { managed + native }
}
extension Reason: Identifiable { public var id: String { name } }
extension Contributor: Identifiable {}
extension Gen2Heap: Identifiable {
    public var id: Int32 { heap }
    public var aboveThreshold: Bool { fragmentation >= 200_000 && Double(fragmentation) >= Double(size) * 0.25 }
}

public enum StockGC {
    public static let gen2FragmentationLimit: UInt64 = 200_000
    public static let gen2FragmentationBurden = 0.25
}

extension Chunk {
    public var userStart: UInt64 { start + (state == .mapping ? 0 : 16) }
    public var userEnd: UInt64 { start + size + (state == .inUse || state == .cached ? 8 : 0) }
}
extension ChunkGroup: Identifiable {}
extension ChunkSizeGroup: Identifiable { public var id: UInt64 { size } }
extension Reach: CaseIterable {
    public static let allCases: [Reach] = [.nativeRoots, .liveManaged, .pendingManaged, .deadManaged, .unreferenced]
    public var rawValue: Int { Self.allCases.firstIndex(of: self)! }
    public var title: String {
        switch self {
        case .nativeRoots: "Native roots"
        case .liveManaged: "Live objects"
        case .pendingManaged: "Pending finalization"
        case .deadManaged: "Dead objects"
        case .unreferenced: "Unreferenced (leak candidates)"
        }
    }
    public var holder: String {
        switch self {
        case .nativeRoots: "native roots"
        case .liveManaged: "live managed objects"
        case .pendingManaged: "managed objects pending finalization"
        case .deadManaged: "dead managed objects"
        case .unreferenced: "nothing (leak candidate)"
        }
    }
}

extension DuplicateString: Identifiable {
    public var id: Int { sample }
    public var wasted: UInt64 { UInt64(count - 1) * bytesEach }
}
extension CollectionCapacity: Identifiable {
    public var id: String { kind }
    public var type: String { kind }
}
extension ZeroArrays: Identifiable {
    public var id: String { String(typeId) }
    public var type: String { kind }
}
extension TypeCount: Identifiable {
    public var id: String { String(typeId) }
    public var type: String { kind }
}
extension Finalizable: Identifiable {
    public var id: String { String(typeId) }
    public var type: String { kind }
}
extension GarbageGroup: Identifiable {
    public var id: String { String(typeId) }
    public var type: String { kind }
}

extension ThreadState: Identifiable {
    public var id: Int32 { tid }
    public var state: String {
        if waitsForLock != nil { return "waits for a lock" }
        if wait != nil { return "blocked" }
        if idle { return "idle" }
        if sleeps { return "sleeps" }
        return managed ? "other" : "native code"
    }
}
extension ThreadInfo {
    public var inUseBytes: UInt64 { stackEnd > stackPointer ? stackEnd - stackPointer : 0 }
    public var touchedBytes: UInt64 { stackEnd > deepestTouched ? stackEnd - deepestTouched : 0 }
}
extension ThreadGroup: Identifiable {
    public var id: String { function }
    public typealias Activity = ThreadActivity
}
extension ThreadActivity: Identifiable { public var id: String { title } }
extension StackNode: Identifiable {}

extension SearchKind: CaseIterable {
    public static let allCases: [SearchKind] = [.address, .finding, .checkRow, .type, .native]
    public var rawValue: String {
        switch self {
        case .address: "Address"
        case .finding: "Findings"
        case .checkRow: "Report rows"
        case .type: "Types"
        case .native: "Native memory"
        }
    }
}
