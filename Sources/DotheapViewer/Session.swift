import AppKit
import HeapCore
import Observation

private final class DumpCopyDestination: NSObject, NSOpenSavePanelDelegate {
    func panel(_ sender: Any, validate url: URL) throws {
        if FileManager.default.fileExists(atPath: url.path) {
            throw NSError(domain: NSCocoaErrorDomain, code: NSFileWriteFileExistsError,
                          userInfo: [NSLocalizedDescriptionKey: "Choose a new filename for the converted copy."])
        }
    }
}

enum Section: String, CaseIterable, Identifiable {
    case summary = "Summary"
    case findings = "Findings"
    case managedHeap = "Managed Heap"
    case native = "Native Memory"
    case types = "Types"
    case dominators = "Dominators"
    case roots = "Roots"
    case garbage = "Garbage"
    case fragmentation = "Fragmentation"
    case tasks = "Tasks"
    case threads = "Threads"
    case timers = "Timers"
    case network = "Network"
    case files = "Files"
    var id: String { rawValue }

    /// The sidebar: the two pages to start from, then the pages grouped by the question they answer. A dump without a
    /// managed heap has no pages about it.
    static func groups(managedHeap: Bool) -> [(title: String?, sections: [Section])] {
        let all: [(title: String?, sections: [Section])] = [
            (nil, [.summary, .findings]),
            ("Memory", [.managedHeap, .native, .types]),
            ("Ownership", [.dominators, .roots]),
            ("Waste", [.garbage, .fragmentation]),
            ("Concurrency", [.tasks, .threads, .timers]),
            ("I/O", [.network, .files]),
        ]
        let managedOnly: Set<Section> = [.managedHeap, .types, .garbage, .fragmentation, .tasks, .timers, .network]
        return all.map { ($0.title, $0.sections.filter { managedHeap || !managedOnly.contains($0) }) }.filter { !$0.sections.isEmpty }
    }

    var symbol: String {
        switch self {
        case .summary: "square.grid.2x2"
        case .findings: "exclamationmark.triangle"
        case .managedHeap: "square.grid.3x3.fill"
        case .native: "cpu"
        case .types: "list.bullet.rectangle"
        case .dominators: "point.3.connected.trianglepath.dotted"
        case .roots: "arrow.up.to.line"
        case .garbage: "trash"
        case .fragmentation: "square.split.diagonal.2x2"
        case .tasks: "arrow.triangle.branch"
        case .threads: "list.bullet.indent"
        case .timers: "timer"
        case .network: "network"
        case .files: "doc.on.doc"
        }
    }
}

/// The views of the Managed Heap page.
enum HeapView: String, CaseIterable, Identifiable {
    case statistics = "Statistics", map = "Region Map", list = "Region List"
    var id: String { rawValue }
}

@MainActor @Observable
final class Session {
    var dumpPath: String?
    var smapsPath: String?
    var dllFolder: String?
    var phase = ""
    var fraction = 0.0
    var loading = false
    var error: String?
    var snapshot: HeapSnapshot? {
        didSet {
            if snapshot !== oldValue { memoryTree = nil; ownerTree = nil }
        }
    }
    var memoryTree: MemoryNode?
    var ownerTree: MemoryNode?
    /// Runtime and library versions outside the supported range, or unavailable in the dump.
    var versionWarnings: [String] = []
    var loadSeconds = 0.0
    /// The download of the runtime libraries' debug files, while the open dump stays in use.
    var symbolDownload: Task<RustArray<HeapSnapshot.SymbolDownload>, Never>?
    var symbolPhase = ""
    var symbolFraction = 0.0
    @ObservationIgnored private var options = AnalysisOptions()

