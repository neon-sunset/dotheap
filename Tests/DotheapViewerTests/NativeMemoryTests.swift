import AppKit
import HeapCore
import SwiftUI
import XCTest
@testable import DotheapViewer

final class NativeMemoryTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("dotheap-native-" + UUID().uuidString)
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", root.appendingPathComponent("scripts/generate-compatibility-fixtures.py").path, directory.path]
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0)
    }

    override func tearDownWithError() throws { try FileManager.default.removeItem(at: directory) }

    @MainActor private func table(_ view: NSView, columns: [String]) -> NSTableView? {
        if let table = view as? NSTableView, table.tableColumns.prefix(columns.count).map(\.title) == columns { return table }
        return view.subviews.lazy.compactMap { self.table($0, columns: columns) }.first
    }

    @MainActor private func wait(_ host: NSView, file: StaticString = #filePath, line: UInt = #line, until ready: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            (host.window?.contentView?.superview ?? host).layoutSubtreeIfNeeded()
            if ready() { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("Native memory view did not update; frame \(host.frame), chunks \(table(host, columns: ["Address", "Size"])?.numberOfRows ?? -1)", file: file, line: line)
        throw NSError(domain: "NativeMemoryTests", code: 1)
    }

    @MainActor func testGroupAndSizeSelectionsRefreshChunksAndDetails() async throws {
        _ = NSApplication.shared
        let snapshot = try HeapSnapshot(path: directory.appendingPathComponent("native-allocations.dmp").path,
                                        cache: .folder(directory.appendingPathComponent("cache").path))
        let graph = try snapshot.nativeGraph
        let groups = try snapshot.chunkGroups
        let rooted = try XCTUnwrap(groups.first { $0.reach == .nativeRoots })
        let unreferenced = try XCTUnwrap(groups.first { $0.reach == .unreferenced })
        XCTAssertEqual(rooted.chunks.count, 3)
        let session = Session()
        session.snapshot = snapshot
        session.section = .native
        session.nativeMode = .allocations
        let host = NSHostingView(rootView: ContentView().environment(session))
        host.sizingOptions = []
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 1500, height: 800),
                              styleMask: [.titled, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        defer { window.close() }
        session.selectedChunkGroup = rooted.id
        let chunkColumns = ["Address", "Size"]
        try await wait(host) { self.table(host, columns: chunkColumns)?.numberOfRows == 3 }
        let sizes = try XCTUnwrap(table(host, columns: ["Size"]))
        XCTAssertEqual(sizes.numberOfRows, 2)
        let small = try XCTUnwrap((0..<sizes.numberOfRows).first { row in
            let cell = sizes.view(atColumn: 0, row: row, makeIfNecessary: true) as? NSTableCellView
            return cell?.textField?.stringValue.filter(\.isNumber) == "4096"
        })
        sizes.selectRowIndexes(IndexSet(integer: small), byExtendingSelection: false)
        try await wait(host) { self.table(host, columns: chunkColumns)?.numberOfRows == 2 }
        let chunks = try XCTUnwrap(table(host, columns: chunkColumns))
        chunks.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        try await wait(host) { session.selectedChunk != nil }
        let selected = try XCTUnwrap(session.selectedChunk)
        XCTAssertEqual(graph.chunks[Int(selected)].size, 4096)
        XCTAssertTrue(session.showInspector)
        XCTAssertNil(session.selectedObject)
        XCTAssertEqual(session.section, .native)
        var ancestor: NSView? = chunks.superview
        var scrollViews = 0
        while let view = ancestor {
            if view is NSScrollView { scrollViews += 1 }
            ancestor = view.superview
        }
        XCTAssertEqual(scrollViews, 1, "The allocation table must not be inside a page scroll view")
        try await wait(host) {
            let detail: ChunkDetail.Model? = snapshot.memoized("native chunk \(selected)")
            return detail != nil
        }
        let detail: ChunkDetail.Model = try XCTUnwrap(snapshot.memoized("native chunk \(selected)"))
        XCTAssertEqual(detail.details.chunk.size, 4096)
        for width in [1200.0, 1700.0] {
            window.setContentSize(NSSize(width: width, height: 800))
            try await wait(host) {
                self.table(host, columns: chunkColumns)?.numberOfRows == 2 && host.frame.width == width
                    && self.table(host, columns: ["Size"])?.enclosingScrollView?.frame.height ?? 0 > 140
            }
            let groups = try XCTUnwrap(table(host, columns: ["Type or source", "Chunks", "Bytes"])?.enclosingScrollView)
            let sizes = try XCTUnwrap(table(host, columns: ["Size"])?.enclosingScrollView)
            let chunks = try XCTUnwrap(table(host, columns: chunkColumns)?.enclosingScrollView)
            let groupFrame = groups.convert(groups.bounds, to: host)
            let sizeFrame = sizes.convert(sizes.bounds, to: host)
            let chunkFrame = chunks.convert(chunks.bounds, to: host)
            XCTAssertEqual(groupFrame.minX, sizeFrame.minX, accuracy: 2)
            XCTAssertEqual(groupFrame.maxX, chunkFrame.maxX, accuracy: 2)
            XCTAssertEqual(sizeFrame.minY, chunkFrame.minY, accuracy: 2)
            XCTAssertEqual(sizeFrame.maxY, chunkFrame.maxY, accuracy: 2)
            XCTAssertFalse(groupFrame.intersects(sizeFrame))
            XCTAssertFalse(sizeFrame.intersects(chunkFrame))
            XCTAssertGreaterThan(sizeFrame.height, 140)
            XCTAssertGreaterThan(chunkFrame.width, 230)
            for scroll in [groups, sizes, chunks] {
                let table = try XCTUnwrap(scroll.documentView as? NSTableView)
                XCTAssertLessThanOrEqual(table.rect(ofColumn: table.numberOfColumns - 1).maxX, scroll.contentSize.width + 1,
                                         "Columns must fit beside the inspector")
            }
        }
        let sizeTable = try XCTUnwrap(table(host, columns: ["Size"]))
        sizeTable.sortDescriptors = [NSSortDescriptor(key: "1", ascending: false)]
        window.setContentSize(NSSize(width: 1200, height: 800))
        try await wait(host) { self.table(host, columns: ["Size"])?.tableColumns.map(\.title) == ["Size", "Chunks"] }
        XCTAssertEqual((sizeTable.view(atColumn: 1, row: 0, makeIfNecessary: true) as? NSTableCellView)?.textField?.stringValue, "2")
        XCTAssertNotEqual(table(host, columns: ["Size"])?.selectedRow, -1)
        XCTAssertEqual(session.selectedChunk, selected)
        session.selectedChunkGroup = unreferenced.id
        session.selectedChunk = nil
        try await wait(host) { self.table(host, columns: chunkColumns)?.numberOfRows == 1 }
        XCTAssertNil(table(host, columns: ["Size"]))
        session.selectedChunkGroup = rooted.id
        try await wait(host) { self.table(host, columns: chunkColumns)?.numberOfRows == 3 }
        XCTAssertEqual(table(host, columns: ["Size"])?.selectedRow, -1)
    }

    func testUniformSizeHeadingDoesNotImplyDifferentSizes() {
        XCTAssertEqual(NativeGroupChunks.title(shown: 500, total: 1249, size: 3904),
                       "\(500.formatted()) of \(1249.formatted()) chunks · \(3904.formatted()) bytes each")
        XCTAssertEqual(NativeGroupChunks.title(shown: 1, total: 1, size: 64), "1 chunk · 64 bytes")
        XCTAssertEqual(NativeGroupChunks.title(shown: 500, total: 1249, size: nil),
                       "Largest \(500.formatted()) of \(1249.formatted()) chunks")
    }

    @MainActor func testRootsOpenManagedAndNativeReferencesInTheInspector() async throws {
        _ = NSApplication.shared
        let snapshot = try HeapSnapshot(path: directory.appendingPathComponent("native-allocations.dmp").path,
                                        cache: .folder(directory.appendingPathComponent("cache").path))
        let graph = try snapshot.ownership
        let session = Session()
        session.snapshot = snapshot
        session.section = .roots
        let host = NSHostingView(rootView: ContentView().environment(session))
        host.sizingOptions = []
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 1200, height: 800),
                              styleMask: [.titled, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        defer { window.close() }
        let sourceColumns = ["Source", "Managed bytes", "Native bytes"]
        let referenceColumns = ["Reference"]
        try await wait(host) { self.table(host, columns: sourceColumns) != nil }
        let sources = try XCTUnwrap(table(host, columns: sourceColumns))
        let handles = try XCTUnwrap(table(host, columns: ["Handle type"]))
        for native in [false, true] {
            let source = try XCTUnwrap(graph.roots.first { native ? $0.chunks > 0 : $0.objects > 0 })
            let row = try XCTUnwrap((0..<sources.numberOfRows).first {
                (sources.view(atColumn: 0, row: $0, makeIfNecessary: true) as? NSTableCellView)?.textField?.stringValue == source.name
            })
            sources.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
            try await wait(host) { self.table(host, columns: referenceColumns)?.numberOfRows == source.objects + source.chunks }
            let references = try XCTUnwrap(table(host, columns: referenceColumns))
            references.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
            XCTAssertEqual(session.section, .roots)
            XCTAssertTrue(session.showInspector)
            try await wait(host) {
                guard let table = self.table(host, columns: referenceColumns) else { return false }
                return table.numberOfColumns == 2 && (table.enclosingScrollView?.contentSize.width ?? 0) > 190
            }
            let scroll = try XCTUnwrap(references.enclosingScrollView)
            let sourceScroll = try XCTUnwrap(sources.enclosingScrollView)
            let handleScroll = try XCTUnwrap(handles.enclosingScrollView)
            let sourceFrame = sourceScroll.convert(sourceScroll.bounds, to: host)
            let handleFrame = handleScroll.convert(handleScroll.bounds, to: host)
            let referenceFrame = scroll.convert(scroll.bounds, to: host)
            XCTAssertLessThan(sourceFrame.maxY, handleFrame.minY)
            XCTAssertEqual(sourceFrame.minX, handleFrame.minX, accuracy: 1)
            XCTAssertEqual(sourceFrame.maxX, handleFrame.maxX, accuracy: 1)
            XCTAssertGreaterThan(referenceFrame.minX, sourceFrame.maxX)
            XCTAssertLessThanOrEqual(references.rect(ofColumn: references.numberOfColumns - 1).maxX, scroll.contentSize.width + 1)
            XCTAssertLessThanOrEqual(sources.rect(ofColumn: sources.numberOfColumns - 1).maxX, sourceScroll.contentSize.width + 1)
            XCTAssertLessThanOrEqual(handles.rect(ofColumn: handles.numberOfColumns - 1).maxX, handleScroll.contentSize.width + 1)
            XCTAssertGreaterThan(scroll.contentSize.width, 190)
            if native {
                XCTAssertNotNil(session.selectedChunk)
                XCTAssertNil(session.selectedObject)
            } else {
                XCTAssertNotNil(session.selectedObject)
                XCTAssertNil(session.selectedChunk)
            }
        }
    }
}
