import AppKit
import HeapCore
import SwiftUI
import XCTest
@testable import DotheapViewer

final class ComputedTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("dotheap-computed-" + UUID().uuidString)
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", root.appendingPathComponent("scripts/generate-compatibility-fixtures.py").path, directory.path]
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0)
    }

    override func tearDownWithError() throws { try FileManager.default.removeItem(at: directory) }

    private func snapshot() throws -> HeapSnapshot {
        try HeapSnapshot(path: directory.appendingPathComponent("reference-rooted.dmp").path,
                         cache: .folder(directory.appendingPathComponent("cache").path))
    }

    @MainActor private func view(_ snapshot: HeapSnapshot, _ key: String, _ query: Query) -> Computed<Int, ResultLabel> {
        Computed(snapshot, key: key, label: "Loading", compute: query.run) { value in
            ResultLabel(text: "\(key):\(value)")
        }
    }

    @MainActor private func window(_ content: Computed<Int, ResultLabel>) -> (NSWindow, NSHostingView<Computed<Int, ResultLabel>>) {
        _ = NSApplication.shared
        let host = NSHostingView(rootView: content)
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 600, height: 400),
                              styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        return (window, host)
    }

    @MainActor private func value(in view: NSView) -> String? {
        if let field = view as? NSTextField, field.identifier?.rawValue == "query-result" { return field.stringValue }
        return view.subviews.lazy.compactMap { self.value(in: $0) }.first
    }

    @MainActor private func wait(_ host: NSView, for expected: String?) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            host.layoutSubtreeIfNeeded()
            if value(in: host) == expected { return }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(value(in: host), expected)
        throw QueryError.timedOut
    }

    @MainActor func testChangingKeyRefreshesTheResultAndReusesTheCache() async throws {
        let snapshot = try snapshot(), first = Query(.success(11)), second = Query(.success(22))
        let (window, host) = window(view(snapshot, "first", first))
        defer { window.close() }
        try await wait(host, for: "first:11")
        host.rootView = view(snapshot, "second", second)
        try await wait(host, for: "second:22")
        host.rootView = view(snapshot, "first", first)
        try await wait(host, for: "first:11")
        XCTAssertEqual(first.calls, 1)
        XCTAssertEqual(second.calls, 1)
    }

    @MainActor func testChangingToACachedKeyReplacesTheDisplayedResult() async throws {
        let snapshot = try snapshot(), query = Query(.failure(.failed))
        _ = snapshot.memo("first") { 11 }
        _ = snapshot.memo("second") { 22 }
        let (window, host) = window(view(snapshot, "first", query))
        defer { window.close() }
        try await wait(host, for: "first:11")
        host.rootView = view(snapshot, "second", query)
        try await wait(host, for: "second:22")
        XCTAssertEqual(query.calls, 0)
    }

    @MainActor func testChangingSnapshotUsesItsOwnCacheForTheSameKey() async throws {
        let first = try snapshot(), second = try snapshot(), query = Query(.failure(.failed))
        _ = first.memo("same key") { 11 }
        _ = second.memo("same key") { 22 }
        let (window, host) = window(view(first, "same key", query))
        defer { window.close() }
        try await wait(host, for: "same key:11")
        host.rootView = view(second, "same key", query)
        try await wait(host, for: "same key:22")
        XCTAssertEqual(query.calls, 0)
    }

    @MainActor func testPreviousRowsDisappearWhileTheNewQueryIsPending() async throws {
        let snapshot = try snapshot(), first = Query(.success(11)), second = Query(.success(22), blocked: true)
        let (window, host) = window(view(snapshot, "first", first))
        defer { second.release(); window.close() }
        try await wait(host, for: "first:11")
        host.rootView = view(snapshot, "second", second)
        await fulfillment(of: [second.started], timeout: 5)
        try await wait(host, for: nil)
        second.release()
        try await wait(host, for: "second:22")
    }

    @MainActor func testChangingKeyAfterFailureLoadsTheNewResult() async throws {
        let snapshot = try snapshot(), failed = Query(.failure(.failed)), next = Query(.success(22))
        let (window, host) = window(view(snapshot, "failed", failed))
        defer { window.close() }
        await fulfillment(of: [failed.finished], timeout: 5)
        host.rootView = view(snapshot, "next", next)
        try await wait(host, for: "next:22")
    }

    @MainActor func testLateSuccessOrFailureDoesNotReplaceTheCurrentResult() async throws {
        for outcome: Result<Int, QueryError> in [.success(11), .failure(.failed)] {
            let snapshot = try snapshot(), old = Query(outcome, blocked: true), current = Query(.success(22))
            let (window, host) = window(view(snapshot, "old", old))
            defer { old.release(); window.close() }
            await fulfillment(of: [old.started], timeout: 5)
            host.rootView = view(snapshot, "current", current)
            try await wait(host, for: "current:22")
            old.release()
            await fulfillment(of: [old.finished], timeout: 5)
            host.rootView = view(snapshot, "current", current)
            host.layoutSubtreeIfNeeded()
            try await wait(host, for: "current:22")
            XCTAssertEqual(current.calls, 1)
        }
    }
}

private struct ResultLabel: NSViewRepresentable {
    let text: String

    func makeNSView(context: Context) -> NSTextField {
        let field = NSTextField(labelWithString: text)
        field.identifier = NSUserInterfaceItemIdentifier("query-result")
        return field
    }

    func updateNSView(_ field: NSTextField, context: Context) { field.stringValue = text }
}

private enum QueryError: Error { case failed, timedOut }

private final class Query: @unchecked Sendable {
    let started = XCTestExpectation(description: "Query started")
    let finished = XCTestExpectation(description: "Query finished")
    private let outcome: Result<Int, QueryError>
    private let blocked: Bool
    private let gate = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var count = 0

    init(_ outcome: Result<Int, QueryError>, blocked: Bool = false) {
        self.outcome = outcome
        self.blocked = blocked
    }

    var calls: Int {
        lock.lock()
        defer { lock.unlock() }
        return count
    }

    func release() { gate.signal() }

    func run(_ snapshot: HeapSnapshot) throws -> Int {
        lock.lock()
        count += 1
        lock.unlock()
        started.fulfill()
        defer { finished.fulfill() }
        if blocked, gate.wait(timeout: .now() + 10) != .success { throw QueryError.timedOut }
        return try outcome.get()
    }
}
