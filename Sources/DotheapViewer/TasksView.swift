import HeapCore
import SwiftUI

/// Waiting async calls as task graphs: graphs of the same shape as one row, the graphs of a row, and one graph as a tree
/// of its calls, from the outermost call down to what the innermost calls await, with the objects of each call.
struct TasksView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var state: TaskGraphs.Group.State?
    @State private var text = ""
    @State private var sort = Sort.tasks
    @State private var open: Set<Int> = []
    @State private var limits: [Int: Int] = [:]

    enum Sort: String, CaseIterable, Identifiable {
        case tasks = "Tasks", stack = "Async stack", state = "State"
        var id: String { rawValue }
    }

    var body: some View {
        Computed(snapshot, key: "task graphs", label: "Building the task graphs", compute: { try $0.taskGraphs }) { model in
            content(model)
        }
    }

    private func content(_ model: TaskGraphs) -> some View {
        @Bindable var session = session
        let groups = model.groups.filter { g in
            (state == nil || g.state == state) && (text.isEmpty || g.summary.localizedCaseInsensitiveContains(text))
        }
        func total(_ s: TaskGraphs.Group.State) -> Int { model.groups.filter { $0.state == s }.reduce(0) { $0 + $1.outermostCalls } }
        let shared = sorted(groups.filter(\.isShared))
        let own = sorted(groups.filter { !$0.isShared })
        let sharedAll = model.groups.filter(\.isShared)
        return VStack(alignment: .leading, spacing: 10) {
            Text("Tasks").font(.title2.weight(.semibold))
            Text("Each row groups unfinished calls with the same async stack, from the outermost call to the innermost running or awaited operation. Shared waits (several tasks awaiting one object) appear before tasks with separate waits. Expand a row to see each awaited object, then select a call to inspect its state-machine fields and paths from roots.")
                .foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            MetricRow {
                Metric(title: "Tasks", value: model.groups.reduce(0) { $0 + $1.outermostCalls }.formatted(), detail: "\(model.groups.count.formatted()) async stacks")
                Metric(title: "Running", value: total(.running).formatted(), detail: "a thread runs a call", color: Palette.live, hideZero: true)
                Metric(title: "Queued", value: total(.queued).formatted(), detail: "ready, wait for a pool thread", color: Palette.queued, hideZero: true)
                Metric(title: "Waiting", value: total(.waiting).formatted(), detail: "something can resume them", hideZero: true)
                Metric(title: "Never resume", value: total(.neverResumes).formatted(), detail: "only the calls hold what they await", color: Palette.dead, hideZero: true)
                Metric(title: "Wait on each other", value: total(.cycle).formatted(), detail: "tasks in cycles", color: Palette.dead, hideZero: true)
                Metric(title: "Dropped", value: total(.dropped).formatted(), detail: "dead while waiting", color: Palette.free, hideZero: true)
                Metric(title: "Shared waits", value: sharedAll.reduce(0) { $0 + $1.graphs.count }.formatted(),
                       detail: "objects that \(sharedAll.reduce(0) { $0 + $1.outermostCalls }) tasks wait on together", color: Color.accentColor, hideZero: true)
            }
            FlowLayout(spacing: 8, lineSpacing: 8) {
                statePicker(model)
                filterAndSort
            }
            let group = model.groups.first(where: { $0.id == session.taskGroup })
            SplitPanes(axis: .horizontal, firstMin: 360, secondMin: 420, initial: 0.5) {
                List(selection: Binding(get: { Row.id(group: session.taskGroup, graph: session.taskGraph) }, set: select)) {
                    if !shared.isEmpty {
                        SwiftUI.Section("One object that several tasks wait on (\(shared.reduce(0) { $0 + $1.outermostCalls }) tasks)") {
                            ForEach(rows(shared)) { listRow($0) }
                        }
                    }
                    SwiftUI.Section("Each task waits on its own object (\(own.reduce(0) { $0 + $1.outermostCalls }) tasks)") {
                        ForEach(rows(own)) { listRow($0) }
                    }
                }
            } second: {
                if let group {
                    let graph = group.graphs.first { $0.id == session.taskGraph } ?? group.graphs[0]
                    TaskGraphView(snapshot: snapshot, graph: graph)
                } else {
                    Placeholder("Select a row to see the objects its tasks wait on.")
                }
            }
        }
        .padding(16)
        .onChange(of: session.taskGroup) { _, id in
            guard let group = model.groups.first(where: { $0.id == id }) else { return }
            if state != nil && state != group.state { state = nil }
            if !text.isEmpty && !group.summary.localizedCaseInsensitiveContains(text) { text = "" }
        }
        .onChange(of: session.taskGraph, initial: true) {
            if session.taskGraph != nil, let group = session.taskGroup { open.insert(group) }
        }
    }

    /// A tab for each state that the dump's tasks are in.
    private func statePicker(_ model: TaskGraphs) -> some View {
        let states = [TaskGraphs.Group.State.running, .queued, .waiting, .neverResumes, .cycle, .dropped].filter { s in model.groups.contains { $0.state == s } }
        return Picker("Show", selection: $state) {
            Text("All").tag(TaskGraphs.Group.State?.none)
            ForEach(states, id: \.self) { Text(Self.title($0)).tag(TaskGraphs.Group.State?.some($0)) }
        }
        .labelsHidden().pickerStyle(.segmented)
    }

    private var filterAndSort: some View {
        HStack(spacing: 8) {
            TextField("Filter by method or type", text: $text).textFieldStyle(.roundedBorder).frame(minWidth: 160, maxWidth: 320)
            Picker("Sort by", selection: $sort) { ForEach(Sort.allCases) { Text($0.rawValue).tag($0) } }.fixedSize()
        }
    }

    /// A row of the list: an async stack, one object that its tasks wait on when the stack has more than one, or the
    /// objects of an open stack that are not shown yet.
    struct Row: Identifiable {
        let group: TaskGraphs.Group
        let graph: TaskGraphs.Graph?
        var more = 0
        let id: String

        init(group: TaskGraphs.Group, graph: TaskGraphs.Graph?, more: Int = 0) {
            self.group = group
            self.graph = graph
            self.more = more
            id = more > 0 ? "\(group.id)/more" : Self.id(group: group.id, graph: graph?.id) ?? ""
        }

        static func id(group: Int?, graph: Int?) -> String? {
            group.map { g in graph.map { "\(g)/\($0)" } ?? "\(g)" }
        }
    }

    /// The rows of the stacks, each open stack followed by its first objects (100, and 200 more for each click on the
    /// last row). NSOutlineView through OutlineGroup takes time quadratic in the children of a row to open it.
    private func rows(_ groups: [TaskGraphs.Group]) -> [Row] {
        var out: [Row] = []
        for g in groups {
            out.append(Row(group: g, graph: nil))
            guard g.graphs.count > 1, open.contains(g.id) else { continue }
            let limit = limits[g.id] ?? 100
            out += g.graphs.prefix(limit).map { Row(group: g, graph: $0) }
            if g.graphs.count > limit { out.append(Row(group: g, graph: nil, more: g.graphs.count - limit)) }
        }
        return out
    }

    @ViewBuilder private func listRow(_ row: Row) -> some View {
        if row.more > 0 {
            Button { limits[row.group.id, default: 100] += 200 } label: {
                Text("\(row.more.formatted()) more…").foregroundStyle(Color.accentColor)
            }
            .buttonStyle(.plain).padding(.leading, 38)
        } else {
            HStack(spacing: 4) {
                if row.graph == nil {
                    Button {
                        if open.contains(row.group.id) { open.remove(row.group.id) } else { open.insert(row.group.id) }
                    } label: {
                        Image(systemName: "chevron.right")
                            .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                            .rotationEffect(.degrees(open.contains(row.group.id) ? 90 : 0))
                            .frame(width: 18, height: 18).contentShape(Rectangle())
                    }
                    .buttonStyle(.plain).opacity(row.group.graphs.count > 1 ? 1 : 0).disabled(row.group.graphs.count < 2)
                } else {
                    Color.clear.frame(width: 34, height: 1)
                }
                TaskRow(row: row)
            }
            .tag(row.id)
        }
    }

    private func sorted(_ groups: [TaskGraphs.Group]) -> [TaskGraphs.Group] {
        switch sort {
        case .tasks: groups
        case .stack: groups.sorted { ($0.outermost.localizedLowercase, $0.id) < ($1.outermost.localizedLowercase, $1.id) }
        case .state: groups.sorted { (Self.title($0.state), $1.outermostCalls, $0.id) < (Self.title($1.state), $0.outermostCalls, $1.id) }
        }
    }

    private func select(_ id: String?) {
        let parts = id?.split(separator: "/").compactMap { Int($0) } ?? []
        session.taskGroup = parts.first
        session.taskGraph = parts.count > 1 ? parts[1] : nil
        session.taskNode = nil
    }

    static func color(_ s: TaskGraphs.Group.State) -> Color {
        switch s {
        case .running: Palette.live
        case .queued: Palette.queued
        case .waiting: .secondary
        case .neverResumes, .cycle: Palette.dead
        case .dropped: Palette.free
        }
    }

    static func title(_ s: TaskGraphs.Group.State) -> String {
        switch s {
        case .running: "Running"
        case .queued: "Queued"
        case .waiting: "Waiting"
        case .neverResumes: "Never resume"
        case .cycle: "Wait on each other"
        case .dropped: "Dropped"
        }
    }
}

