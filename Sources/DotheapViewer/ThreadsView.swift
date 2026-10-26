import HeapCore
import SwiftUI

/// Threads by the function they run and what they do now, the thread pool and its queue, and the locks that threads
/// hold and wait for.
struct ThreadsView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot

    enum Mode: String, CaseIterable, Identifiable {
        case stacks = "Stacks", pool = "Thread pool", locks = "Locks"
        var id: String { rawValue }
    }

    struct Model {
        let states: [Int32: HeapSnapshot.ThreadState]
        let groups: [Node]
        let pool: CheckReport
        let locks: CheckReport
        let queued: Int
    }

    /// A row of the list: the function some threads run, what some of them do now, or one thread.
    struct Node: Identifiable {
        let id: String
        let title: String
        /// What the threads do now, for a function whose threads all do the same; for a thread, its state and wait.
        let detail: String
        let threads: [Int32]
        var thread: Int32?
        let children: [Node]?
    }

    var body: some View {
        Computed(snapshot, key: "threads page", label: "Reading the threads", compute: { s in
            let states = Dictionary(uniqueKeysWithValues: try s.threadStates().map { ($0.tid, $0) })
            let names = Dictionary(try s.threadStacks.threads.map { ($0.tid, $0.name ?? "") }, uniquingKeysWith: { a, _ in a })
            func threads(_ list: some Collection<Int32>) -> [Node]? {
                list.count < 2 ? nil : list.map { t in
                    Node(id: "t\(t)", title: names[t].map { $0.isEmpty ? "thread \(t)" : "thread \(t) (\($0))" } ?? "thread \(t)",
                         detail: Self.detail(states[t]), threads: [t], thread: t, children: nil)
                }
            }
            let groups = try s.threadGroups().map { g in
                g.activities.count == 1
                    ? Node(id: g.function, title: g.function, detail: g.activities[0].title, threads: Array(g.threads), children: threads(g.threads))
                    : Node(id: g.function, title: g.function, detail: "", threads: Array(g.threads),
                           children: g.activities.map { Node(id: g.function + "\n" + $0.title, title: $0.title, detail: "", threads: Array($0.threads), children: threads($0.threads)) })
            }
            return try Model(states: states, groups: groups, pool: s.blockedThreadsCheck(), locks: s.lockCheck(), queued: s.queuedWorkItems().count)
        }) { model in
            content(model)
        }
    }

    private func content(_ model: Model) -> some View {
        @Bindable var session = session
        let states = Array(model.states.values)
        let pool = states.filter(\.pool)
        let queued = model.queued
        let deadlocks = model.locks.sections.first { $0.title == "Deadlocks" }?.rows.reduce(Int64(0)) { n, r in if case let .int(k) = r.cells[0] { return n + k }; return n } ?? 0
        return VStack(alignment: .leading, spacing: 10) {
            Text("Threads").font(.title2.weight(.semibold))
            Text("Threads are grouped at the first frame where their stacks differ, below common startup frames. Expand a group to see each thread's current function or wait and its application caller. Managed threads use managed frames; other threads use named native frames, improved by runtime debug files. Frames are estimated from return addresses and can include stale calls. Thread pool and Locks show queued work, held locks and inferred waiters.")
                .foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            MetricRow {
                Metric(title: "Threads", value: states.count.formatted(), detail: "\(states.filter(\.managed).count.formatted()) with managed frames")
                Metric(title: "Pool workers", value: pool.count.formatted(),
                       detail: "\(pool.filter { $0.wait != nil }.count) blocked, \(pool.filter(\.idle).count) idle", hideZero: true)
                Metric(title: "Queued work", value: queued.formatted(), detail: "items wait for a pool thread", color: queued > 0 ? Palette.queued : nil, hideZero: true)
                Metric(title: "Blocked", value: states.filter { $0.wait != nil && $0.waitsForLock == nil }.count.formatted(), detail: "in synchronous waits", hideZero: true)
                Metric(title: "Wait for locks", value: states.filter { $0.waitsForLock != nil }.count.formatted(), detail: "\(deadlocks) in deadlocks",
                       color: deadlocks > 0 ? Palette.dead : nil, hideZero: true)
            }
            Picker("Show", selection: $session.threadsMode) { ForEach(Mode.allCases) { Text($0.rawValue).tag($0) } }
                .labelsHidden().pickerStyle(.segmented).fixedSize()
            switch session.threadsMode {
            case .stacks: StackTreeView(snapshot: snapshot, model: model)
            case .pool: ReportView(snapshot: snapshot, report: model.pool)
            case .locks: ReportView(snapshot: snapshot, report: model.locks)
            }
        }
        .padding(16)
    }

    static func color(_ state: String) -> Color {
        switch state {
        case "waits for a lock", "blocked": Palette.dead
        case "idle", "sleeps", "native code": .secondary
        default: .primary
        }
    }

    /// "blocked, Monitor.Wait in Worker.Run, holds 2 locks".
    static func detail(_ t: HeapSnapshot.ThreadState?) -> String {
        guard let t else { return "other" }
        let waits = t.waitsForLock != nil ? nil : t.wait.map { $0 + (t.waitFrame.map { $0 == "-" ? "" : " in " + $0 } ?? "") }
        let holds = t.holdsLocks.isEmpty ? nil : t.holdsLocks.count == 1 ? "holds 1 lock" : "holds \(t.holdsLocks.count) locks"
        return [t.state, waits, holds].compactMap { $0 }.joined(separator: ", ")
    }

    /// "14 blocked, 2 idle".
    static func summary(_ threads: [Int32], _ states: [Int32: HeapSnapshot.ThreadState]) -> [(state: String, count: Int)] {
        var counts = [String: Int]()
        for t in threads { counts[states[t]?.state ?? "other", default: 0] += 1 }
        return counts.sorted { ($0.value, $1.key) > ($1.value, $0.key) }.map { ($0.key, $0.value) }
    }
}

