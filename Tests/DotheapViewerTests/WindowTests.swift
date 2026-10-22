import AppKit
import HeapCore
import SwiftUI
import XCTest
@testable import DotheapViewer

final class WindowTests: XCTestCase {
    private var directory: URL!
    private var downloadSymbols: Any?

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.resolvingSymlinksInPath().appendingPathComponent("dotheap-windows-" + UUID().uuidString)
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["python3", root.appendingPathComponent("scripts/generate-compatibility-fixtures.py").path, directory.path]
        try process.run()
        process.waitUntilExit()
        XCTAssertEqual(process.terminationStatus, 0)
        downloadSymbols = UserDefaults.standard.object(forKey: "downloadRuntimeSymbols")
        UserDefaults.standard.set(false, forKey: "downloadRuntimeSymbols")
    }

    override func tearDownWithError() throws {
        UserDefaults.standard.set(downloadSymbols, forKey: "downloadRuntimeSymbols")
        for entry in CacheChoice.entries(measure: false) {
            if let path = entry.dumpPath,
               URL(fileURLWithPath: path).deletingLastPathComponent().resolvingSymlinksInPath() == directory.resolvingSymlinksInPath() {
                try CacheChoice.remove(entry)
            }
        }
        try FileManager.default.removeItem(at: directory)
    }

    @MainActor private func window(_ name: String, windows: ViewerWindows, delegate: AppDelegate) -> NSWindow {
        autoreleasepool {
            let file = directory.appendingPathComponent(name + ".dmp")
            let view = DumpWindow(file: .constant(file), delegate: delegate).environment(windows)
            let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 1000, height: 800),
                                  styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
            window.isReleasedWhenClosed = false
            window.contentView = NSHostingView(rootView: view)
            window.orderBack(nil)
            return window
        }
    }

    @MainActor private func loaded(_ window: NSWindow, windows: ViewerWindows) async throws -> Session {
        let deadline = Date().addingTimeInterval(10)
        while Date() < deadline {
            window.contentView?.layoutSubtreeIfNeeded()
            if let session = windows.session(for: window), !session.loading, session.summaryFindings != nil {
                XCTAssertNil(session.error)
                return session
            }
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTFail("The dump did not finish loading: \(windows.session(for: window)?.error ?? "no error")")
        return try XCTUnwrap(windows.session(for: window))
    }

    @MainActor private func close(_ window: NSWindow) {
        window.close()
        window.contentView = nil
    }

    @MainActor func testWindowsKeepSeparateDumpsAndNavigation() async throws {
        _ = NSApplication.shared
        let windows = ViewerWindows(), delegate = AppDelegate()
        let first = window("reference-rooted", windows: windows, delegate: delegate)
        let second = window("reference-dead", windows: windows, delegate: delegate)
        defer { close(first); close(second) }
        let a = try await loaded(first, windows: windows)
        let b = try await loaded(second, windows: windows)
        XCTAssertFalse(a === b)
        XCTAssertFalse(a.snapshot === b.snapshot)
        XCTAssertEqual(a.dumpPath, directory.appendingPathComponent("reference-rooted.dmp").path)
        XCTAssertEqual(b.dumpPath, directory.appendingPathComponent("reference-dead.dmp").path)

        a.section = .files
        b.section = .types
        let deadline = Date().addingTimeInterval(5)
        while (a.backStack.isEmpty || b.backStack.isEmpty) && Date() < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(a.backStack.map(\.section), [.summary])
        a.back()
        XCTAssertEqual(a.section, .summary)
        XCTAssertEqual(b.section, .types)
        XCTAssertEqual(b.backStack.count, 1)
    }

    @MainActor func testClosingAWindowReleasesItsSessionAndProtectsOtherCaches() async throws {
        _ = NSApplication.shared
        let windows = ViewerWindows(), delegate = AppDelegate()
        var first: NSWindow? = window("reference-rooted", windows: windows, delegate: delegate)
        let second = window("reference-dead", windows: windows, delegate: delegate)
        defer { if let first { close(first) }; close(second) }
        weak var oldSession: Session?
        weak var oldSnapshot: HeapSnapshot?
        do {
            let session = try await loaded(try XCTUnwrap(first), windows: windows)
            oldSession = session
            oldSnapshot = session.snapshot
        }
        let remaining = try await loaded(second, windows: windows)
        let entries = CacheChoice.entries(measure: false)
        let a = try XCTUnwrap(entries.first { $0.directory == oldSnapshot?.cacheDirectory })
        let b = try XCTUnwrap(entries.first { $0.directory == remaining.snapshot?.cacheDirectory })
        XCTAssertTrue(windows.isOpen(a))
        XCTAssertTrue(windows.isOpen(b))

        weak var oldWindow = first
        autoreleasepool {
            close(first!)
            first = nil
        }
        for _ in 0..<20 {
            if oldSession == nil, oldSnapshot == nil { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertNil(oldWindow.flatMap { windows.session(for: $0) })
        XCTAssertNil(oldSession)
        XCTAssertNil(oldSnapshot)
        XCTAssertFalse(windows.isOpen(a))
        XCTAssertTrue(windows.isOpen(b))
        XCTAssertNotNil(remaining.snapshot)
    }

    @MainActor func testCacheStaysProtectedWhileAnotherWindowReprocessesItsDump() async throws {
        _ = NSApplication.shared
        let windows = ViewerWindows(), delegate = AppDelegate()
        let window = window("reference-rooted", windows: windows, delegate: delegate)
        defer { close(window) }
        let session = try await loaded(window, windows: windows)
        let entry = try XCTUnwrap(CacheChoice.entries(measure: false).first { $0.directory == session.snapshot?.cacheDirectory })
        session.snapshot = nil
        session.loading = true
        XCTAssertTrue(windows.isOpen(entry))
        close(window)
        XCTAssertFalse(windows.isOpen(entry))
    }
}