private struct TaskRow: View {
    let row: TasksView.Row

    var body: some View {
        if let graph = row.graph {
            HStack {
                Text(graph.name).lineLimit(1).truncationMode(.middle).help(graph.name)
                Spacer(minLength: 8)
                Text(graph.tasks == 1 ? "1 task" : "\(graph.tasks) tasks").font(.caption).monospacedDigit().foregroundStyle(.secondary)
            }
        } else {
            let g = row.group
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                HStack(spacing: 4) {
                    Text(g.outermostCalls.formatted()).monospacedDigit()
                    if g.isShared {
                        Image(systemName: "arrow.triangle.merge").foregroundStyle(Color.accentColor)
                            .help("\(g.outermostCalls) tasks wait on \(g.graphs.count == 1 ? "one object" : "\(g.graphs.count) objects") together")
                    }
                }
                .frame(minWidth: 28, alignment: .trailing)
                VStack(alignment: .leading, spacing: 1) {
                    Text(g.outermost).lineLimit(2)
                    Text("waits on " + g.suspendedOn + (g.graphs.count > 1 ? ", \(g.graphs.count) objects" : ""))
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                }
                .help(g.summary)
                Spacer(minLength: 8)
                Text(TasksView.title(g.state)).font(.caption).foregroundStyle(TasksView.color(g.state))
            }
            .padding(.vertical, 1)
        }
    }
}

