import AppKit
import HeapCore
import SwiftUI
import UniformTypeIdentifiers

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var openedArguments = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Started from a terminal as a plain executable: become a regular app with a Dock icon and menus.
        NSApplication.shared.setActivationPolicy(.regular)
        NSApplication.shared.activate()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    @MainActor func openFromArguments(_ session: Session, file: URL?) {
        guard !openedArguments else { return }
        openedArguments = true
        guard file == nil, session.dumpPath == nil, !session.loading else { return }
        let defaults = UserDefaults.standard
        if let pid = defaults.string(forKey: "pid").flatMap(Int32.init) {
            if let p = LiveProcess.list().first(where: { $0.pid == pid }) { session.open(process: p) } else { session.error = "process \(pid) is no longer running" }
            return
        }
        guard let dump = defaults.string(forKey: "dump") else { return }
        session.open(dump: dump, smaps: defaults.string(forKey: "smaps"), dlls: defaults.string(forKey: "dlls"))
    }
}

@main
struct DotheapApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var windows = ViewerWindows()

    var body: some Scene {
        WindowGroup("Dotheap", id: "dump", for: URL.self) { $file in
            DumpWindow(file: $file, delegate: delegate).environment(windows)
        }
        .defaultSize(width: 1560, height: 980)
        .commands {
            AboutCommands()
            ViewerCommands()
            HistoryCommands()
            HelpCommands()
            CacheCommands(windows: windows)
        }
        Window("About Dotheap", id: "about") {
            AboutView()
        }
        .windowResizability(.contentSize)
        .defaultPosition(.center)
        Window("Cache", id: "cache") {
            CacheView().environment(windows)
        }
        .defaultSize(width: 900, height: 560)
        WindowGroup("Glossary", id: "glossary", for: String.self) { $gc in
            GlossaryView(gc: [HeapKind.GC.satori, .coreClr, .none].first { $0.rawValue == gc }).frame(minWidth: 480, minHeight: 400)
        }
        .defaultSize(width: 640, height: 760)
        .commandsRemoved()
        Settings { SettingsView() }
    }
}

struct DumpWindow: View {
    @Binding var file: URL?
    let delegate: AppDelegate
    @State private var session = Session()
    @Environment(ViewerWindows.self) private var windows
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        ContentView()
            .environment(session)
            .focusedSceneValue(session)
            .toolbarBackgroundVisibility(.hidden, for: .windowToolbar)
            .frame(minHeight: 700)
            .background(WindowSession(session: session, windows: windows))
            .handlesExternalEvents(preferring: Set(file.map { [$0.absoluteString] } ?? []), allowing: ["*"])
            .onOpenURL { if $0.isFileURL { OpenPanel.open($0, in: session, openWindow: openWindow) } }
            .onChange(of: file, initial: true) { _, url in
                guard let url, session.dumpPath.map({ URL(fileURLWithPath: $0).standardizedFileURL }) != url else { return }
                session.open(file: url.path)
            }
            .onChange(of: session.dumpPath) { _, path in
                if let path { file = URL(fileURLWithPath: path).standardizedFileURL }
            }
            .onAppear {
                delegate.openFromArguments(session, file: file)
                MouseNavigation.install(windows)
            }
    }
}

