import Foundation
import HeapCore
import XCTest
@testable import DotheapViewer

final class StaticOwnershipTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("dotheap-statics-" + UUID().uuidString)
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let process = Foundation.Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", root.appendingPathComponent("scripts/generate-compatibility-fixtures.py").path, directory.path]
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0)
    }

    override func tearDownWithError() throws { try FileManager.default.removeItem(at: directory) }

    func testSummaryCountsStaticValuesOnceAndPreservesAllocationTypes() throws {
        for mode in ["fields", "types", "unnamed", "owned"] {
            let snapshot = try HeapSnapshot(path: directory.appendingPathComponent("static-shared-\(mode).dmp").path,
                                            cache: .folder(directory.appendingPathComponent(mode + "-cache").path))
            let types = snapshot.typeStats.map { ($0.typeId, $0.count, $0.retained) }
            let owners = try MemoryTree.owners(snapshot)
            XCTAssertEqual(owners.bytes, 104)
            XCTAssertEqual(try owners.children.reduce(0) { $0 + $1.bytes }, owners.bytes)
            let live = try MemoryTree.dominated(snapshot, node: snapshot.graph.rootNode, path: "live")
            XCTAssertEqual(live.reduce(0) { $0 + $1.bytes }, 104)

            let tree = try snapshot.ownerTree()
            let storage = tree.storageFields(of: 0)
            XCTAssertEqual(storage.count, mode == "unnamed" ? 2 : 4)
            for field in storage {
                let entry = try tree.entry(field.owner)
                XCTAssertEqual(entry.managed + entry.native, 0, "Shared values do not belong to an arbitrary field")
                XCTAssertEqual(Array(entry.targets), [field.target], "A shared field value must remain inspectable")
                XCTAssertEqual(try snapshot.inspectObject(field.target).info.address, snapshot.heap.addresses[field.target])
            }
            let local = try snapshot.storageOwnerTree(0)
            let object = try snapshot.ownership.objectNode(0)
            XCTAssertEqual(try local.entry(object).managed, mode == "owned" ? 80 : 104)
            XCTAssertEqual(try local.children(of: object).reduce(0) { try $0 + local.entry($1).managed } + snapshot.heap.size(0),
                           try local.entry(object).managed)
            for (before, after) in zip(types, snapshot.typeStats) {
                XCTAssertEqual(before.0, after.typeId)
                XCTAssertEqual(before.1, after.count)
                XCTAssertEqual(before.2, after.retained)
            }
        }
    }
}
