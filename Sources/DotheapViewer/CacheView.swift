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
            entries = await Task.detached { CacheChoice.entries(measure: false).filter { $0.dumpExists == true && $0.lastOpened > 0 } }.value
        }
    }
}

/// Captured dumps and their analysis caches.
struct CacheView: View {
    @Environment(ViewerWindows.self) private var windows
    @State private var changes = 0
    @State private var entries: RustArray<CacheEntry>?
    @State private var others: [(name: String, holds: String, bytes: UInt64)] = []
    @State private var selection = Set<String>()
    @State private var order = [KeyPathComparator(\CacheEntry.lastOpened, order: .reverse)]
    @State private var confirmAll = false
    @State private var confirmDumps = false
    @State private var pendingDumps: [CacheEntry] = []
    @State private var deleting = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let entries {
                let total = entries.reduce(0) { $0 + $1.bytes + $1.dumpBytes } + others.reduce(0) { $0 + $1.bytes }
                HStack(alignment: .firstTextBaseline) {
                    Text(Format.mb(total)).font(.title2.weight(.semibold).monospacedDigit())
                    Text("in \(CacheChoice.root)").foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    Spacer()
                    Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: CacheChoice.root)]) }
                }
                Text("Clearing analysis keeps the dump files.")
                    .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Table(entries.sorted(using: order), selection: $selection, sortOrder: $order) {
                    TableColumn("Dump", value: \CacheEntry.name) { e in
                        Text(e.name).help(e.dumpPath ?? "")
                            .contextMenu {
                                if let path = e.dumpExists == true ? e.dumpPath : e.directories.first {
                                    Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) }
                                }
                            }
                    }
                    TableColumn("Location", value: \CacheEntry.dumpFolder) { e in
                        Text(e.dumpFolder).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    }
                    TableColumn("Dump file", value: \CacheEntry.dumpStatus) { e in
                        Text(e.dumpStatus).foregroundStyle(e.dumpExists == false ? Palette.dead : .secondary)
                    }.width(min: 80, ideal: 110, max: 120)
                    TableColumn("Dump size", value: \CacheEntry.dumpBytes) { e in
                        Text(e.captured ? Format.mb(e.dumpBytes) : "—").monospacedDigit()
                    }.width(min: 70, ideal: 80, max: 90)
                    TableColumn("Analysis", value: \CacheEntry.bytes) { e in Text(Format.mb(e.bytes)).monospacedDigit() }.width(min: 70, ideal: 80, max: 90)
                    TableColumn("Last opened", value: \CacheEntry.lastOpened) { e in
                        if e.lastOpened > 0 { Text(e.openedDate, format: .relative(presentation: .named)) }
                        else { Text("Not opened").foregroundStyle(.secondary) }
                    }.width(min: 80, ideal: 100, max: 110)
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
                    let available = entries.filter { !$0.directories.isEmpty && !isOpen($0) }
                    let selected = available.filter { selection.contains($0.id) }
                    let missing = available.filter { $0.dumpExists == false }
                    let captures = entries.filter { $0.captured && selection.contains($0.id) && !isOpen($0) }
                    Menu("Clear Analysis") {
                        Button("Selected (\(Format.mb(selected.reduce(0) { $0 + $1.bytes })))") { delete(selected) }
                            .disabled(selected.isEmpty)
                        Button("Missing Dumps (\(Format.mb(missing.reduce(0) { $0 + $1.bytes })))") { delete(missing) }
                            .disabled(missing.isEmpty)
                        Divider()
                        Button("All…") { confirmAll = true }
                    }
                    .fixedSize().disabled(available.isEmpty || deleting)
                    Button("Delete Captured Dumps…", role: .destructive) {
                        pendingDumps = captures
                        confirmDumps = true
                    }.disabled(captures.isEmpty || deleting)
                    Spacer()
                    if deleting { ProgressView().controlSize(.small) }
                    Button("Refresh") { changes += 1 }.disabled(deleting)
                }
                if let error { Text(error).foregroundStyle(.red).textSelection(.enabled) }
            } else {
                ProgressView("Calculating sizes…").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .padding(16)
        .frame(minWidth: 760, minHeight: 420)
        .confirmationDialog("Clear all cached analysis?", isPresented: $confirmAll) {
            Button("Clear All Analysis", role: .destructive) { delete((entries ?? RustArray()).filter { !$0.directories.isEmpty }) }
        } message: {
            Text("Dump files are kept. Open dumps are skipped.")
        }
        .confirmationDialog("Delete \(pendingDumps.count) captured \(pendingDumps.count == 1 ? "dump" : "dumps")?", isPresented: $confirmDumps) {
            Button("Delete", role: .destructive) {
                delete(pendingDumps, captured: true)
            }
        } message: {
            Text("Permanently deletes the dumps and their analysis and residency files.")
        }
        .task(id: windows.cacheChanges + [changes]) { await load() }
    }

    private func isOpen(_ e: CacheEntry) -> Bool { windows.isOpen(e) }

    private func load() async {
        let (list, other) = await Task.detached {
            let list = CacheChoice.entries()
            let capturedBytes = list.reduce(UInt64(0)) { $0 + $1.dumpBytes }
            let other = CacheChoice.otherFolders.sorted { $0.key < $1.key }.compactMap { name, holds -> (String, String, UInt64)? in
                let bytes = CacheChoice.diskBytes(CacheChoice.root + "/" + name)
                if name == "live" {
                    let remaining = bytes - min(bytes, capturedBytes)
                    return remaining > 0 ? ("Other capture files", "residency files and unfinished dumps", remaining) : nil
                }
                return (name.capitalized, holds, bytes)
            }
            return (list, other)
        }.value
        guard !Task.isCancelled else { return }
        entries = list
        others = other.map { (name: $0.0, holds: $0.1, bytes: $0.2) }
    }

    private func delete(_ list: [CacheEntry], captured: Bool = false) {
        guard !deleting else { return }
        let list = list.filter { !isOpen($0) }
        error = nil
        deleting = true
        Task {
            let errors = await Task.detached {
                list.compactMap { entry -> String? in
                    do {
                        if captured { try CacheChoice.removeCapturedDump(entry) }
                        else { try CacheChoice.remove(entry) }
                        return nil
                    } catch { return "Cannot delete \(entry.name): \(analysisErrorMessage(error))" }
                }
            }.value
            error = errors.isEmpty ? nil : errors.joined(separator: "\n")
            selection = []
            deleting = false
            changes += 1
            windows.cacheChanged()
        }
    }
}

private extension CacheEntry {
    var dumpFolder: String { dumpPath.map { ($0 as NSString).deletingLastPathComponent } ?? "Unknown" }
    var dumpStatus: String { captured ? "Captured" : dumpExists.map { $0 ? "Available" : "Missing" } ?? "Unknown" }
}

/// File menu items for the cache folders: Open Recent and the Cache window.
struct CacheCommands: Commands {
    let windows: ViewerWindows
    @Environment(\.openWindow) private var openWindow
    @FocusedValue(Session.self) private var session

    var body: some Commands {
        CommandGroup(after: .newItem) {
            let _ = windows.cacheChanges
            Menu("Open Recent") {
                ForEach(CacheChoice.entries(measure: false).filter { $0.dumpExists == true && $0.lastOpened > 0 }.prefix(10)) { e in
                    Button(e.name) {
                        if let path = e.dumpPath { OpenPanel.open(URL(fileURLWithPath: path), in: session, openWindow: openWindow) }
                    }
                }
            }
            Button("Cache…") { openWindow(id: "cache") }
        }
    }
}