private struct ViewerCommands: Commands {
    @FocusedValue(Session.self) private var session
    @Environment(\.openWindow) private var openWindow
    @AppStorage("downloadRuntimeSymbols") private var downloadRuntimeSymbols = true

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Window") { openWindow(id: "dump") }.keyboardShortcut("n")
            Button("Open Dump…") { OpenPanel.chooseDump(session, openWindow: openWindow) }.keyboardShortcut("o")
            Button("Dump Running Process…") { session?.showProcesses = true }
                .keyboardShortcut("o", modifiers: [.command, .shift]).disabled(session == nil)
            Button("Add Residency File…") { if let session { OpenPanel.chooseSmaps(session) } }.disabled(session?.dumpPath == nil)
            Button("Download Runtime Symbols") { session?.downloadSymbols() }
                .disabled(session?.snapshot.map { !$0.dump.platform.isLinux } ?? true || session?.symbolDownload != nil)
                .help("Downloads the debug files of the .NET runtime libraries from Microsoft's symbol server, so that native frames have function names")
            Toggle("Download Runtime Symbols When a Dump Opens", isOn: $downloadRuntimeSymbols)
                .help("Asks Microsoft's symbol server for the debug files of the .NET runtime libraries of a Linux dump by their build IDs")
            Button("Process Dump Again") {
                if let session, let d = session.dumpPath { session.open(dump: d, smaps: session.smapsPath, dlls: session.dllFolder, rebuild: true) }
            }.disabled(session?.dumpPath == nil)
        }
        CommandMenu("Analysis") {
            Button("Verify Heap…") { session?.showVerification = true }
                .disabled(session?.dumpPath == nil || session?.loading == true)
            Divider()
            Toggle("Count Native Memory Words as Roots (Conservative)", isOn: Binding(
                get: { UserDefaults.standard.object(forKey: "nativeMemoryRoots") as? Bool ?? false },
                set: {
                    UserDefaults.standard.set($0, forKey: "nativeMemoryRoots")
                    if let session, let d = session.dumpPath { session.open(dump: d, smaps: session.smapsPath, dlls: session.dllFolder) }
                }))
        }
        CommandGroup(after: .textEditing) {
            Button("Search the Dump…") { session?.showSearch = true }
                .keyboardShortcut("f", modifiers: [.command, .shift]).disabled(session?.snapshot == nil)
        }
    }
}

/// The back and forward buttons of a mouse (buttons 3 and 4) move through the history.
@MainActor enum MouseNavigation {
    private static var monitor: Any?

    static func install(_ windows: ViewerWindows) {
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .otherMouseDown) { [weak windows] e in
            guard let window = e.window, let session = windows?.session(for: window) else { return e }
            switch e.buttonNumber {
            case 3: session.back(); return nil
            case 4: session.forward(); return nil
            default: return e
            }
        }
    }
}

/// A click on a row gives its list or table the keyboard focus, as a click on an AppKit table does. The window gives the
/// focus only to the view under the pointer when that view accepts it; in SwiftUI's lists and tables that view is the
/// cell's hosting view, which does not, so the focus stayed where it was and the arrow keys moved the sidebar.
@MainActor enum TableFocus {
    private static var monitor: Any?

    static func install() {
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { e in
            guard let window = e.window, let hit = window.contentView?.superview?.hitTest(e.locationInWindow), !hit.acceptsFirstResponder else { return e }
            var view = hit.superview
            while let v = view, !(v is NSTableView) { view = v.superview }
            if let table = view, table.acceptsFirstResponder, window.firstResponder !== table { window.makeFirstResponder(table) }
            return e
        }
    }
}

struct HelpCommands: Commands {
    @FocusedValue(Session.self) private var session
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .help) {
            Button("Glossary") { openWindow(id: "glossary", value: session?.snapshot?.heap.kind.gc.rawValue ?? "") }
                .keyboardShortcut("?", modifiers: .command)
        }
    }
}

enum OpenPanel {
    @MainActor static func chooseDump(_ session: Session?, openWindow: OpenWindowAction) {
        let panel = NSOpenPanel()
        panel.title = "Open a core of a .NET process: Linux x64 or arm64 (dotnet-dump, kernel, gcore; .zst allowed) or macOS arm64 (dotnet-dump)"
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        open(url, in: session, openWindow: openWindow)
    }

    @MainActor static func open(_ url: URL, in session: Session?, openWindow: OpenWindowAction) {
        if let session, session.dumpPath == nil, !session.loading {
            session.open(file: url.path)
        } else {
            openWindow(id: "dump", value: url.standardizedFileURL)
        }
    }

