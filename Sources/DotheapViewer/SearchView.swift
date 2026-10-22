import HeapCore
import SwiftUI

struct SearchView: View {
    @Environment(Session.self) private var session
    @Environment(\.openWindow) private var openWindow
    let snapshot: HeapSnapshot
    @State private var results: RustArray<SearchResult> = RustArray()
    @State private var selection = 0
    @FocusState private var focused: Bool

    enum Target {
        case page(Section), heapView(HeapView), finding(String), glossary, result(SearchResult.Target)
    }

    struct Item: Identifiable {
        let id: Int
        let group: String
        let title: String
        let detail: String
        let target: Target
    }

    /// Pages and page parts by name.
    private var pages: [(String, String, Target)] {
        Section.allCases.map { ($0.rawValue, "page", Target.page($0)) }
            + HeapView.allCases.map { ($0.rawValue, "Managed Heap", Target.heapView($0)) }
            + FindingsView.heapUse.map { ($0.rawValue, "Findings › Heap use", Target.finding($0.rawValue)) }
            + [("Finalizers", "Findings › Application", .finding("finalizer-backlog")), ("Glossary", "Help", .glossary)]
    }

    private var items: [Item] {
        let q = query.trimmingCharacters(in: .whitespaces)
        guard !q.isEmpty else { return [] }
        var list = pages.filter { $0.0.localizedCaseInsensitiveContains(q) }
            .map { Item(id: 0, group: "Pages", title: $0.0, detail: $0.1, target: $0.2) }
        list += results.map { Item(id: 0, group: $0.kind.rawValue, title: $0.title, detail: $0.detail, target: .result($0.target)) }
        return list.enumerated().map { Item(id: $0.offset, group: $0.element.group, title: $0.element.title, detail: $0.element.detail, target: $0.element.target) }
    }

    private var query: String { session.searchQuery }

    var body: some View {
        @Bindable var session = session
        let items = items
        VStack(alignment: .leading, spacing: 0) {
            TextField("Types, methods, findings, C++ classes or an address", text: $session.searchQuery)
                .textFieldStyle(.plain).font(.title3).padding(12)
                .focused($focused)
                .onKeyPress(.downArrow) { selection = min(selection + 1, max(items.count - 1, 0)); return .handled }
                .onKeyPress(.upArrow) { selection = max(selection - 1, 0); return .handled }
                .onSubmit { if items.indices.contains(selection) { open(items[selection]) } }
            Divider()
            if session.summaryFindings == nil {
                ProgressView("Preparing findings").frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if !query.trimmingCharacters(in: .whitespaces).isEmpty && items.isEmpty {
                Text("Nothing found.").foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(items) { item in
                                if item.id == 0 || items[item.id - 1].group != item.group {
                                    Text(item.group).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                                        .padding(.horizontal, 12).padding(.top, 10).padding(.bottom, 2)
                                }
                                row(item).id(item.id)
                            }
                        }
                        .padding(.bottom, 8)
                    }
                    .onChange(of: selection) { proxy.scrollTo(selection) }
                }
            }
        }
        .frame(width: 760, height: 520)
        .onAppear { focused = true }
        .onExitCommand { session.showSearch = false }
        .task(id: "\(query)|\(session.summaryFindings != nil)") {
            let q = query, s = snapshot
            guard session.summaryFindings != nil, !q.trimmingCharacters(in: .whitespaces).isEmpty else { results = RustArray(); return }
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled else { return }
            do {
                let found = try await Task.detached { try s.search(q, limitPerKind: 30) }.value
                guard !Task.isCancelled else { return }
                results = found
                selection = 0
            } catch {
                guard !Task.isCancelled else { return }
                session.error = analysisErrorMessage(error)
                session.showSearch = false
            }
        }
    }

    private func row(_ item: Item) -> some View {
        Button { open(item) } label: {
            VStack(alignment: .leading, spacing: 1) {
                Text(item.title).lineLimit(1).truncationMode(.middle)
                if !item.detail.isEmpty { Text(item.detail).font(.caption).foregroundStyle(.secondary).lineLimit(1) }
            }
            .padding(.horizontal, 12).padding(.vertical, 4)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(item.id == selection ? Color.accentColor.opacity(0.25) : .clear)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(item.title)
    }

    private func open(_ item: Item) {
        session.showSearch = false
        switch item.target {
        case let .page(section): session.section = section
        case let .heapView(view): session.section = .managedHeap; session.heapView = view
        case let .finding(name): session.show(finding: name)
        case .glossary: openWindow(id: "glossary", value: snapshot.heap.kind.gc.rawValue)
        case let .result(target): session.show(target, in: snapshot)
        }
    }
}