/// One graph as a tree of calls, and the call selected in it.
struct TaskGraphView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let graph: TaskGraphs.Graph
    @AppStorage("mergeTaskCalls") private var merged = true

    private var roots: RustArray<TaskGraphs.Node> { merged ? graph.roots : graph.trees }

    struct Line: Identifiable {
        let path: [Int]
        let node: TaskGraphs.Node
        var id: [Int] { path }
    }

    private var lines: [Line] {
        var out: [Line] = []
        func walk(_ nodes: RustArray<TaskGraphs.Node>, _ path: [Int]) {
            for (k, n) in nodes.enumerated() {
                out.append(Line(path: path + [k], node: n))
                walk(n.children, path + [k])
            }
        }
        walk(roots, [])
        return out
    }

    private func node(at path: [Int]) -> TaskGraphs.Node? {
        var list = roots, node: TaskGraphs.Node?
        for k in path {
            guard k < list.count else { return nil }
            node = list[k]
            list = list[k].children
        }
        return node
    }

    var body: some View {
        let selected = session.taskNode ?? [0]
        SplitPanes(axis: .vertical, firstMin: 160, secondMin: 220, initial: 0.55, showSecond: node(at: selected) != nil) {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if graph.isShared, let object = graph.waitedOn {
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Image(systemName: "arrow.triangle.merge").foregroundStyle(Color.accentColor)
                            Text("\(graph.tasks) tasks wait on one object").fontWeight(.semibold)
                            ObjectLink(snapshot: snapshot, object: object)
                        }
                        .padding(8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.accentColor.opacity(0.12), in: RoundedRectangle(cornerRadius: 6))
                        .padding(.bottom, 6)
                    }
                    HStack {
                        Text(graph.name).font(.headline).textSelection(.enabled)
                        Spacer()
                        Toggle("Merge calls with the same text", isOn: Binding(get: { merged }, set: { merged = $0; session.taskNode = nil }))
                            .toggleStyle(.checkbox).font(.caption)
                    }
                    .padding(.bottom, 6)
                    ForEach(lines) { line in
                        TaskNodeRow(node: line.node, depth: line.path.count - 1, selected: line.path == selected)
                            .contentShape(Rectangle())
                            .onTapGesture { session.taskNode = line.path }
                    }
                }
                .padding(PageInsets.detail)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        } second: {
            if let n = node(at: selected) {
                TaskNodeDetail(snapshot: snapshot, node: n).id(selected)
            }
        }
    }
}