    /// Switching sections clears the region selection unless a link navigates to a region on purpose.
    var section: Section = .summary {
        didSet { if section != oldValue && !navigating { selectedRegion = nil } }
    }
    @ObservationIgnored private var navigating = false
    var selectedRegion: Int?
    var heapView: HeapView = .statistics
    /// The malloc chunk group selected on the Native Memory page.
    var selectedChunkGroup: String?
    enum InspectedObject: Equatable {
        case object(Int), chunk(Int)
    }
    var inspectedObject: InspectedObject?
    var selectedChunk: Int32? {
        get { if case let .chunk(index) = inspectedObject { Int32(index) } else { nil } }
        set {
            if let newValue { inspectedObject = .chunk(Int(newValue)) }
            else if case .chunk = inspectedObject { inspectedObject = nil }
        }
    }
    var nativeMode: NativeMemoryView.Mode = .overview
    var showSearch = false
    var searchQuery = ""
    /// The entry open on the Findings page: a check name or a heap use list.
    var finding = "async"
    /// Every check, computed in the background after the dump opens; the summary findings come with them.
    var reports: RustArray<CheckReport>?
    var integrityReport: CheckReport?
    var summaryFindings: RustArray<HeapSnapshot.SummaryFinding>?
    var selectedType: UInt32?
    /// The group of task graphs, the graph and the node (its path from the graph's roots) open on the Tasks page.
    var taskGroup: Int?
    var taskGraph: Int?
    var taskNode: [Int]?
    /// The view and the merged stack row open on the Threads page.
    var threadsMode = ThreadsView.Mode.stacks
    var threadStack: String?
    /// The row open on the Timers page, and the views and rows open on the I/O pages.
    var timerGroup: String?
    var timer: Int?
    var networkMode = NetworkView.Mode.connections
    var networkGroup: String?
    var filesMode = FilesView.Mode.open
    var selectedObject: Int? {
        get { if case let .object(index) = inspectedObject { index } else { nil } }
        set {
            if let newValue { inspectedObject = .object(newValue) }
            else if case .object = inspectedObject { inspectedObject = nil }
        }
    }
    var showInspector = false
    /// The Summary page's view and the ids of the blocks open in its map.
    var summaryMode: SummaryMode = SummaryMode(rawValue: UserDefaults.standard.string(forKey: "summaryMode") ?? "") ?? .kind {
        didSet { UserDefaults.standard.set(summaryMode.rawValue, forKey: "summaryMode") }
    }
    var summaryPath: [String] = []

    /// Where the window is: the page and what is open or selected on it. Back and Forward move between places.
    struct Place: Equatable {
        var section: Section, heapView: HeapView, finding: String, selectedRegion: Int?, selectedType: UInt32?
        var selectedChunkGroup: String?, taskGroup: Int?, taskGraph: Int?, taskNode: [Int]?
        var threadsMode: ThreadsView.Mode, threadStack: String?, timerGroup: String?, timer: Int?, networkMode: NetworkView.Mode, networkGroup: String?
        var filesMode: FilesView.Mode, inspectedObject: InspectedObject?, nativeMode: NativeMemoryView.Mode, summaryMode: SummaryMode, summaryPath: [String]
    }

    var place: Place {
        get {
            Place(section: section, heapView: heapView, finding: finding, selectedRegion: selectedRegion, selectedType: selectedType,
                  selectedChunkGroup: selectedChunkGroup, taskGroup: taskGroup, taskGraph: taskGraph, taskNode: taskNode,
                  threadsMode: threadsMode, threadStack: threadStack, timerGroup: timerGroup, timer: timer, networkMode: networkMode, networkGroup: networkGroup,
                  filesMode: filesMode, inspectedObject: inspectedObject, nativeMode: nativeMode, summaryMode: summaryMode, summaryPath: summaryPath)
        }
        set {
            navigating = true
            section = newValue.section
            navigating = false
            heapView = newValue.heapView; finding = newValue.finding; selectedRegion = newValue.selectedRegion; selectedType = newValue.selectedType
            selectedChunkGroup = newValue.selectedChunkGroup
            taskGroup = newValue.taskGroup; taskGraph = newValue.taskGraph; taskNode = newValue.taskNode
            threadsMode = newValue.threadsMode; threadStack = newValue.threadStack; timerGroup = newValue.timerGroup; timer = newValue.timer
            networkMode = newValue.networkMode; networkGroup = newValue.networkGroup; filesMode = newValue.filesMode
            inspectedObject = newValue.inspectedObject; nativeMode = newValue.nativeMode
            summaryMode = newValue.summaryMode; summaryPath = newValue.summaryPath
            showInspector = inspectedObject != nil
        }
    }

