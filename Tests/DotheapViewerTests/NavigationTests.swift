import AppKit
import HeapCore
import SwiftUI
import XCTest
@testable import DotheapViewer

final class NavigationTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("dotheap-navigation-" + UUID().uuidString)
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", root.appendingPathComponent("scripts/generate-compatibility-fixtures.py").path, directory.path]
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0)
    }

    override func tearDownWithError() throws { try FileManager.default.removeItem(at: directory) }

    private func snapshot(_ name: String = "reference-rooted") throws -> HeapSnapshot {
        try HeapSnapshot(path: directory.appendingPathComponent(name + ".dmp").path,
                         cache: .folder(directory.appendingPathComponent(name + "-cache").path))
    }

    @MainActor private func window(_ pages: PageContainer) -> NSWindow {
        _ = NSApplication.shared
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 900, height: 700),
                              styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = pages
        window.orderBack(nil)
        return window
    }

    @MainActor private func settle(_ pages: PageContainer) async throws {
        for _ in 0..<4 {
            pages.layoutSubtreeIfNeeded()
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    @MainActor private func firstView<T: NSView>(_ type: T.Type, in view: NSView) -> T? {
        if let found = view as? T { return found }
        return view.subviews.lazy.compactMap { self.firstView(type, in: $0) }.first
    }

    @MainActor func testPageObservesSessionAppearanceAndResize() async throws {
        let snapshot = try snapshot(), session = Session(), probe = Probe()
        let pages = PageContainer(), window = window(pages)
        defer { window.close() }
        pages.show(.tasks, snapshot: snapshot) { ProbeView(snapshot: snapshot, probe: probe).environment(session) }
        try await settle(pages)
        let filter = try XCTUnwrap(probe.filter)
        filter.wrappedValue = "Socket"
        let first = try XCTUnwrap(pages.subviews.first)

        session.selectedObject = 1
        try await settle(pages)
        XCTAssertEqual(probe.filter?.wrappedValue, "Socket")
        XCTAssertEqual(probe.object, 1)
        XCTAssertEqual(pages.subviews.filter { !$0.isHidden }.count, 1)

        window.appearance = NSAppearance(named: .darkAqua)
        try await settle(pages)
        XCTAssertEqual(probe.colorScheme, .dark)
        window.setContentSize(NSSize(width: 1100, height: 800))
        try await settle(pages)
        XCTAssertEqual(first.frame, pages.bounds)
    }

    @MainActor func testResizingOnlyLaysOutTheVisiblePage() async throws {
        let snapshot = try snapshot(), pages = PageContainer(), window = window(pages)
        let first = ResizeProbe(), second = ResizeProbe()
        defer { window.close() }
        pages.show(.tasks, snapshot: snapshot) { ResizeProbeView(probe: first) }
        try await settle(pages)
        pages.show(.threads, snapshot: snapshot) { ResizeProbeView(probe: second) }
        try await settle(pages)
        let previousSizes = first.sizes
        XCTAssertFalse(previousSizes.isEmpty)

        window.setContentSize(NSSize(width: 1100, height: 800))
        try await settle(pages)
        XCTAssertEqual(first.sizes, previousSizes)
        XCTAssertEqual(second.sizes.last, pages.bounds.size)
        XCTAssertNotEqual(second.sizes.last, previousSizes.last)

        pages.show(.tasks, snapshot: snapshot) { ResizeProbeView(probe: first) }
        try await settle(pages)
        XCTAssertEqual(first.sizes.last, pages.bounds.size)
        XCTAssertEqual(pages.subviews.filter { !$0.isHidden }.count, 1)
    }

    @MainActor func testWrappingControlsKeepsFocusAndTextDuringResize() async throws {
        let snapshot = try snapshot(), pages = PageContainer(), window = window(pages)
        defer { window.close() }
        pages.show(.tasks, snapshot: snapshot) { FlowProbeView() }
        try await settle(pages)
        func fields(in view: NSView) -> [NSTextField] {
            if let field = view as? NSTextField { return [field] }
            return view.subviews.flatMap { fields(in: $0) }
        }
        let controls = fields(in: pages)
        XCTAssertEqual(controls.count, 2)
        let field = try XCTUnwrap(controls.first)
        let other = try XCTUnwrap(controls.last)
        XCTAssertEqual(field.convert(field.bounds, to: pages).minY, other.convert(other.bounds, to: pages).minY)
        field.stringValue = "Socket"
        field.delegate?.controlTextDidChange?(Notification(name: NSControl.textDidChangeNotification, object: field))
        XCTAssertTrue(window.makeFirstResponder(field))

        window.setContentSize(NSSize(width: 250, height: 700))
        try await settle(pages)
        XCTAssertTrue(fields(in: pages).first === field)
        XCTAssertEqual(field.stringValue, "Socket")
        XCTAssertNotNil(field.currentEditor())
        XCTAssertNotEqual(field.convert(field.bounds, to: pages).minY, other.convert(other.bounds, to: pages).minY)
        XCTAssertLessThanOrEqual(field.convert(field.bounds, to: pages).maxX, pages.bounds.maxX)

        window.setContentSize(NSSize(width: 150, height: 700))
        try await settle(pages)
        XCTAssertLessThanOrEqual(field.convert(field.bounds, to: pages).maxX, pages.bounds.maxX)
        XCTAssertEqual(field.stringValue, "Socket")
    }

    @MainActor func testSwitchingSectionsReleasesPageContentAndReusesTheHost() async throws {
        let snapshot = try snapshot(), pages = PageContainer(), window = window(pages)
        defer { window.close() }
        weak var previous: PageLifetime?
        pages.show(.tasks, snapshot: snapshot) {
            let lifetime = PageLifetime()
            previous = lifetime
            return LifetimeView(lifetime: lifetime)
        }
        try await settle(pages)
        let host = try XCTUnwrap(pages.subviews.first)
        XCTAssertNotNil(previous)

        pages.show(.threads, snapshot: snapshot) { Text("Threads") }
        XCTAssertNotNil(previous, "The current page stays until the selection update has completed")
        try await settle(pages)
        XCTAssertNil(previous)
        XCTAssertTrue(pages.subviews.first === host)
        XCTAssertEqual(pages.subviews.count, 1)
    }

    @MainActor func testChangingDumpReleasesThePageAndSnapshot() async throws {
        let pages = PageContainer(), window = window(pages)
        defer { window.close() }
        weak var oldSnapshot: HeapSnapshot?
        weak var oldPage: NSView?
        try autoreleasepool {
            let snapshot = try snapshot()
            oldSnapshot = snapshot
            pages.show(.tasks, snapshot: snapshot) { SnapshotView(snapshot: snapshot) }
        }
        try await settle(pages)
        oldPage = pages.subviews.first
        try autoreleasepool {
            let snapshot = try XCTUnwrap(oldSnapshot)
            pages.show(.threads, snapshot: snapshot) { SnapshotView(snapshot: snapshot) }
        }
        try await settle(pages)
        XCTAssertNotNil(oldSnapshot)
        XCTAssertNotNil(oldPage)
        XCTAssertEqual(pages.subviews.count, 1)
        let replacement = try snapshot("reference-dead")
        autoreleasepool {
            pages.show(.tasks, snapshot: replacement) { SnapshotView(snapshot: replacement) }
        }
        try await settle(pages)
        XCTAssertNil(oldPage)
        XCTAssertNil(oldSnapshot)
        XCTAssertEqual(pages.subviews.count, 1)
        XCTAssertFalse(try XCTUnwrap(pages.subviews.first).isHidden)
    }

    @MainActor func testSwitchingPagesResignsFocusInRemovedContent() async throws {
        let snapshot = try snapshot(), pages = PageContainer(), window = window(pages)
        defer { window.close() }
        pages.show(.tasks, snapshot: snapshot) { TextField("Filter", text: .constant("Socket")) }
        try await settle(pages)
        let old = try XCTUnwrap(pages.subviews.first)
        let field = try XCTUnwrap(firstView(NSTextField.self, in: old))
        XCTAssertTrue(window.makeFirstResponder(field))
        pages.show(.threads, snapshot: snapshot) { Text("Threads") }
        try await settle(pages)
        XCTAssertNil(field.window)
        XCTAssertNil(field.currentEditor())
    }

    @MainActor func testTypeLinkRevealsARowExcludedByTheFilter() async throws {
        let snapshot = try snapshot(), session = Session(), pages = PageContainer(), window = window(pages)
        defer { window.close() }
        pages.show(.types, snapshot: snapshot) { TypesView(snapshot: snapshot).environment(session) }
        try await settle(pages)
        let field = try XCTUnwrap(firstView(NSTextField.self, in: pages))
        let table = try XCTUnwrap(firstView(NSTableView.self, in: pages))
        field.stringValue = "No matching type"
        field.delegate?.controlTextDidChange?(Notification(name: NSControl.textDidChangeNotification, object: field))
        try await settle(pages)
        XCTAssertEqual(table.numberOfRows, 0)
        pages.show(.tasks, snapshot: snapshot) { Text("Tasks") }
        session.selectedType = try XCTUnwrap(snapshot.typeStats.first).typeId
        pages.show(.types, snapshot: snapshot) { TypesView(snapshot: snapshot).environment(session) }
        try await settle(pages)
        XCTAssertEqual(field.stringValue, "")
        XCTAssertEqual(table.numberOfRows, snapshot.typeStats.count)
        XCTAssertGreaterThanOrEqual(table.selectedRow, 0)
    }

    @MainActor func testFirstVisitRunsAfterSelectionAndSkipsSupersededPages() async throws {
        let snapshot = try snapshot(), pages = PageContainer(), window = window(pages)
        defer { window.close() }
        var built: [DotheapViewer.Section] = []
        pages.show(.tasks, snapshot: snapshot) { built.append(.tasks); return Text("Tasks") }
        pages.show(.threads, snapshot: snapshot) { built.append(.threads); return Text("Threads") }
        XCTAssertTrue(built.isEmpty)
        try await settle(pages)
        XCTAssertEqual(built, [.threads])
        pages.show(.tasks, snapshot: snapshot) { built.append(.tasks); return Text("Tasks") }
        pages.show(.threads, snapshot: snapshot) { built.append(.threads); return Text("Threads") }
        try await settle(pages)
        XCTAssertEqual(built, [.threads])
        XCTAssertEqual(pages.subviews.count, 1)
        XCTAssertFalse(try XCTUnwrap(pages.subviews.first).isHidden)
    }

    @MainActor func testChangingDumpCancelsPendingPageConstruction() async throws {
        let pages = PageContainer(), window = window(pages)
        defer { window.close() }
        weak var oldSnapshot: HeapSnapshot?
        try autoreleasepool {
            let snapshot = try snapshot()
            oldSnapshot = snapshot
            pages.show(.tasks, snapshot: snapshot) {
                XCTFail("A page from the previous dump must not be constructed")
                return SnapshotView(snapshot: snapshot)
            }
        }
        let replacement = try snapshot("reference-dead")
        pages.show(.threads, snapshot: replacement) { SnapshotView(snapshot: replacement) }
        try await settle(pages)
        XCTAssertNil(oldSnapshot)
        XCTAssertEqual(pages.subviews.count, 1)
    }
}

@MainActor private final class Probe {
    var filter: Binding<String>?
    var object: Int?
    var colorScheme: ColorScheme?
}

private struct ProbeView: View {
    let snapshot: HeapSnapshot
    let probe: Probe
    @Environment(Session.self) private var session
    @Environment(\.colorScheme) private var colorScheme
    @State private var filter = ""

    var body: some View {
        TextField("Filter \(snapshot.heap.count) objects", text: $filter)
            .onAppear { probe.filter = $filter }
            .onChange(of: session.selectedObject, initial: true) { _, object in probe.object = object }
            .onChange(of: colorScheme, initial: true) { _, scheme in probe.colorScheme = scheme }
    }
}

private struct SnapshotView: View {
    let snapshot: HeapSnapshot
    var body: some View { Text("\(snapshot.heap.count) objects") }
}

@MainActor private final class ResizeProbe {
    var sizes: [CGSize] = []
}

private struct ResizeProbeView: View {
    let probe: ResizeProbe

    var body: some View {
        Color.clear.onGeometryChange(for: CGSize.self) { $0.size } action: { probe.sizes.append($0) }
    }
}

private final class PageLifetime {}

private struct LifetimeView: View {
    let lifetime: PageLifetime
    var body: some View { Text(ObjectIdentifier(lifetime).debugDescription) }
}

private struct FlowProbeView: View {
    @State private var text = ""

    var body: some View {
        FlowLayout(spacing: 8, lineSpacing: 8) {
            TextField("Filter", text: $text).frame(minWidth: 0, idealWidth: 200, maxWidth: 200)
            TextField("Other", text: .constant("")).frame(minWidth: 0, idealWidth: 200, maxWidth: 200)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}