private struct TaskNodeRow: View {
    let node: TaskGraphs.Node
    let depth: Int
    let selected: Bool

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Color.clear.frame(width: CGFloat(depth) * 18, height: 1)
            Image(systemName: symbol).foregroundStyle(color).frame(width: 16)
            VStack(alignment: .leading, spacing: 1) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(node.kind == .thread ? "Thread waits synchronously" : node.title).fontWeight(node.kind == .method ? .semibold : .regular)
                    if !node.detail.isEmpty, node.kind != .suspension { Text(node.detail).foregroundStyle(.secondary) }
                    if node.objects.count > 1 {
                        Text("×\(node.objects.count)").font(.caption.monospacedDigit().weight(.semibold)).foregroundStyle(.white)
                            .padding(.horizontal, 6).background(Color.accentColor, in: Capsule())
                    }
                }
                if node.kind == .suspension, !node.detail.isEmpty {
                    Text(node.detail).font(.caption).foregroundStyle(node.neverResumes ? Palette.dead : .secondary)
                }
                if let held = node.heldBy { Text(held).font(.caption).foregroundStyle(.secondary) }
                if !node.threads.isEmpty {
                    Text((node.running ? "on " : "") + node.threads.prefix(3).joined(separator: ", ") + (node.threads.count > 3 ? ", ..." : ""))
                        .font(.caption).foregroundStyle(node.running ? Palette.live : .secondary)
                }
            }
            .lineLimit(2)
            Spacer(minLength: 0)
        }
        .padding(.vertical, 3).padding(.horizontal, 4)
        .background(selected ? Color.accentColor.opacity(0.18) : .clear, in: RoundedRectangle(cornerRadius: 5))
    }

    private var symbol: String {
        switch node.kind {
        case .method: node.running ? "play.circle" : "function"
        case .task: node.title.hasPrefix("Task.WhenAll") || node.title.hasPrefix("Task.WhenAny") ? "arrow.triangle.branch" : "square.stack"
        case .thread: "cpu"
        case .suspension: "pause.circle"
        case .cycle: "arrow.triangle.2.circlepath"
        }
    }

    private var color: Color {
        switch node.kind {
        case .suspension: node.neverResumes ? Palette.dead : .accentColor
        case .cycle: Palette.dead
        case .method: node.running ? Palette.live : .secondary
        default: .secondary
        }
    }
}

/// The objects of a call: each object it merges, the state machine of an async method, what holds the outermost call's
/// task, what an innermost call awaits, and the path from a root to the object.
private struct TaskNodeDetail: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let node: TaskGraphs.Node
    @State private var index = 0

    var body: some View {
        let object = node.objects[min(index, node.objects.count - 1)]
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(node.kind == .thread ? "Thread waits synchronously" : node.title).font(.headline).textSelection(.enabled)
                    if !node.detail.isEmpty, node.kind != .suspension { Text(node.detail).foregroundStyle(.secondary) }
                    Spacer()
                    if node.objects.count > 1 {
                        Button { index = max(0, index - 1) } label: { Image(systemName: "chevron.left") }.disabled(index == 0)
                        Text("\(index + 1) of \(node.objects.count)").monospacedDigit().foregroundStyle(.secondary)
                        Button { index = min(node.objects.count - 1, index + 1) } label: { Image(systemName: "chevron.right") }.disabled(index == node.objects.count - 1)
                    }
                }
                Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                    switch node.kind {
                    case .method:
                        link("Box", object)
                        if node.running, index < node.threads.count { GridRow { Text("Runs on").foregroundStyle(.secondary); Text(node.threads[index]) } }
                        Computed(snapshot, key: "state fields \(object)", label: "Reading the fields", compute: { try $0.stateMachineFields(ofBox: object) }) { fields in
                        ForEach(Array(fields.enumerated()), id: \.offset) { _, f in
                            if let name = TaskGraphs.fieldName(f.field.field.name) {
                                GridRow {
                                    Text(name).foregroundStyle(.secondary)
                                    if !f.objects.isEmpty {
                                        VStack(alignment: .leading, spacing: 2) { ForEach(f.objects, id: \.self) { ObjectLink(snapshot: snapshot, object: $0, showSize: false) } }
                                    } else if f.field.target == nil, f.field.display.allSatisfy({ $0 == "0" || $0 == " " }) {
                                        Text("empty").foregroundStyle(.tertiary)
                                    } else {
                                        Text(f.field.display).textSelection(.enabled)
                                    }
                                }
                            }
                        }
                        }
                    case .thread:
                        link("Waits in", object)
                        if index < node.threads.count { GridRow { Text("Thread").foregroundStyle(.secondary); Text(node.threads[index]) } }
                    case .suspension:
                        link("Awaits", object)
                        if node.neverResumes {
                            GridRow { Text("").gridCellUnsizedAxes(.horizontal); Text("Only the calls above hold this object: nothing else can complete it.").foregroundStyle(Palette.dead) }
                        }
                        ForEach(node.resources, id: \.self) { link("On", $0) }
                        if !node.detail.isEmpty, !node.neverResumes { GridRow { Text("Held").foregroundStyle(.secondary); Text(node.detail) } }
                        ForEach(node.holders, id: \.self) { link("Holder", $0) }
                    case .task, .cycle:
                        link("Task", object)
                    }
                    if let held = node.heldBy {
                        GridRow { Text("Task").foregroundStyle(.secondary); Text(held) }
                        ForEach(node.holders, id: \.self) { link("Holder", $0) }
                    }
                }
                RootPath(snapshot: snapshot, object: object)
            }
            .padding(PageInsets.detail)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func link(_ title: String, _ object: Int) -> some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            ObjectLink(snapshot: snapshot, object: object)
        }
    }
}