    private(set) var backStack: [Place] = []
    private(set) var forwardStack: [Place] = []
    /// The place last recorded; changes made in one turn of the main run loop are one step.
    @ObservationIgnored private var recorded: Place?

    var fromCache = false
    var converting = false
    @ObservationIgnored private var openTask: Task<Void, Never>?

    func cancelConversion() { openTask?.cancel() }

    private enum ConversionChoice { case original, convert(String?), cancel }

    private func conversionChoice(_ path: String) -> ConversionChoice {
        let alert = NSAlert()
        alert.messageText = "Convert this dump for faster access?"
        alert.informativeText = "This dump must be decompressed sequentially. Conversion lets Dotheap read compressed blocks directly and avoid a second compressed copy in its cache. Replacing the original needs temporary space and keeps the original intact until conversion is verified."
        alert.addButton(withTitle: "Replace Original")
        alert.addButton(withTitle: "Keep Original…")
        alert.addButton(withTitle: "Open Original")
        alert.addButton(withTitle: "Cancel")
        alert.showsSuppressionButton = true
        let response = alert.runModal()
        alert.window.orderOut(nil)
        if alert.suppressionButton?.state == .on {
            UserDefaults.standard.set(false, forKey: "suggestDumpConversion")
        }
        if response == .alertFirstButtonReturn { return .convert(nil) }
        guard response == .alertSecondButtonReturn else {
            return response == .alertThirdButtonReturn ? .original : .cancel
        }
        let panel = NSSavePanel()
        let validation = DumpCopyDestination()
        panel.delegate = validation
        panel.title = "Save converted dump"
        panel.prompt = "Convert"
        let source = URL(fileURLWithPath: path)
        var name = source.deletingPathExtension().lastPathComponent
        if name.hasSuffix(".dmp") { name.removeLast(4) }
        panel.nameFieldStringValue = name + "-seekable.dmp.zst"
        panel.directoryURL = source.deletingLastPathComponent()
        guard withExtendedLifetime(validation, { panel.runModal() }) == .OK,
              let destination = panel.url else { return .cancel }
        return .convert(destination.path)
    }

