import AppKit
import HeapCore
import SwiftUI
import XCTest
@testable import DotheapViewer

final class DisplayTests: XCTestCase {
    func testTypeNamesOnlyAbbreviateWhenTheFullNameDoesNotFit() {
        let font = NSFont.systemFont(ofSize: NSFont.systemFontSize)
        let name = "System.Collections.Generic.Dictionary`2<System.String, Example.Cache.Entry+Enumerator<System.Byte[]>>[]"
        let width = (name as NSString).size(withAttributes: [.font: font]).width
        XCTAssertEqual(TypeNameStyle.initials.display(name, width: width + 1, font: font), name)
        XCTAssertEqual(TypeNameStyle.initials.display(name, width: width - 1, font: font),
                       "S.C.G.Dictionary`2<S.String, E.C.Entry+Enumerator<S.Byte[]>>[]")
        XCTAssertEqual(TypeNameStyle.initials.display("System.Object[]", width: 200, font: font), "System.Object[]")
        XCTAssertEqual(TypeNameStyle.abbreviate("T[]*"), "T[]*")
        XCTAssertEqual(TypeNameStyle.abbreviate("<unknown type>"), "<unknown type>")
    }

    func testTwoLineNamesBreakAtTypeBoundaries() {
        let font = NSFont.systemFont(ofSize: NSFont.systemFontSize)
        let name = "System.Threading.Tasks.Task+DelayPromiseWithCancellation"
        let prefix = "System.Threading.Tasks.Task+"
        let width = (prefix as NSString).size(withAttributes: [.font: font]).width + 8
        XCTAssertEqual(TypeNameStyle.expanded.display(name, width: width, font: font),
                       prefix + "\nDelayPromiseWithCancellation")
        XCTAssertEqual(TypeNameStyle.expanded.display(name, width: width, font: font, singleLine: true), name)
        XCTAssertEqual(TypeNameStyle.expanded.display("DelayPromiseWithCancellation", width: 80, font: font),
                       "DelayPromiseWithCancellation")
        XCTAssertEqual(TypeNameStyle.expanded.display("System.Object[]", width: 200, font: font), "System.Object[]")
    }

    @MainActor func testTypeNameLinkRemainsClickable() async throws {
        _ = NSApplication.shared
        var clicks = 0
        let host = NSHostingView(rootView: AllocationLink(name: "System.Object[]", address: 0x1000, size: 64, state: .live) { clicks += 1 }
            .padding(16).frame(width: 400, height: 100))
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 400, height: 100), styleMask: .borderless, backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        defer { window.close() }
        func nameField(_ view: NSView) -> TypeNameField? {
            (view as? TypeNameField) ?? view.subviews.lazy.compactMap(nameField).first
        }
        let deadline = ContinuousClock.now + .seconds(5)
        while nameField(host)?.frame.width ?? 0 == 0, ContinuousClock.now < deadline {
            host.layoutSubtreeIfNeeded()
            try await Task.sleep(for: .milliseconds(10))
        }
        let field = try XCTUnwrap(nameField(host))
        let point = field.convert(NSPoint(x: field.bounds.midX, y: field.bounds.midY), to: nil)
        for type in [NSEvent.EventType.leftMouseDown, .leftMouseUp] {
            let event = try XCTUnwrap(NSEvent.mouseEvent(with: type, location: point, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                                        windowNumber: window.windowNumber, context: nil, eventNumber: 1, clickCount: 1, pressure: 1))
            NSApp.sendEvent(event)
        }
        XCTAssertEqual(clicks, 1)
    }

    @MainActor func testTypeNameExpandsWhenTheFieldGetsWider() {
        let field = TypeNameField(labelWithString: "")
        field.font = .systemFont(ofSize: NSFont.systemFontSize)
        field.name = "System.Threading.Tasks.Task"
        field.style = .initials
        field.frame.size = NSSize(width: 100, height: 20)
        field.layoutSubtreeIfNeeded()
        XCTAssertEqual(field.stringValue, "S.T.T.Task")
        field.frame.size.width = 400
        field.layoutSubtreeIfNeeded()
        XCTAssertEqual(field.stringValue, field.name)
    }

    @MainActor func testWrappedNamesHaveRoomForEveryLine() async throws {
        _ = NSApplication.shared
        let font = NSFont.systemFont(ofSize: NSFont.systemFontSize)
        let name = "System.Object[]"
        let fullWidth = ceil((name as NSString).size(withAttributes: [.font: font]).width)
        let widths: [CGFloat] = [48, 50, 54, fullWidth - 1, fullWidth, fullWidth + 1, 200]
        let host = NSHostingView(rootView: VStack {
            ForEach(widths, id: \.self) { width in
                TypeNameText(name: name, style: .expanded).frame(width: width)
            }
        }.frame(width: 300, height: 400))
        let window = NSWindow(contentRect: NSRect(x: -4000, y: 0, width: 300, height: 400), styleMask: .borderless, backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        window.orderBack(nil)
        defer { window.close() }
        func fields(_ view: NSView) -> [TypeNameField] {
            if let field = view as? TypeNameField { return [field] }
            return view.subviews.flatMap(fields)
        }
        let deadline = ContinuousClock.now + .seconds(5)
        while fields(host).count != widths.count || fields(host).contains(where: { $0.stringValue.isEmpty }) {
            guard ContinuousClock.now < deadline else { return XCTFail("Type names did not finish layout") }
            host.layoutSubtreeIfNeeded()
            try await Task.sleep(for: .milliseconds(10))
        }
        let names = fields(host)
        XCTAssertTrue(names.contains { $0.stringValue.contains("\n") })
        for field in names {
            XCTAssertEqual(field.stringValue.replacingOccurrences(of: "\n", with: ""), name)
            XCTAssertGreaterThanOrEqual(field.bounds.height, try XCTUnwrap(field.cell).cellSize.height)
        }
        for field in names.suffix(3) {
            XCTAssertEqual(field.stringValue, name)
        }
    }

    func testRetainedSizeShowsTotalAndTheOtherMemoryKind() {
        let total = Format.bytes(4096 + 512)
        let part = Format.bytes(512)
        XCTAssertEqual(Format.retained(managed: 4096, native: 512, nativeOwner: false), "\(total) (\(part) native)")
        XCTAssertEqual(Format.retained(managed: 512, native: 4096, nativeOwner: true), "\(total) (\(part) managed)")
        XCTAssertEqual(Format.retained(managed: 4096, native: 0, nativeOwner: false), Format.bytes(4096))
        XCTAssertEqual(Format.retained(managed: 0, native: 4096, nativeOwner: true), Format.bytes(4096))
    }
}