    @MainActor static func chooseSmaps(_ session: Session) {
        let panel = NSOpenPanel()
        panel.title = "Choose smaps or vmmap output recorded near the capture (.zst and .gz allowed)"
        guard panel.runModal() == .OK, let url = panel.url, let dump = session.dumpPath else { return }
        session.open(dump: dump, smaps: url.path, dlls: session.dllFolder)
    }
}

struct ContentView: View {
    @Environment(Session.self) private var session
    @Environment(\.openWindow) private var openWindow
    @AppStorage("typeNameStyle") private var typeNameStyle = TypeNameStyle.standard
    private static let pageMinWidth: CGFloat = 640, inspectorMinWidth: CGFloat = 340

    var body: some View {
        @Bindable var session = session
        Group {
            if session.loading {
                VStack(spacing: 12) {
                    ProgressView(value: session.fraction) { Text(session.phase) }
                        .frame(width: 360)
                    Text(session.dumpPath ?? "").font(.caption).foregroundStyle(.secondary)
                    if session.converting || session.canCancelCapture {
                        Button("Cancel", action: session.cancelOpen)
                    }
                }
            } else if let error = session.snapshot?.dump.readError {
                ContentUnavailableView("Cannot read the dump", systemImage: "exclamationmark.triangle", description: Text(error))
            } else if let snapshot = session.snapshot {
                NavigationSplitView {
                    List(selection: $session.section) {
                        ForEach(Section.groups(managedHeap: snapshot.heap.kind.gc != .none), id: \.sections) { group in
                            if let title = group.title {
                                SwiftUI.Section(title) { ForEach(group.sections, id: \.self) { sidebarRow($0) } }
                            } else {
                                ForEach(group.sections, id: \.self) { sidebarRow($0) }
                            }
                        }
                    }
                    .navigationSplitViewColumnWidth(min: 160, ideal: 180)
                } detail: {
                    VStack(spacing: 0) {
                        if let error = session.error {
                            HStack {
                                Text(error).textSelection(.enabled)
                                Spacer()
                                Button("Dismiss") { session.error = nil }
                            }
                            .foregroundStyle(.red).padding(10)
                        }
                        ForEach(session.versionWarnings, id: \.self) { w in
                            Label(w, systemImage: "exclamationmark.triangle.fill").font(.callout).foregroundStyle(.primary)
                                .padding(.horizontal, 16).padding(.vertical, 6).frame(maxWidth: .infinity, alignment: .leading)
                                .background(Palette.queued.opacity(0.25))
                        }
                        SectionPages(snapshot: snapshot)
                    }
                    // A dump opened again after a symbol download is a new snapshot: pages compute their results again.
                    .id(ObjectIdentifier(snapshot))
                    // AppKit's split view asks a column's content for its minimum size on every constraint pass; a
                    // minimum that follows the content (wrapping text, a table's rows) changes with each layout and
                    // AppKit stops with an exception. The columns' minimums are constants; content fits inside them.
                    .frame(minWidth: 0, maxWidth: .infinity, minHeight: 0, maxHeight: .infinity, alignment: .topLeading)
                    .navigationTitle((session.dumpPath as NSString?)?.lastPathComponent ?? "")
                    .navigationSubtitle("\(snapshot.heap.count.formatted()) objects, \(session.fromCache ? "opened from cache" : "processed") in \(session.loadSeconds.formatted(.number.precision(.fractionLength(1)))) s"
                                        + (session.symbolDownload != nil && !session.symbolPhase.isEmpty ? "; \(session.symbolPhase.prefix(1).lowercased() + session.symbolPhase.dropFirst()) (\(Int(session.symbolFraction * 100))%)" : ""))
                    .inspector(isPresented: $session.showInspector) {
                        ObjectInspector(snapshot: snapshot).id(ObjectIdentifier(snapshot))
                            .frame(minWidth: 0, maxWidth: .infinity, minHeight: 0, maxHeight: .infinity)
                            .inspectorColumnWidth(min: Self.inspectorMinWidth, ideal: 440, max: 700)
                    }
                    // The inspector is inside the detail column, so the column's minimum width includes it.
                    .navigationSplitViewColumnWidth(min: Self.pageMinWidth + (session.showInspector ? Self.inspectorMinWidth : 0), ideal: Self.pageMinWidth)
                }
                .sheet(isPresented: $session.showSearch) { SearchView(snapshot: snapshot) }
                .toolbar { NavigationToolbar(session: session) }
            } else if let path = session.dumpPath, let error = session.error {
                VStack(alignment: .leading, spacing: 8) {
                    HStack {
                        Text("Findings").font(.title.weight(.semibold))
                        Spacer()
                        Button("Open Dump…") { OpenPanel.chooseDump(session, openWindow: openWindow) }
                    }
                    .padding(.horizontal, 20)
                    Text(error).foregroundStyle(.red).textSelection(.enabled).padding(.horizontal, 20)
                    Text("Normal analysis failed. Heap verification runs independently.")
                        .foregroundStyle(.secondary).padding(.horizontal, 20)
                    VerificationView(path: path, snapshot: nil, showsDone: false)
                }
                .padding(.top, 20)
            } else {
                WelcomeView()
            }
        }
        .environment(\.typeNameStyle, typeNameStyle)
        .sheet(isPresented: $session.showProcesses) { ProcessListView() }
        .sheet(isPresented: $session.showVerification) {
            if let path = session.dumpPath { VerificationView(path: path, snapshot: session.snapshot) }
        }
        .onAppear {
            TableFocus.install()
            // -window cache or -window glossary opens that window at startup.
            if let id = UserDefaults.standard.string(forKey: "window"), ["cache", "glossary"].contains(id) { openWindow(id: id) }
        }
    }