/// The shortest path from a root to an object: what keeps it alive.
struct RootPath: View {
    let snapshot: HeapSnapshot
    let object: Int
    /// Types the path does not pass through, and how the title names them.
    var avoiding: Set<String> = []
    var avoidingText = ""

    struct Model {
        let live: Bool
        let avoidedAll: Bool
        let root: String
        let local: String?
        let objects: [Int]
        let fields: [String?]
    }

    var body: some View {
        let key = "root path \(object) " + avoiding.sorted().joined(separator: ",")
        VStack(alignment: .leading, spacing: 3) {
            Text("Path from a root").font(.subheadline.weight(.semibold))
            if !avoidingText.isEmpty { Text("Paths through \(avoidingText) are skipped.").font(.caption).foregroundStyle(.secondary) }
            Computed(snapshot, key: key, label: "Finding the path from a root", compute: { [object, avoiding] s in try Self.model(s, object, avoiding) }) { m in
                content(m)
            }
            .id(key)
        }
    }

    private static func model(_ s: HeapSnapshot, _ object: Int, _ avoiding: Set<String>) throws -> Model {
        guard s.graph.live[object] else { return Model(live: false, avoidedAll: false, root: "", local: nil, objects: [], fields: []) }
        let avoided = avoiding.isEmpty ? nil : try s.pathToRoot(object, avoiding: avoiding)
        let p = avoided ?? s.graph.pathToRoot(object)
        return Model(live: true, avoidedAll: !avoiding.isEmpty && avoided == nil, root: p.source.map { s.describe(source: $0) } ?? "none",
                     local: try p.objects.first.flatMap(s.stackHolderText), objects: Array(p.objects),
                     fields: try p.objects.indices.map { k in k > 0 ? try s.referenceName(from: p.objects[k - 1], to: p.objects[k]) : nil })
    }

    @ViewBuilder private func content(_ m: Model) -> some View {
        if !m.live {
            Text("Dead: nothing keeps it alive.").foregroundStyle(.secondary)
        } else {
            if m.avoidedAll {
                Text("All paths from a root go through \(avoidingText). Nothing else keeps the object alive. This is the shortest path:").foregroundStyle(.secondary)
            }
            Text("Root: " + m.root).foregroundStyle(.secondary).textSelection(.enabled)
            if let local = m.local {
                Text("The object below is " + local + " (the frame is estimated from return addresses).").font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
            }
            ForEach(Array(m.objects.enumerated()), id: \.offset) { k, o in
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    ObjectLink(snapshot: snapshot, object: o, showSize: false)
                    if let name = m.fields[k] {
                        Text(name).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    }
                }
                .padding(.leading, CGFloat(min(k, 12)) * 8)
            }
        }
    }
}
