import AppKit
import HeapCore
import SwiftUI

struct SectionPages: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot

    var body: some View {
        SectionPageHost(snapshot: snapshot, section: session.section, session: session)
    }
}

private struct SectionPageHost: NSViewRepresentable {
    let snapshot: HeapSnapshot
    let section: Section
    let session: Session

    func makeNSView(context: Context) -> PageContainer { PageContainer() }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView: PageContainer, context: Context) -> CGSize? {
        proposal.replacingUnspecifiedDimensions(by: .zero)
    }

    func updateNSView(_ view: PageContainer, context: Context) {
        let session = session, snapshot = snapshot, section = section
        view.show(section, snapshot: snapshot) {
            SectionPage(snapshot: snapshot, section: section).environment(session)
        }
    }
}

final class PageContainer: NSView {
    private weak var snapshot: HeapSnapshot?
    private var page: NSHostingView<AnyView>?
    private var selected: Section?
    private var visible: Section?
    private var pending: CFRunLoopObserver?

    deinit { if let pending { CFRunLoopObserverInvalidate(pending) } }

    func show<Content: View>(_ section: Section, snapshot: HeapSnapshot, @ViewBuilder content: @escaping () -> Content) {
        guard self.snapshot !== snapshot || selected != section else { return }
        if let pending { CFRunLoopObserverInvalidate(pending) }
        pending = nil
        if self.snapshot !== snapshot {
            page?.removeFromSuperview()
            page = nil
            visible = nil
            self.snapshot = snapshot
        }
        if let previous = visible, previous != section, let old = page {
            if let responder = window?.firstResponder as? NSView, responder.isDescendant(of: old) {
                window?.makeFirstResponder(nil)
            }
        }
        selected = section
        guard visible != section else { return }

        // Commit the selection highlight before page construction or layout runs on the main thread.
        pending = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.beforeWaiting.rawValue, false, CFIndex.max) { [weak self] _, _ in
            self?.pending = nil
            DispatchQueue.main.async { [weak self] in
                guard let self, self.snapshot === snapshot, self.selected == section, self.visible != section else { return }
                let root = AnyView(content().id(section))
                if let page = self.page {
                    page.rootView = root
                } else {
                    let page = NSHostingView(rootView: root)
                    // The split view supplies the page size; content must not increase the window's minimum size.
                    page.sizingOptions = []
                    page.safeAreaRegions = []
                    page.frame = self.bounds
                    page.autoresizingMask = [.width, .height]
                    self.page = page
                    self.addSubview(page)
                }
                self.visible = section
            }
        }
        CFRunLoopAddObserver(CFRunLoopGetMain(), pending, .commonModes)
    }
}

private struct SectionPage: View {
    @AppStorage("typeNameStyle") private var typeNameStyle = TypeNameStyle.standard
    let snapshot: HeapSnapshot
    let section: Section

    var body: some View {
        Group {
            switch section {
            case .summary: SummaryView(snapshot: snapshot)
            case .findings: FindingsView(snapshot: snapshot)
            case .managedHeap: ManagedHeapView(snapshot: snapshot)
            case .native: NativeMemoryView(snapshot: snapshot)
            case .types: TypesView(snapshot: snapshot)
            case .dominators: DominatorsView(snapshot: snapshot)
            case .roots: RootsView(snapshot: snapshot)
            case .garbage: GarbageView(snapshot: snapshot)
            case .fragmentation: FragmentationView(snapshot: snapshot)
            case .tasks: TasksView(snapshot: snapshot)
            case .threads: ThreadsView(snapshot: snapshot)
            case .timers: TimersView(snapshot: snapshot)
            case .network: NetworkView(snapshot: snapshot)
            case .files: FilesView(snapshot: snapshot)
            }
        }
        .frame(minWidth: 0, maxWidth: .infinity, minHeight: 0, maxHeight: .infinity, alignment: .topLeading)
        .environment(\.typeNameStyle, typeNameStyle)
    }
}