    private func sidebarRow(_ s: Section) -> some View {
        Label(s.rawValue, systemImage: s.symbol)
            .badge(s == .findings ? (session.reports?.reduce(0) { $0 + $1.findings.count } ?? 0) : 0)
            .tag(s)
    }
}

private struct HistoryCommands: Commands {
    @FocusedValue(Session.self) private var session

    var body: some Commands {
        CommandGroup(after: .toolbar) {
            Button("Back") { session?.back() }.keyboardShortcut("[").disabled(session?.backStack.isEmpty ?? true)
            Button("Forward") { session?.forward() }.keyboardShortcut("]").disabled(session?.forwardStack.isEmpty ?? true)
        }
    }
}

private struct NavigationToolbar: ToolbarContent {
    let session: Session

    var body: some ToolbarContent {
        ToolbarItem {
            Button { session.showSearch = true } label: { Label("Search the Dump", systemImage: "magnifyingglass") }.help("Search the dump (⇧⌘F)")
        }
        ToolbarItem {
            Button { session.back() } label: { Label("Back", systemImage: "chevron.backward") }.disabled(session.backStack.isEmpty).help("Back (⌘[)")
        }
        ToolbarItem {
            Button { session.forward() } label: { Label("Forward", systemImage: "chevron.forward") }.disabled(session.forwardStack.isEmpty).help("Forward (⌘])")
        }
        ToolbarItem {
            Button { session.showInspector.toggle() } label: { Label("Inspector", systemImage: "sidebar.right") }
        }
    }
}

private extension LiveProcess.Process {
    var memoryForSorting: UInt64 { memoryBytes ?? 0 }
}

/// Running processes grouped by .NET diagnostics availability.
struct ProcessListView: View {
    @Environment(Session.self) private var session
    @State private var processes: RustArray<LiveProcess.Process> = RustArray()
    @State private var selected: Int32?
    @State private var runtimeCapture = false
    @State private var filter = ""
    @State private var order = [KeyPathComparator(\LiveProcess.Process.memoryForSorting, order: .reverse)]

