import AppKit
import HeapCore
import SwiftUI

/// The dumps opened last whose files are still where they were, from the cache folders.
struct RecentDumps: View {
    @Environment(Session.self) private var session
    @Environment(\.openWindow) private var openWindow
    @State private var entries: [CacheEntry] = []

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("Recent dumps").font(.headline).opacity(entries.isEmpty ? 0 : 1)
                Spacer()
                Button("Cache…") { openWindow(id: "cache") }.buttonStyle(.link)
            }
            if !entries.isEmpty {
                ForEach(entries.prefix(10)) { e in
                    Button { if let path = e.dumpPath { session.open(file: path) } } label: {
                        HStack(alignment: .firstTextBaseline) {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(e.name).fontWeight(.medium)
                                Text(((e.dumpPath ?? "") as NSString).deletingLastPathComponent).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                            }
                            Spacer(minLength: 24)
                            VStack(alignment: .trailing, spacing: 2) {
                                Text(e.openedDate, format: .relative(presentation: .named)).font(.caption)
                                Text(e.processed ? "processed" : "processed on first open").font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        .padding(.vertical, 4).padding(.horizontal, 8)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(e.dumpPath ?? "")
                }
            }
        }
        .frame(width: 560)
        .task(id: session.cacheChanges) {
            entries = await Task.detached { CacheChoice.entries(measure: false).filter { $0.dumpExists == true } }.value
        }
    }
}

/// The cache folder: each processed dump's folder with its disk space, whether the dump is still where it was opened,
/// and buttons that delete folders.
struct CacheView: View {
    @Environment(ViewerWindows.self) private var windows
    @State private var changes = 0
    @State private var entries: RustArray<CacheEntry>?
    @State private var others: [(name: String, holds: String, bytes: UInt64)] = []
    @State private var selection = Set<String>()
    @State private var order = [KeyPathComparator(\CacheEntry.lastOpened, order: .reverse)]
    @State private var confirmAll = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let entries {
                let total = entries.reduce(0) { $0 + $1.bytes } + others.reduce(0) { $0 + $1.bytes }
                HStack(alignment: .firstTextBaseline) {
                    Text(Format.mb(total)).font(.title2.weight(.semibold).monospacedDigit())
                    Text("in \(CacheChoice.root)").foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    Spacer()
                    Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: CacheChoice.root)]) }
                }
                Text("A dump whose folder is deleted is processed again when it opens next.")
                    .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Table(entries.sorted(using: order), selection: $selection, sortOrder: $order) {
                    TableColumn("Dump", value: \CacheEntry.name) { e in Text(e.name).help(e.dumpPath ?? "") }
                    TableColumn("Where it was opened", value: \CacheEntry.dumpFolder) { e in
                        Text(e.dumpFolder).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    }
                    TableColumn("Dump file", value: \CacheEntry.dumpStatus) { e in
                        Text(e.dumpStatus).foregroundStyle(e.dumpExists == false ? Palette.dead : .secondary)
                    }.width(min: 80, ideal: 120)
                    TableColumn("Disk space", value: \CacheEntry.bytes) { e in Text(Format.mb(e.bytes)).monospacedDigit() }.width(min: 70, ideal: 90)
                    TableColumn("Last opened", value: \CacheEntry.lastOpened) { e in Text(e.openedDate, format: .relative(presentation: .named)) }.width(min: 80, ideal: 110)
                }
                .frame(minHeight: 200)
                ForEach(others, id: \.name) { o in
                    HStack {
                        Text(o.name).fontWeight(.medium)
                        Text(o.holds).foregroundStyle(.secondary)
                        Spacer()
                        Text(Format.mb(o.bytes)).monospacedDigit()
                    }
                    .font(.callout)
                }
                HStack {
                    let missing = entries.filter { $0.dumpExists == false && !isOpen($0) }
                    Button("Delete Selected") { delete(entries.filter { selection.contains($0.id) && !isOpen($0) }) }
                        .disabled(!entries.contains { selection.contains($0.id) && !isOpen($0) })
                    Button("Delete Folders of Moved or Deleted Dumps (\(Format.mb(missing.reduce(0) { $0 + $1.bytes })))") { delete(missing) }
                        .disabled(missing.isEmpty)
                    Spacer()
                    Button("Delete All…") { confirmAll = true }.disabled(entries.allSatisfy(isOpen))
                }
                if let error { Text(error).foregroundStyle(.red).textSelection(.enabled) }
            } else {
                ProgressView("Measuring the cache folder").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .padding(16)
        .frame(minWidth: 760, minHeight: 420)
        .confirmationDialog("Delete the folders of all dumps?", isPresented: $confirmAll) {
            Button("Delete All", role: .destructive) { delete((entries ?? RustArray()).filter { !isOpen($0) }) }
        } message: {
            Text("Each dump is processed again when it opens next. Folders of open dumps stay.")
        }
        .task(id: windows.cacheChanges + [changes]) { await load() }
    }

    private func isOpen(_ e: CacheEntry) -> Bool { windows.isOpen(e) }

    private func load() async {
        let (list, other) = await Task.detached {
            (CacheChoice.entries(), CacheChoice.otherFolders.sorted { $0.key < $1.key }.map { ($0.key, $0.value, CacheChoice.diskBytes(CacheChoice.root + "/" + $0.key)) })
        }.value
        entries = list
        others = other.map { (name: $0.0, holds: $0.1, bytes: $0.2) }
    }

    private func delete(_ list: some Sequence<CacheEntry>) {
        error = nil
        for e in list {
            do { try CacheChoice.remove(e) } catch { self.error = "cannot delete \(e.directory): \(analysisErrorMessage(error))" }
        }
        selection = []
        changes += 1
    }
}

private extension CacheEntry {
    var dumpFolder: String { dumpPath.map { ($0 as NSString).deletingLastPathComponent } ?? "not recorded" }
    var dumpStatus: String { dumpExists.map { $0 ? "there" : "moved or deleted" } ?? "-" }
}

/// File menu items for the cache folders: Open Recent and the Cache window.
struct CacheCommands: Commands {
    @Environment(\.openWindow) private var openWindow
    @FocusedValue(Session.self) private var session

    var body: some Commands {
        CommandGroup(after: .newItem) {
            let _ = session?.cacheChanges
            Menu("Open Recent") {
                ForEach(CacheChoice.entries(measure: false).filter { $0.dumpExists == true }.prefix(10)) { e in
                    Button(e.name) {
                        if let path = e.dumpPath { OpenPanel.open(URL(fileURLWithPath: path), in: session, openWindow: openWindow) }
                    }
                }
            }
            Button("Cache…") { openWindow(id: "cache") }
        }
    }
}
