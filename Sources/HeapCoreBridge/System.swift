import Foundation
import RustHeapCore

public enum CacheChoice: Sendable, Equatable {
    case automatic, rebuild
    case folder(String)
    public static var root: String { RustHeapCore.cacheRoot() }
    public static let otherFolders = ["symbols": ".NET runtime debug symbols", "live": "captured dumps"]
    public static func entries(measure: Bool = true) -> RustArray<CacheEntry> { RustHeapCore.cacheEntries(measure: measure) }
    public static func diskBytes(_ path: String) -> UInt64 { RustHeapCore.diskBytes(path: path) }
    public static func remove(_ entry: CacheEntry) throws { try RustHeapCore.removeCache(entry: entry) }
    public static func removeCapturedDump(_ entry: CacheEntry) throws { try RustHeapCore.removeCapturedDump(entry: entry) }
}

public func analysisErrorMessage(_ error: Error) -> String {
    if let error = error as? AnalysisError, case .Failed(let message) = error { return message }
    return error.localizedDescription
}

public enum LiveProcess {
    public typealias Process = RustHeapCore.Process
    public static func list() -> RustArray<Process> { RustHeapCore.processes() }
    public static func absolute(_ path: String) -> String { URL(fileURLWithPath: path).standardizedFileURL.path }
    public static func defaultDumpPath(for process: Process, runtime: Bool = false) throws -> String { try RustHeapCore.defaultDumpPath(process: process, runtime: runtime) }
    public static func writeDump(_ process: Process, to path: String, pause: Bool = false,
                                 progress: @escaping (String, Double) -> Void,
                                 cancelled: @escaping () -> Bool) throws -> String? {
        try RustHeapCore.writeProcessDump(process: process, path: path, pause: pause,
            progress: ProgressHandler(update: { phase, fraction in
                progress(phase.prefix(1).uppercased() + phase.dropFirst(), fraction)
            }, cancelled: cancelled))
    }
    public static func writeRuntimeDump(_ process: Process, to path: String,
                                        progress: @escaping (String, Double) -> Void) throws {
        try RustHeapCore.writeRuntimeDump(process: process, path: path,
            progress: ProgressHandler(update: progress))
    }
}

public enum DumpConversion {
    public static func needed(_ path: String) throws -> Bool { try RustHeapCore.dumpNeedsConversion(path: path) }
    public static func convert(_ source: String, to target: String?,
                               progress: @escaping (String, Double) -> Void,
                               cancelled: @escaping () -> Bool) throws -> [String] {
        Array(try RustHeapCore.convertDump(source: source, target: target,
            progress: ProgressHandler(update: progress, cancelled: cancelled)))
    }
    public static func smaps(nextTo path: String) -> String? { RustHeapCore.smapsNextToDump(path: path) }
}

public enum TypeNames {
    public static func short(_ name: String) -> String { RustHeapCore.shortTypeName(name: name) }
    public static func withoutNamespaces(_ name: String) -> String { RustHeapCore.withoutNamespaces(name: name) }
    public static func propertyName(_ path: String) -> String { RustHeapCore.propertyName(name: path) }
}

extension SmapsMapping {
    public static func isSmaps(_ path: String) -> Bool {
        [".smaps", ".smaps.zst", ".smaps.gz"].contains { path.hasSuffix($0) }
    }
    public static func dump(nextTo path: String) -> String? { RustHeapCore.dumpNextToSmaps(path: path) }
}

extension Residency {
    public var agrees: Bool { RustHeapCore.residencyAgrees(residency: self) }
    public var warning: String? { RustHeapCore.residencyWarning(residency: self) }
}

extension CacheEntry: Identifiable {
    public var id: String { dumpPath ?? directories.first ?? name }
    public var openedDate: Date { Date(timeIntervalSince1970: lastOpened) }
}

extension RustHeapCore.Process: Identifiable {
    public var id: Int32 { pid }
}

public enum HandleTables {
    public static let typeNames = Dictionary(uniqueKeysWithValues: (UInt8(0)...UInt8(11)).map {
        ($0, RustHeapCore.handleTypeName(kind: $0))
    })
}

public struct NativeMemory: Sendable {
    public typealias Mapping = RustHeapCore.Mapping
    public typealias MallocHeap = RustHeapCore.MallocHeap
    public typealias MallocParameters = RustHeapCore.MallocParameters
    let value: RustHeapCore.NativeMemory
    public let categories: RustArray<RustHeapCore.Category>
    public var mappings: RustArray<Mapping> { value.mappings }
    public var mallocHeaps: RustArray<MallocHeap> { value.mallocHeaps }
    public var mmappedChunks: Int { value.mmappedChunks }
    public var mmappedBytes: UInt64 { value.mmappedBytes }
    public var glibcVersion: String? { value.glibcVersion }
    public static func attachedThreads(dump: CoreDump, arena: UInt64) throws -> UInt64 {
        try RustHeapCore.attachedThreads(arena: arena, snapshot: dump.bridge)
    }
}

public func verifyHeap(path: String) throws -> HeapVerification { try RustHeapCore.verifyHeap(path: path) }
