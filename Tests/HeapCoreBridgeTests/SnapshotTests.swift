import Foundation
@testable import HeapCore
@testable import RustHeapCore
import XCTest

final class SnapshotTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("dotheap-bridge-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let process = Foundation.Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", root.appendingPathComponent("scripts/generate-compatibility-fixtures.py").path, directory.path]
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0)
    }

    override func tearDownWithError() throws {
        try FileManager.default.removeItem(at: directory)
    }

    private func open(_ name: String) throws -> HeapSnapshot {
        try HeapSnapshot(path: directory.appendingPathComponent(name + ".dmp").path,
                         cache: .folder(directory.appendingPathComponent(name + "-cache").path))
    }

    func testGCStorageProjectionAndAccounting() throws {
        for (name, kind) in [("current-work-chunks", "mark work chunks"), ("current-finalization-storage", "finalization queue")] {
            let snapshot = try open(name)
            let region = snapshot.heap.regions[0]
            XCTAssertEqual(region.gcStorage, kind)
            XCTAssertEqual(region.gcStorageBytes, 2_060_264)
            XCTAssertEqual(region.freeBytes, 0)
            XCTAssertEqual(region.unparsedBytes, 0)
            XCTAssertEqual(region.committedUnusedBytes, 0)
            let totals = snapshot.totalStats
            XCTAssertEqual(totals.gcStorage, 2_060_264)
            XCTAssertEqual(totals.gcStorage + totals.headers + totals.committedUnused, totals.committed)
            XCTAssertEqual(try snapshot.fragmentation().totalWaste, 28_656)
            XCTAssertEqual(snapshot.satoriRegionKinds[0], kind)
            let granule = snapshot.granuleMap()[0][1]
            XCTAssertEqual(granule.gcStorage, 2_060_264)
            XCTAssertEqual(granule.gaps, 0)
            XCTAssertEqual(granule.committedUnused, 0)
            XCTAssertTrue(granule.isRegionStart)
        }
    }

    func testReferenceGraphAndCacheRoundTrip() throws {
        let snapshot = try open("reference-rooted")
        XCTAssertEqual(snapshot.heap.count, 3)
        XCTAssertEqual(snapshot.graph.liveCount, 2)
        XCTAssertEqual(Array(snapshot.graph.live), [true, true, false])
        let object = try snapshot.inspectObject(0)
        XCTAssertEqual(Array(object.references), [1])
        XCTAssertEqual(object.info.address, snapshot.heap.addresses[0])
        XCTAssertEqual(object.typeName, snapshot.typeName(ofObject: 0))
        for type in snapshot.typeStats {
            XCTAssertEqual(snapshot.typeID(named: type.name), snapshot.typeStats.first { $0.name == type.name }?.typeId)
        }
        XCTAssertNil(snapshot.typeID(named: "missing type"))
        XCTAssertEqual(snapshot.heap.object(containing: object.info.address + 8), 0)
        XCTAssertFalse(object.path.objects.isEmpty)
        XCTAssertEqual(Array(try snapshot.inspectObject(1).referrers), [0])
        XCTAssertNotNil(try snapshot.inspectObject(2).dead)
        XCTAssertFalse(snapshot.fromCache)
        let cached = try open("reference-rooted")
        XCTAssertTrue(cached.fromCache)
        XCTAssertEqual(Array(cached.heap.addresses), Array(snapshot.heap.addresses))
        XCTAssertEqual(Array(cached.graph.retained), Array(snapshot.graph.retained))
        XCTAssertEqual(try cached.inspectObject(0), object)
        for source in cached.graph.sources.indices {
            XCTAssertEqual(cached.graph.rootObjects(of: source), snapshot.graph.rootObjects(of: source))
        }
        XCTAssertEqual(cached.handleGroups().reduce(0) { $0 + $1.count }, cached.graph.handles.count)
        for (index, region) in cached.heap.regions.enumerated() {
            let objects = cached.heap.objects(inRegion: index)
            XCTAssertEqual(objects.count, region.endObject - region.firstObject)
            for (offset, info) in objects.enumerated() {
                XCTAssertEqual(info, try cached.inspectObject(region.firstObject + offset).info)
            }
        }
    }

    func testOpenProgressIncludesViewPreparationForNewAndCachedDumps() throws {
        let path = directory.appendingPathComponent("reference-rooted.dmp").path
        let cache = CacheChoice.folder(directory.appendingPathComponent("progress-cache").path)
        for cached in [false, true] {
            var updates: [(String, Double)] = []
            let snapshot = try HeapSnapshot(path: path, cache: cache) { phase, fraction in
                updates.append((phase, fraction))
            }
            XCTAssertEqual(snapshot.fromCache, cached)
            XCTAssertEqual(updates.first?.1, 0)
            XCTAssertEqual(updates.last?.0, "Done")
            XCTAssertEqual(updates.last?.1, 1)
            XCTAssertEqual(updates.first { $0.0 == "Reading dump headers" }?.1, 0.45)
            XCTAssertEqual(updates.first { $0.0 == (cached ? "Opening cached objects" : "Walking the heap") }?.1, 0.5)
            XCTAssertTrue(updates.contains { $0.0 == "Preparing the view" && $0.1 < 1 })
            XCTAssertTrue(updates.dropLast().allSatisfy { $0.1 >= 0 && $0.1 < 1 })
            XCTAssertTrue(zip(updates, updates.dropFirst()).allSatisfy { $0.1 <= $1.1 })
        }
    }

    func testOwnershipGroupsIncludeObjectSizes() throws {
        let snapshot = try open("reference-rooted")
        let graph = try snapshot.ownership
        let groups = try snapshot.ownershipGroups(of: graph.rootNode)
        XCTAssertFalse(groups.isEmpty)
        for group in groups {
            var size: UInt64 = 0
            for node in group.nodes {
                XCTAssertEqual(graph.hasChildren(node), !graph.children(of: node).isEmpty)
                if case let .object(object) = graph.node(node) { size += snapshot.heap.size(object) }
            }
            XCTAssertEqual(group.size, size)
        }
        XCTAssertEqual(try snapshot.ownershipGroups(of: graph.rootNode), groups)
        XCTAssertEqual(try open("reference-rooted").ownershipGroups(of: graph.rootNode), groups)
    }

    func testFinalizationAndDeadGraphsKeepTheirState() throws {
        let snapshot = try open("current")
        XCTAssertGreaterThan(snapshot.graph.pendingCount, 0)
        for object in 0..<snapshot.heap.count where snapshot.graph.pending[object] {
            XCTAssertEqual(snapshot.graph.state(object), .pendingFinalization)
        }
        let dead = try open("reference-dead")
        XCTAssertEqual(dead.graph.liveCount, 0)
        let garbage = try dead.garbage
        XCTAssertEqual(garbage.deadCount, 3)
        XCTAssertGreaterThan(garbage.retained(0), dead.heap.size(0))
        XCTAssertEqual((try dead.inspectObject(1).dead?.referrers).map(Array.init), [0])
    }

    func testArraysAndTypedReports() throws {
        let array = try open("query-reference-array")
        let detail = try array.inspectObject(0)
        XCTAssertEqual(detail.length, 2)
        XCTAssertEqual(Array(detail.references), [1, 2])
        XCTAssertEqual(detail.referenceElements?.map(\.target), [1, 2])
        let zero = try open("query-zero-array")
        XCTAssertEqual(try zero.mostlyZeroArrays().first?.zeroBytes, 960)
        XCTAssertFalse(try zero.allChecks().isEmpty)
        XCTAssertGreaterThan(try zero.processMemory().parts.reduce(0) { $0 + $1.bytes }, 0)
        XCTAssertEqual(try zero.nativeGraph.chunks.count, 0)
        XCTAssertGreaterThan(try zero.ownership.retained(0), 0)
        XCTAssertEqual(try zero.timers().timers.count, 0)
        XCTAssertEqual(try zero.files().open.count, 0)
        XCTAssertEqual(try zero.network().sockets.count, 0)
    }

    func testInvalidInputsReturnReadableErrors() throws {
        XCTAssertThrowsError(try open("invalid-bounds")) { error in
            XCTAssertTrue(analysisErrorMessage(error).contains("layout"))
        }
        let snapshot = try open("reference-rooted")
        XCTAssertThrowsError(try snapshot.inspectObject(snapshot.heap.count)) { error in
            XCTAssertEqual(analysisErrorMessage(error), "Object index is out of range")
        }
        XCTAssertThrowsError(try snapshot.dump.bytes(snapshot.heap.addresses[0], 32 << 20))
    }

    func testGraphOwnsSnapshotStorage() throws {
        var snapshot: HeapSnapshot? = try open("reference-rooted")
        let heap = snapshot!.heap
        let graph = snapshot!.graph
        let ownership = try snapshot!.ownership
        snapshot = nil
        XCTAssertEqual(heap.count, 3)
        XCTAssertEqual(heap.object(containing: heap.addresses[0]), 0)
        XCTAssertEqual(Array(graph.live), [true, true, false])
        XCTAssertGreaterThan(ownership.retained(0), 0)
    }

    func testColumnsReadTheSameStorageAndKeepItAlive() throws {
        var column: RustArray<UInt64>?
        weak var owner: RustOwner?
        try autoreleasepool {
            let snapshot = try open("reference-rooted")
            column = snapshot.heap.addresses
            XCTAssertEqual(column!.storage.data, snapshot.bridge.addresses().storage.data)
            owner = snapshot.bridge.owner
        }
        XCTAssertNotNil(owner)
        XCTAssertEqual(column?.count, 3)
        XCTAssertEqual(column?[0], 0x40209018)
        column = nil
        XCTAssertNil(owner)
    }

    func testCachedRecordsAndNestedSlicesRetainTheirRustOwner() throws {
        var group: OwnershipGroup?
        var nodes: RustArray<Int>?
        weak var owner: RustOwner?
        try autoreleasepool {
            let snapshot = try open("reference-rooted")
            let groups = try snapshot.ownershipGroups(of: 0)
            group = groups.first!
            nodes = group!.nodes.slice(0..<group!.nodes.count)
            owner = group!._owner
            let again = try snapshot.ownershipGroups(of: 0)
            XCTAssertEqual(group!._pointer, again[0]._pointer)
            XCTAssertEqual(nodes!.storage.data, again[0].nodes.storage.data)
        }
        XCTAssertNotNil(owner)
        XCTAssertFalse(group!.name.isEmpty)
        XCTAssertFalse(nodes!.isEmpty)
        group = nil
        XCTAssertNotNil(owner)
        XCTAssertGreaterThan(nodes![0], 0)
        nodes = nil
        XCTAssertNil(owner)
    }

    func testSharedCollectionsCompareWithoutReadingTheirElements() throws {
        let snapshot = try open("reference-rooted")
        let column = snapshot.heap.addresses
        var reads = 0
        let view = RustArray<UInt64>(column.storage, owner: snapshot.bridge.owner) { pointer, _ in
            reads += 1
            return pointer.load(as: UInt64.self)
        }
        XCTAssertEqual(view, column)
        XCTAssertEqual(view.slice(1..<3), column.slice(1..<3))
        XCTAssertEqual(reads, 0)
        XCTAssertNotEqual(view.slice(0..<1), column.slice(1..<2))
        XCTAssertEqual(reads, 1)
        XCTAssertEqual(RustArray<UInt64>(), column.slice(0..<0))
        XCTAssertNotEqual(column, column.slice(0..<1))

        var comparisons = 0
        let map = RustMap<Int, UInt64>(pointer: snapshot.bridge.owner.pointer, owner: snapshot.bridge.owner,
            get: { _, _ in nil }, equal: { _, _ in comparisons += 1; return true })
        XCTAssertEqual(map, map)
        XCTAssertEqual(comparisons, 0)
    }

    func testBorrowedOptionalAndMapKeepSnapshotStorageAlive() throws {
        let source = try open("reference-rooted")
        XCTAssertNil(source.residency)
        let text = source.native.mappings.map {
            "\(String($0.start, radix: 16))-\(String($0.end, radix: 16)) r--p 00000000 00:00 0 /fixture\nRss: \(($0.end - $0.start) / 1024) kB\n"
        }.joined()
        let smaps = directory.appendingPathComponent("matching.smaps")
        try text.write(to: smaps, atomically: true, encoding: .utf8)
        var residency: Residency?
        var rss: RustMap<UInt64, UInt64>?
        weak var owner: RustOwner?
        try autoreleasepool {
            let snapshot = try HeapSnapshot(path: directory.appendingPathComponent("reference-rooted.dmp").path,
                smapsPath: smaps.path, cache: .folder(directory.appendingPathComponent("residency-cache").path))
            residency = try XCTUnwrap(snapshot.residency)
            XCTAssertEqual(residency!._pointer, snapshot.bridge.residency()?._pointer)
            XCTAssertTrue(residency!.agrees)
            rss = residency!.rss
            owner = snapshot.bridge.owner
        }
        XCTAssertNotNil(owner)
        residency = nil
        XCTAssertNotNil(owner)
        for mapping in source.native.mappings {
            XCTAssertEqual(rss?[mapping.start], mapping.end - mapping.start)
        }
        rss = nil
        XCTAssertNil(owner)
    }

    func testPrimitiveSlicesPassBackToRustAndBytesRemainReadable() throws {
        let snapshot = try open("reference-rooted")
        let graph = try snapshot.nativeGraph
        XCTAssertFalse(graph.isRootChunk(0))
        XCTAssertTrue(try graph.largestChunks(RustArray(), limit: 500).isEmpty)
        let roots = snapshot.graph.rootObjects(of: 0)
        XCTAssertFalse(roots.isEmpty)
        XCTAssertTrue(try graph.largestChunks(roots, limit: 0).isEmpty)
        XCTAssertThrowsError(try graph.largestChunks(roots, limit: 1)) { error in
            XCTAssertEqual(analysisErrorMessage(error), "Chunk index is out of range")
        }
        let bytes = try XCTUnwrap(snapshot.dump.bytes(snapshot.heap.addresses[0], 8))
        let methodTable = bytes.enumerated().reduce(UInt64(0)) { $0 | UInt64($1.element) << ($1.offset * 8) }
        XCTAssertEqual(methodTable, snapshot.heap.methodTable(0).address)
    }

    func testNativeGroupsSizesAndReferencesUseTheSameChunks() throws {
        let snapshot = try open("native-allocations")
        let graph = try snapshot.nativeGraph
        let groups = try snapshot.chunkGroups
        XCTAssertEqual(groups.count, 2)
        let group = try XCTUnwrap(groups.first { $0.reach == .nativeRoots })
        XCTAssertEqual(group.kind, .nativeRoot)
        XCTAssertEqual(group.bytes, 16384)
        XCTAssertEqual(group.minSize, 4096)
        XCTAssertEqual(group.maxSize, 8192)
        let sizes = try graph.chunkSizes(group.chunks)
        XCTAssertEqual(sizes.map(\.size), [8192, 4096])
        XCTAssertEqual(sizes.map(\.count), [1, 2])
        XCTAssertEqual(sizes.map(\.bytes), [8192, 8192])
        XCTAssertEqual(try graph.largestChunks(group.chunks, size: 4096, limit: 500).map { graph.chunks[Int($0)].size }, [4096, 4096])
        XCTAssertEqual(try graph.largestChunks(group.chunks, limit: 1).map { graph.chunks[Int($0)].size }, [8192])
        XCTAssertTrue(try graph.largestChunks(group.chunks, size: 32, limit: 500).isEmpty)
        XCTAssertTrue(try graph.largestChunks(group.chunks, size: 0, limit: 500).isEmpty)
        for index in group.chunks {
            XCTAssertEqual(try snapshot.chunkGroup(containing: Int(index)), group.id)
        }
        let source = Int(group.chunks[0]), target = Int(group.chunks[1])
        XCTAssertTrue(graph.pointerIsExact(from: source, to: target))
        XCTAssertFalse(graph.pointerIsExact(from: target, to: source))
        XCTAssertEqual(graph.details(source).pointers.first, Int32(target))
        XCTAssertEqual(try graph.attribution(target).kind, .nativeRoot)
        XCTAssertThrowsError(try graph.attribution(graph.chunks.count))
        XCTAssertNil(try snapshot.chunkGroup(containing: graph.chunks.count))
        XCTAssertNil(try snapshot.chunkGroup(containing: -1))
        XCTAssertNil(try snapshot.chunkGroup(containing: 1 << 32))
        XCTAssertEqual(try open("native-allocations").chunkGroups.map(\.id), groups.map(\.id))
        let ownership = try snapshot.ownership
        XCTAssertEqual(ownership.roots.reduce(0) { $0 + $1.chunks }, 1)
        XCTAssertEqual(ownership.roots.reduce(0) { $0 + $1.objects }, 1)
        let root = try XCTUnwrap(ownership.roots.first { $0.chunks > 0 })
        XCTAssertEqual(root.native, 16384)
        XCTAssertEqual(Array(ownership.rootReferences(root.node)), [Int32(ownership.chunkNode(0))])
        XCTAssertEqual(ownership.node(ownership.chunkNode(0)), .chunk(0))
    }

    func testRustReleasesSwiftCallbacksOnSuccessAndFailure() throws {
        final class Callback: RustHeapCore.Progress, @unchecked Sendable {
            var updates = 0
            func update(phase: String, fraction: Double) { updates += 1 }
            func cancelled() -> Bool { false }
        }
        for name in ["reference-rooted", "invalid-bounds"] {
            weak var callback: Callback?
            try autoreleasepool {
                let progress = Callback()
                callback = progress
                let load = {
                    try RustHeapCore.Snapshot.open(path: self.directory.appendingPathComponent(name + ".dmp").path,
                        dllFolders: [], smapsPath: nil, options: AnalysisOptions(), rebuild: false,
                        cacheDirectory: self.directory.appendingPathComponent(name + "-callback-cache").path,
                        progress: progress)
                }
                if name == "invalid-bounds" { XCTAssertThrowsError(try load()) }
                else {
                    _ = try load()
                    XCTAssertGreaterThan(progress.updates, 0)
                }
            }
            XCTAssertNil(callback, name)
        }
    }
}
