import AppKit
import HeapCore
import Observation
import SwiftUI

@MainActor @Observable
final class ViewerWindows {
    @ObservationIgnored private let sessions = NSMapTable<NSWindow, Session>(keyOptions: .weakMemory, valueOptions: .weakMemory)
    private var revision = 0

    var cacheChanges: [Int] {
        [revision] + (sessions.objectEnumerator()?.allObjects.compactMap { ($0 as? Session)?.cacheChanges } ?? [])
    }

    func register(_ session: Session, for window: NSWindow) {
        sessions.setObject(session, forKey: window)
        revision += 1
    }

    func remove(_ window: NSWindow) {
        sessions.removeObject(forKey: window)
        revision += 1
    }

    func session(for window: NSWindow) -> Session? { sessions.object(forKey: window) }

    func cacheChanged() {
        for case let session as Session in sessions.objectEnumerator()?.allObjects ?? [] {
            session.cacheChanges += 1
        }
        revision += 1
    }

    func isOpen(_ entry: CacheEntry) -> Bool {
        _ = revision
        return sessions.objectEnumerator()?.allObjects.contains {
            guard let session = $0 as? Session else { return false }
            if let directory = session.snapshot?.cacheDirectory, entry.directories.contains(directory) { return true }
            guard let dump = session.dumpPath, let path = entry.dumpPath else { return false }
            return URL(fileURLWithPath: dump).resolvingSymlinksInPath() == URL(fileURLWithPath: path).resolvingSymlinksInPath()
        } ?? false
    }
}

struct WindowSession: NSViewRepresentable {
    let session: Session
    let windows: ViewerWindows

    func makeNSView(context: Context) -> View { View(session: session, windows: windows) }
    func updateNSView(_ view: View, context: Context) {}
    static func dismantleNSView(_ view: View, coordinator: ()) { view.detach() }

    final class View: NSView {
        private let session: Session
        private let windows: ViewerWindows
        private weak var registeredWindow: NSWindow?

        init(session: Session, windows: ViewerWindows) {
            self.session = session
            self.windows = windows
            super.init(frame: .zero)
        }

        required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            detach()
            if let window {
                windows.register(session, for: window)
                registeredWindow = window
            }
        }

        func detach() {
            if let registeredWindow { windows.remove(registeredWindow) }
            registeredWindow = nil
        }
    }
}