    func open(dump: String, smaps: String?, dlls: String?, rebuild: Bool = false) {
        dumpPath = dump; smapsPath = smaps; dllFolder = dlls
        snapshot = nil; error = nil; loading = true; phase = "Opening the dump"; fraction = 0
        reports = nil; integrityReport = nil; summaryFindings = nil
        selectedRegion = nil; selectedType = nil; selectedObject = nil; summaryPath = []; backStack = []; forwardStack = []; recorded = nil
        taskGroup = nil; taskGraph = nil; taskNode = nil; threadStack = nil; timerGroup = nil; timer = nil; networkGroup = nil
        selectedChunkGroup = nil; inspectedObject = nil; showInspector = false
        symbolDownload?.cancel(); symbolDownload = nil
        let options = AnalysisOptions(nativeMemoryRoots: UserDefaults.standard.object(forKey: "nativeMemoryRoots") as? Bool ?? false)
        self.options = options
        let suggestConversion = UserDefaults.standard.object(forKey: "suggestDumpConversion") as? Bool ?? true
        openTask = Task.detached(priority: .userInitiated) {
            do {
                var dump = dump
                var smaps = smaps
                var conversionWarnings: [String] = []
                if suggestConversion, !rebuild, try DumpConversion.needed(dump) {
                    switch await self.conversionChoice(dump) {
                    case .cancel:
                        await MainActor.run { self.loading = false }
                        return
                    case .original: break
                    case .convert(let target):
                        let destination = target ?? dump
                        await MainActor.run { self.converting = true; self.phase = "Converting and verifying the dump" }
                        conversionWarnings = try DumpConversion.convert(dump, to: target, progress: { phase, fraction in
                            Task { @MainActor in
                                guard self.converting else { return }
                                self.phase = phase; self.fraction = fraction
                            }
                        }, cancelled: { Task.isCancelled })
                        smaps = smaps ?? DumpConversion.smaps(nextTo: destination)
                        dump = destination
                        let convertedSmaps = smaps
                        await MainActor.run {
                            self.converting = false; self.fraction = 0
                            self.dumpPath = destination; self.smapsPath = convertedSmaps
                        }
                    }
                }
                let started = Date()
                let openedDump = dump
                let s = try HeapSnapshot(path: dump, dllFolders: dlls.map { [$0] } ?? [], smapsPath: smaps, options: options,
                                         cache: rebuild ? .rebuild : .automatic) { phase, fraction in
                    Task { @MainActor in
                        guard self.loading, fraction >= self.fraction else { return }
                        self.phase = phase; self.fraction = fraction
                    }
                }
                let warnings = s.memo("versions") { s.versions }.warnings + (s.residency?.warning.map { [$0] } ?? []) + conversionWarnings
                await MainActor.run {
                    self.snapshot = s
                    self.versionWarnings = warnings
                    self.fromCache = s.fromCache
                    self.loading = false
                    self.loadSeconds = Date().timeIntervalSince(started)
                    self.applyStartSelection(s)
                    self.startHistory()
                    // The Dock menu's recent files; Open Recent reads the cache folders.
                    NSDocumentController.shared.noteNewRecentDocumentURL(URL(fileURLWithPath: LiveProcess.absolute(openedDump)))
                    self.cacheChanges += 1
                }
                async let checks: Void = self.runChecks(s)
                var shown = s
                if s.dump.platform.isLinux, UserDefaults.standard.object(forKey: "downloadRuntimeSymbols") as? Bool ?? true {
                    shown = await self.updateSymbols(s, automatic: true)
                }
                await checks
                if shown !== s { await self.runChecks(shown) }
            } catch {
                let cancelled = Task.isCancelled
                await MainActor.run {
                    self.error = cancelled ? nil : analysisErrorMessage(error)
                    self.loading = false; self.converting = false
                }
            }
        }
    }

    /// Opens a file from Finder, a drop or the open panel. An smaps file opens the dump next to it, or else is added to
    /// the open dump; any other file opens as a dump.
    func open(file path: String) {
        guard SmapsMapping.isSmaps(path) else { open(dump: path, smaps: nil, dlls: nil); return }
        if let dump = SmapsMapping.dump(nextTo: path) {
            open(dump: dump, smaps: path, dlls: dump == dumpPath ? dllFolder : nil)
        } else if let dump = dumpPath {
            open(dump: dump, smaps: path, dlls: dllFolder)
        } else {
            error = "There is no dump next to \(path) with the same name and .dmp.zst or .dmp. Open the dump first, then add the smaps file."
        }
    }

    var showProcesses = false
    var showVerification = false
    /// Counts opens and cache deletions, so that the views of the cache folders read them again.
    var cacheChanges = 0

    /// Asks a running .NET process for a full dump through its diagnostics socket, then opens the dump.
    func open(process p: LiveProcess.Process) {
        snapshot = nil; error = nil; loading = true; fraction = 0
        phase = "Writing a full dump of \(p.name) (pid \(p.pid))"
        Task.detached(priority: .userInitiated) {
            do {
                let path = try LiveProcess.defaultDumpPath(for: p)
                try LiveProcess.writeDump(p, to: path)
                await MainActor.run { self.open(dump: path, smaps: nil, dlls: nil) }
            } catch {
                await MainActor.run { self.error = analysisErrorMessage(error); self.loading = false }
            }
        }
    }

