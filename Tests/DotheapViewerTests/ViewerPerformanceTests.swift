import AppKit
import HeapCore
import SwiftUI
import XCTest
@testable import DotheapViewer

final class ViewerPerformanceTests: XCTestCase {
    @MainActor func testScrollingAndTabSwitches() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let dump = environment["DOTHEAP_PERFORMANCE_DUMP"],
              let addressText = environment["DOTHEAP_PERFORMANCE_OBJECT"],
              let address = UInt64(addressText.replacingOccurrences(of: "0x", with: ""), radix: 16) else {
            throw XCTSkip("Set DOTHEAP_PERFORMANCE_DUMP and DOTHEAP_PERFORMANCE_OBJECT to benchmark a representative dump in a release build.")
        }
        let cache = environment["DOTHEAP_PERFORMANCE_CACHE"].map(CacheChoice.folder) ?? .automatic
        let snapshot = try await Task.detached { try HeapSnapshot(path: dump, cache: cache) }.value
        _ = try await Task.detached { try snapshot.network() }.value
        let object = try XCTUnwrap(snapshot.heap.object(containing: address))
        _ = try await Task.detached { try ObjectModel(snapshot, object) }.value
        _ = NSApplication.shared
        let session = Session()
        let host = NSHostingView(rootView: AnyView(NetworkView(snapshot: snapshot).environment(session).environment(\.typeNameStyle, .initials)))
        host.sizingOptions = []
        host.safeAreaRegions = []
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 1100, height: 850), styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        defer { window.close() }
        func views<T: NSView>(_ type: T.Type, in view: NSView) -> [T] {
            (view as? T).map { [$0] } ?? view.subviews.flatMap { views(type, in: $0) }
        }
        func settle(_ seconds: Double = 0.1) async throws {
            let until = Date().addingTimeInterval(seconds)
            repeat { host.layoutSubtreeIfNeeded(); window.displayIfNeeded(); try await Task.sleep(for: .milliseconds(10)) } while Date() < until
        }
        func scroll(_ scroll: NSScrollView, name: String, steps: Int = 220, stride: CGFloat = 180) async throws {
            let document = try XCTUnwrap(scroll.documentView)
            var times: [Double] = []
            for i in 0..<steps {
                let maxY = max(0, document.bounds.height - scroll.contentView.bounds.height)
                let y = CGFloat(i).truncatingRemainder(dividingBy: max(1, ceil(maxY / stride))) * stride
                let start = CFAbsoluteTimeGetCurrent()
                scroll.contentView.scroll(to: NSPoint(x: 0, y: y))
                scroll.reflectScrolledClipView(scroll.contentView)
                host.layoutSubtreeIfNeeded()
                window.displayIfNeeded()
                times.append((CFAbsoluteTimeGetCurrent() - start) * 1000)
                try await Task.sleep(for: .milliseconds(3))
            }
            times.sort()
            let p95 = times[Int(Double(times.count) * 0.95)]
            print("\(name): median \(times[times.count / 2]) ms, p95 \(p95) ms, max \(times.last!) ms")
            XCTAssertLessThan(p95, 1000 / 60, "\(name) exceeds a 60 Hz frame budget")
        }
        let deadline = Date().addingTimeInterval(30)
        while views(NSTableView.self, in: host).isEmpty, Date() < deadline { try await settle() }
        let connections = try XCTUnwrap(views(NSTableView.self, in: host).first { $0.tableColumns.first?.title == "Sockets" })
        try await scroll(try XCTUnwrap(connections.enclosingScrollView), name: "Connections")
        let start = CFAbsoluteTimeGetCurrent()
        session.networkMode = .requests
        try await settle(0.05)
        let elapsed = (CFAbsoluteTimeGetCurrent() - start) * 1000
        print("Connections to Requests: \(elapsed) ms")
        XCTAssertLessThan(elapsed, 250)
        session.networkMode = .pools
        try await settle()
        let pools = try XCTUnwrap(views(NSTableView.self, in: host).first)
        try await scroll(try XCTUnwrap(pools.enclosingScrollView), name: "HttpClient pools")
        let grouping = try XCTUnwrap(views(NSSegmentedControl.self, in: host).first { $0.segmentCount == 2 && $0.label(forSegment: 0) == "Flat" })
        grouping.selectedSegment = 1
        grouping.sendAction(grouping.action, to: grouping.target)
        try await settle()
        let grouped = try XCTUnwrap(views(NSTableView.self, in: host).first)
        try await scroll(try XCTUnwrap(grouped.enclosingScrollView), name: "Grouped HttpClient pools")
        window.setContentSize(NSSize(width: 670, height: 850))
        for style in TypeNameStyle.allCases {
            host.rootView = AnyView(ObjectDetail(snapshot: snapshot, object: object).environment(session).environment(\.typeNameStyle, style))
            try await settle(0.3)
            let inspector = try XCTUnwrap(views(NSScrollView.self, in: host).first)
            try await scroll(inspector, name: "Inspector, \(style)", steps: 240, stride: 45)
        }
    }
}