/// The threads by function and activity, each opening into its threads, and the selected thread.
private struct StackTreeView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let model: ThreadsView.Model

    var body: some View {
        @Bindable var session = session
        let tid = find(session.threadStack, in: model.groups).flatMap { $0.thread ?? $0.threads.first }
        SplitPanes(axis: .horizontal, firstMin: 360, secondMin: 360, initial: 0.5) {
            List(model.groups, children: \.children, selection: $session.threadStack) { node in
                StackRow(node: node, states: model.states).tag(node.id)
            }
        } second: {
            if let tid {
                ThreadDetail(snapshot: snapshot, tid: tid, state: model.states[tid]).id(tid)
            } else {
                Placeholder("Select a row to see its threads.")
            }
        }
        .onAppear { if session.threadStack == nil { session.threadStack = model.groups.first?.id } }
    }

    private func find(_ id: String?, in nodes: [ThreadsView.Node]) -> ThreadsView.Node? {
        guard let id else { return nil }
        for n in nodes {
            if n.id == id { return n }
            if let found = find(id, in: n.children ?? []) { return found }
        }
        return nil
    }
}

private struct StackRow: View {
    let node: ThreadsView.Node
    let states: [Int32: HeapSnapshot.ThreadState]

    var body: some View {
        if let t = node.thread {
            VStack(alignment: .leading, spacing: 1) {
                Text(node.title).lineLimit(1).truncationMode(.middle)
                Text(node.detail).font(.caption).foregroundStyle(ThreadsView.color(states[t]?.state ?? "other")).lineLimit(1).truncationMode(.middle)
            }
            .help(node.detail)
            .padding(.vertical, 1)
        } else {
            group
        }
    }

    private var group: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(node.threads.count.formatted()).monospacedDigit().frame(minWidth: 28, alignment: .trailing)
            VStack(alignment: .leading, spacing: 1) {
                Text(node.title).lineLimit(1).truncationMode(.middle).help(node.title)
                if !node.detail.isEmpty, node.detail != node.title {
                    Text(node.detail).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle).help(node.detail)
                }
                HStack(spacing: 6) {
                    ForEach(ThreadsView.summary(node.threads, states), id: \.state) { s in
                        Text("\(s.count) \(s.state)").font(.caption).foregroundStyle(ThreadsView.color(s.state))
                    }
                }
            }
        }
        .padding(.vertical, 1)
    }
}

/// One thread: what it waits for, the locks it holds, and its whole stack.
private struct ThreadDetail: View {
    let snapshot: HeapSnapshot
    let tid: Int32
    let state: HeapSnapshot.ThreadState?

    struct Model {
        let label: String
        let thread: ThreadStacks.ThreadInfo?
        /// The function of each frame, outermost first.
        let frames: [String]
    }

    var body: some View {
        Computed(snapshot, key: "thread \(tid)", label: "Reading the stack", compute: { [tid] s in
            let stacks = try s.threadStacks, thread = stacks.thread(tid)
            return Model(label: stacks.label(tid), thread: thread,
                         frames: thread.map { t in ([t.instructionPointer] + t.frames.map(\.returnAddress)).reversed().map(stacks.describe) } ?? [])
        }) { m in
            content(m)
        }
    }

    private func content(_ m: Model) -> some View {
        let thread = m.thread
        return ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(m.label).font(.headline).textSelection(.enabled)
                    Text(state?.state ?? "other").foregroundStyle(ThreadsView.color(state?.state ?? "other"))
                }
                Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                    if let lock = state?.waitsForLock {
                        GridRow { Text("Waits for").foregroundStyle(.secondary); ObjectLink(snapshot: snapshot, object: lock) }
                    } else if let wait = state?.wait {
                        GridRow { Text("Blocked in").foregroundStyle(.secondary); Text(wait + (state?.waitFrame.map { $0 == "-" ? "" : " in " + $0 } ?? "")).textSelection(.enabled) }
                    }
                    if let held = state?.holdsLocks, !held.isEmpty {
                        GridRow {
                            Text("Holds").foregroundStyle(.secondary)
                            VStack(alignment: .leading, spacing: 2) { ForEach(held, id: \.self) { ObjectLink(snapshot: snapshot, object: $0) } }
                        }
                    }
                    if let o = thread?.managedThread {
                        GridRow { Text("Thread object").foregroundStyle(.secondary); ObjectLink(snapshot: snapshot, object: o) }
                    }
                    if state?.pool == true { GridRow { Text("Pool").foregroundStyle(.secondary); Text(state?.idle == true ? "idle worker" : "worker") } }
                    if let thread { GridRow { Text("Stack").foregroundStyle(.secondary); Text("\(Format.bytes(thread.inUseBytes)) in use, \(Format.bytes(thread.touchedBytes)) touched") } }
                }
                .font(.callout)
                if thread != nil {
                    Text("Stack, outermost first").font(.subheadline.weight(.semibold))
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(Array(m.frames.enumerated()), id: \.offset) { _, f in
                            Text(f).font(.callout.monospaced()).lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                        }
                    }
                }
            }
            .padding(PageInsets.detail)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