    /// Startup selection from user defaults: -section <name>, -heapView <name>, -finding <check name>, -search <text>,
    /// -region <hex address>, -type <name part>, -object <hex address>, -task <hex address of an object in a task graph>,
    /// -threadsView <name> (Stacks, Thread pool, Locks), -networkView <name> (Connections, Requests, HttpClient pools),
    /// -filesView <name> (Open files, Paths in native objects, Mapped files).
    private func applyStartSelection(_ s: HeapSnapshot) {
        let d = UserDefaults.standard
        if let name = d.string(forKey: "section"), let section = Section(rawValue: name) { self.section = section }
        if let name = d.string(forKey: "heapView"), let view = HeapView(rawValue: name) { heapView = view }
        if let name = d.string(forKey: "threadsView"), let mode = ThreadsView.Mode(rawValue: name) { threadsMode = mode }
        if let name = d.string(forKey: "networkView"), let mode = NetworkView.Mode(rawValue: name) { networkMode = mode }
        if let name = d.string(forKey: "filesView"), let mode = FilesView.Mode(rawValue: name) { filesMode = mode }
        if let name = d.string(forKey: "finding") { finding = name }
        if let text = d.string(forKey: "search") { searchQuery = text; showSearch = true }
        func address(_ key: String) -> UInt64? { d.string(forKey: key).flatMap { UInt64($0.replacingOccurrences(of: "0x", with: ""), radix: 16) } }
        if let a = address("region") { selectedRegion = s.heap.region(containing: a) }
        if let t = d.string(forKey: "type") {
            selectedType = s.typeStats.filter { $0.name.contains(t) }.max { $0.retained < $1.retained }?.typeId
        }
        if let a = address("object"), let i = s.heap.object(containing: a) { select(object: i) }
        if let a = address("task"), let i = s.heap.object(containing: a) { show(taskOf: i, in: s) }
    }

    /// The Tasks page with the graph that an object belongs to.
    func show(taskOf object: Int, in s: HeapSnapshot) {
        Task {
            do {
                let graphs = try await Task.detached { try s.taskGraphs }.value
                guard snapshot === s, let (g, k) = graphs.graph(containing: object) else { return }
                section = .tasks
                taskGroup = graphs.groups[g].id
                taskGraph = graphs.groups[g].graphs[k].id
                taskNode = nil
            } catch {
                if snapshot === s { self.error = analysisErrorMessage(error) }
            }
        }
    }

    func show(region: Int) {
        navigating = true
        section = .managedHeap
        navigating = false
        if heapView == .statistics { heapView = .map }
        selectedRegion = region
    }

    func show(finding name: String) {
        finding = name
        section = .findings
    }

    /// The page for a summary finding's command: a check name, or the page of the CLI command named.
    func show(see: String?) {
        switch see {
        case "retained": section = .dominators
        case "garbage": section = .garbage
        case "fragmentation": section = .fragmentation
        case "finalizable": show(finding: "finalizer-backlog")
        case let see? where !see.hasPrefix("native"): show(finding: see)
        default: section = .native
        }
    }

    func show(_ target: SearchResult.Target, in s: HeapSnapshot) {
        switch target {
        case let .finding(see): show(see: see)
        case let .check(name): show(finding: name)
        case let .type(t): section = .types; selectedType = t
        case let .nativeGroup(id): section = .native; nativeMode = .allocations; selectedChunkGroup = id
        case let .object(i): select(object: i)
        case let .region(r): show(region: r)
        case let .chunk(c): select(chunk: c)
        }
    }