    private func matches(_ process: LiveProcess.Process) -> Bool {
        filter.isEmpty || process.name.localizedCaseInsensitiveContains(filter) || String(process.pid).contains(filter)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Dump a running process").font(.headline)
                Spacer()
                TextField("Filter by name or PID", text: $filter).textFieldStyle(.roundedBorder).frame(width: 220)
            }
            Text("Capture a kernel snapshot in the cache folder and open it. The process continues while the dump is written. If a snapshot is unavailable, Dotheap asks before pausing the process for capture.")
                .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            Table(of: LiveProcess.Process.self, selection: $selected, sortOrder: $order) {
                TableColumn("PID", value: \LiveProcess.Process.pid) { Text(String($0.pid)).monospacedDigit() }.width(70)
                TableColumn("Program", value: \LiveProcess.Process.name) { Text($0.name).help($0.name) }
                TableColumn("Memory", value: \LiveProcess.Process.memoryForSorting) {
                    Text($0.memoryBytes.map(Format.bytes) ?? "—").monospacedDigit()
                        .help("Physical memory footprint, including compressed memory.")
                }.width(110)
                TableColumn("User", value: \LiveProcess.Process.uid) { Text(String($0.uid)).monospacedDigit() }.width(60)
            } rows: {
                SwiftUI.Section(".NET applications") {
                    ForEach(processes.filter { $0.socket != nil && matches($0) }.sorted(using: order)) { TableRow($0) }
                }
                SwiftUI.Section("Other applications") {
                    ForEach(processes.filter { $0.socket == nil && matches($0) }.sorted(using: order)) { TableRow($0) }
                }
            }
            .frame(minHeight: 220)
            HStack {
                Button("Refresh") { processes = LiveProcess.list() }
                Toggle("Pause for .NET runtime dump", isOn: $runtimeCapture)
                    .toggleStyle(.checkbox)
                    .disabled(processes.first(where: { $0.pid == selected })?.socket == nil)
                    .help("Request the dump through .NET diagnostics. The process stays paused until the raw dump is written.")
                Spacer()
                Button("Cancel") { session.showProcesses = false }.keyboardShortcut(.cancelAction)
                Button("Dump and Open") {
                    guard let p = processes.first(where: { $0.pid == selected }) else { return }
                    session.showProcesses = false
                    session.open(process: p, runtime: runtimeCapture)
                }
                .keyboardShortcut(.defaultAction).disabled(selected == nil)
            }
        }
        .padding(16)
        .frame(width: 620, height: 400)
        .onAppear { processes = LiveProcess.list() }
        .onChange(of: selected) { _, _ in runtimeCapture = false }
    }
}

struct WelcomeView: View {
    @Environment(Session.self) private var session
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "square.grid.3x3.fill").font(.system(size: 48)).foregroundStyle(.secondary)
            Text("Dotheap").font(.largeTitle.weight(.semibold))
            Text("Open a full .NET dump from Linux (x64 or arm64) or macOS (arm64), captured with dotheap dump or dotnet-dump collect --type Full. Satori and stock CoreCLR heaps are supported.")
                .foregroundStyle(.secondary)
            HStack {
                Button("Open Dump…") { OpenPanel.chooseDump(session, openWindow: openWindow) }.keyboardShortcut("o")
                Button("Dump Running Process…") { session.showProcesses = true }
            }
            .controlSize(.large)
            if let error = session.error {
                Text(error).foregroundStyle(.red).textSelection(.enabled)
                if session.dumpPath != nil {
                    Button("Verify Heap…") { session.showVerification = true }
                }
            }
            RecentDumps().padding(.top, 12)
        }
        .padding(40)
        .dropDestination(for: URL.self) { urls, _ in
            guard let url = urls.first else { return false }
            session.open(file: url.path)
            return true
        }
    }
}