    func showNativeGroup(containing chunk: Int, in snapshot: HeapSnapshot) {
        Task {
            do {
                let group = try await Task.detached { try snapshot.chunkGroup(containing: chunk) }.value
                guard self.snapshot === snapshot, let group else { return }
                section = .native
                nativeMode = .allocations
                selectedChunkGroup = group
            } catch {
                if self.snapshot === snapshot { self.error = analysisErrorMessage(error) }
            }
        }
    }

    /// Downloads the debug files of the runtime libraries; the dump stays open and in use.
    func downloadSymbols() {
        guard let s = snapshot, symbolDownload == nil else { return }
        Task {
            let shown = await updateSymbols(s, automatic: false)
            if shown !== s { await runChecks(shown) }
        }
    }

    /// Downloads the debug files of the runtime libraries of the open dump. When a file is new, opens the dump again from
    /// the cache, so that native frames get names, and shows it on the same page. Returns the snapshot shown.
    private func updateSymbols(_ s: HeapSnapshot, automatic: Bool) async -> HeapSnapshot {
        symbolPhase = ""; symbolFraction = 0
        let download = Task.detached(priority: .utility) {
            s.downloadRuntimeSymbols(automatic: automatic, isCancelled: { Task.isCancelled }, progress: { text, fraction in
                Task { @MainActor in if self.snapshot === s { self.symbolPhase = text; self.symbolFraction = fraction } }
            })
        }
        symbolDownload = download
        let results = await download.value
        if snapshot === s { symbolDownload = nil }
        guard results.contains(where: { $0.result == "downloaded" }), snapshot === s, let dump = dumpPath else { return s }
        let smaps = smapsPath, dlls = dllFolder, options = options
        do {
            let reopened = try await Task.detached(priority: .userInitiated) {
                try HeapSnapshot(path: dump, dllFolders: dlls.map { [$0] } ?? [], smapsPath: smaps, options: options)
            }.value
            guard snapshot === s else { return s }
            snapshot = reopened
            reports = nil
            summaryFindings = nil
            return reopened
        } catch {
            if snapshot === s { self.error = analysisErrorMessage(error) }
            return s
        }
    }

    private func runChecks(_ s: HeapSnapshot) async {
        guard snapshot === s else { return }
        do {
            let integrity = try await Task.detached(priority: .userInitiated) { try s.verificationCheck() }.value
            if snapshot === s { integrityReport = integrity }
            let reports = try await Task.detached(priority: .userInitiated) { try s.memo("all checks") { try s.allChecks() } }.value
            if snapshot === s { self.reports = reports }
            let findings = try await Task.detached(priority: .userInitiated) { try s.summaryFindings() }.value
            if snapshot === s { summaryFindings = findings }
        } catch {
            if snapshot === s { self.error = analysisErrorMessage(error) }
        }
    }

    func select(chunk: Int) {
        inspectedObject = .chunk(chunk)
        showInspector = true
    }

    func select(object i: Int) {
        inspectedObject = .object(i)
        showInspector = true
    }

    /// Records the place after each change; one observation of the place is armed at a time.
    private func trackPlace() {
        let now = place
        if let last = recorded, last != now {
            backStack.append(last)
            if backStack.count > 500 { backStack.removeFirst() }
            forwardStack.removeAll()
        }
        recorded = now
        armed = true
        withObservationTracking { _ = place } onChange: { [weak self] in
            Task { @MainActor [weak self] in self?.armed = false; self?.trackPlace() }
        }
    }
    @ObservationIgnored private var armed = false

    /// Starts the history at the place a dump opens to.
    private func startHistory() {
        recorded = place
        if !armed { trackPlace() }
    }

    func back() {
        guard let previous = backStack.popLast() else { return }
        forwardStack.append(place)
        recorded = previous
        place = previous
    }

    func forward() {
        guard let next = forwardStack.popLast() else { return }
        backStack.append(place)
        recorded = next
        place = next
    }
}
