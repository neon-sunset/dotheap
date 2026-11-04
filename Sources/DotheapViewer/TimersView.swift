import HeapCore
import SwiftUI

/// Live timers grouped by what made them, what they call or what awaits them, and what holds them; the timers of the
/// selected group, and the selected timer with the path from a root to the object that holds it.
struct TimersView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var text = ""
    @State private var order: TableSort? = TableSort(column: "Timers", ascending: false)

    struct Model {
        let timers: HeapSnapshot.Timers
        /// Each timer of a group with more than one, named by the part of its path from a root that differs.
        let places: [Int: String]
    }

    struct TimerGroup: Identifiable {
        let id: String
        let timers: [HeapSnapshot.TimerInfo]
        var first: HeapSnapshot.TimerInfo { timers[0] }
        var count: Int { timers.count }
        var kind: String { first.kind }
        var does: String { first.awaitedBy ?? first.callback }
        var heldBy: String { first.owner != nil && first.owner == first.awaitingCall ? "the state machine of this call" : first.ownerText }
        var period: String { id.split(separator: "\t", omittingEmptySubsequences: false).last.map(String.init) ?? "" }
        /// Milliseconds, with one-shot and stopped timers last.
        var periodValue: Int64 { first.period.map(Int64.init) ?? Int64.max }
    }

    var body: some View {
        Computed(snapshot, key: "timers page", label: "Reading the timers", compute: { s in
            let timers = try s.timers()
            var places: [Int: String] = [:]
            for list in Dictionary(grouping: timers.timers, by: { HeapSnapshot.timerKey($0) }).values where list.count > 1 {
                for (t, p) in zip(list, try s.timerPlaces(list)) { places[t.object] = p }
            }
            return Model(timers: timers, places: places)
        }) { model in
            content(model)
        }
    }

    private func content(_ model: Model) -> some View {
        @Bindable var session = session
        let all = model.timers.timers
        let groups = Dictionary(grouping: all, by: { HeapSnapshot.timerKey($0) })
            .map { TimerGroup(id: $0.key, timers: $0.value) }
            .filter { text.isEmpty || $0.id.localizedCaseInsensitiveContains(text) }
            .sorted { ($0.count, $1.id) > ($1.count, $0.id) }
        let group = groups.first { $0.id == session.timerGroup }
        let awaited = all.filter { $0.awaitedBy != nil }.count
        return VStack(alignment: .leading, spacing: 10) {
            Text("Timers").font(.title2.weight(.semibold))
            Text("Timers from Timer, System.Timers.Timer, PeriodicTimer, Task.Delay, Task.WaitAsync and CancelAfter, grouped by callback or awaiting call and owner. Select a group to see timers named by the differing parts of their owner paths; select a timer for its full path from a root, excluding timer queues. A dump has no clock: ages are relative to its latest timer start, not capture time.")
                .foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            MetricRow {
                Metric(title: "Timers", value: all.count.formatted(), detail: "\(all.filter { $0.state == "scheduled" }.count.formatted()) scheduled")
                Metric(title: "Periodic", value: all.filter { $0.period != nil }.count.formatted(), detail: "fire again after each period", hideZero: true)
                Metric(title: "Awaited", value: awaited.formatted(), detail: "an async call waits for them", hideZero: true)
                Metric(title: "Call a method", value: all.filter { $0.awaitedBy == nil && $0.kind != "CancelAfter" }.count.formatted(), detail: "Timer callbacks", hideZero: true)
                Metric(title: "Stopped or disposed", value: all.filter { $0.state != "scheduled" }.count.formatted(), detail: "still referenced", hideZero: true)
            }
            TextField("Filter by method, type or field", text: $text).textFieldStyle(.roundedBorder).frame(maxWidth: 320)
            SplitPanes(axis: .horizontal, firstMin: 420, secondMin: 360, initial: 0.55) {
                TextTable(rows: groups, columns: [
                    .init("Timers", width: 50, minWidth: 40, font: .digits, value: \.count, text: { $0.count.formatted() }),
                    .init("Kind", width: 100, minWidth: 70, value: \.kind, text: \.kind),
                    .init("Calls or awaited by", width: 300, minWidth: 140, flexible: true, truncation: .byTruncatingMiddle,
                          value: \.does, text: \.does, help: { $0.does }),
                    .init("Held by", width: 200, minWidth: 100, truncation: .byTruncatingMiddle, value: \.heldBy, text: \.heldBy,
                          help: { $0.heldBy }, isTypeName: { _ in true }),
                    .init("Period", width: 80, minWidth: 50, value: \.periodValue, text: \.period),
                ], sort: $order, selection: session.timerGroup, select: { session.timerGroup = $0; session.timer = nil })
            } second: {
                if let group {
                    TimerGroupPane(snapshot: snapshot, group: group, places: model.places).id(group.id)
                } else {
                    Placeholder("Select a row to see its timers.")
                }
            }
        }
        .padding(16)
        .onAppear { if session.timerGroup == nil { session.timerGroup = groups.first?.id } }
        .onChange(of: session.timerGroup) { _, id in
            if let id, !groups.contains(where: { $0.id == id }) { text = "" }
        }
    }
}

/// The timers of a group above the selected one when the group has more than one; the timer alone when it has one.
private struct TimerGroupPane: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let group: TimersView.TimerGroup
    let places: [Int: String]

    var body: some View {
        let timers = group.timers.sorted { (places[$0.object] ?? "").localizedStandardCompare(places[$1.object] ?? "") == .orderedAscending }
        let shown = timers.first { $0.object == session.timer } ?? timers[0]
        if timers.count > 1 {
            SplitPanes(axis: .vertical, firstMin: 80, secondMin: 240, firstIdeal: CGFloat(min(timers.count, 8)) * 38 + 12) {
                List(timers, id: \.object, selection: Binding(get: { shown.object }, set: { session.timer = $0 })) { t in
                    VStack(alignment: .leading, spacing: 1) {
                        Text(places[t.object] ?? "").lineLimit(2)
                        Text(TimersView.detail(t)).font(.caption).foregroundStyle(t.state == "scheduled" ? .secondary : Palette.dead).lineLimit(1)
                    }
                    .help(places[t.object] ?? "")
                    .tag(t.object)
                }
            } second: {
                TimerDetail(snapshot: snapshot, timer: shown).id(shown.object)
            }
        } else {
            TimerDetail(snapshot: snapshot, timer: shown).id(shown.object)
        }
    }
}

extension TimersView {
    /// "11.9 min old, fires 60 min after its start", and the state when the timer is not scheduled.
    static func detail(_ t: HeapSnapshot.TimerInfo) -> String {
        [t.state == "scheduled" ? nil : t.state,
         t.age.map { HeapSnapshot.duration($0) + " old" },
         t.dueTime.map { "fires " + HeapSnapshot.duration(Int64($0)) + " after its start" }].compactMap { $0 }.joined(separator: ", ")
    }
}

private struct TimerDetail: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let timer: HeapSnapshot.TimerInfo

    var body: some View {
        let t = timer
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(t.kind).font(.headline)
                    Text(t.state).foregroundStyle(t.state == "scheduled" ? Color.secondary : Palette.dead)
                }
                Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                    link("Timer", t.object)
                    if let s = t.source { link("Made by", s) }
                    if t.awaitedBy == nil { row("Calls", t.callback) } else { row("Does", t.callback) }
                    if let o = t.target { link(t.kind == "CancelAfter" ? "Source" : "Target", o) }
                    if let call = t.awaitedBy {
                        GridRow {
                            Text("Awaited by").foregroundStyle(.secondary)
                            HStack(spacing: 8) {
                                Text(call).textSelection(.enabled)
                                if let box = t.awaitingCall { Button("Open in Tasks") { session.show(taskOf: box, in: snapshot) } }
                            }
                        }
                        if let outer = t.outermost, outer != call { row("Outermost call", outer) }
                    }
                    GridRow {
                        Text("Held by").foregroundStyle(.secondary)
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Text(t.ownerText).textSelection(.enabled)
                            if let o = t.owner { ObjectLink(snapshot: snapshot, object: o, showSize: false) }
                        }
                    }
                    row("Due", t.dueTime.map { "\(HeapSnapshot.duration(Int64($0))) after its start" } ?? "not scheduled")
                    row("Period", t.period.map { HeapSnapshot.duration(Int64($0)) } ?? "none (the timer fires once)")
                    if let age = t.age { row("Started", "\(HeapSnapshot.duration(age)) before the latest timer start") }
                }
                RootPath(snapshot: snapshot, object: t.owner ?? t.source ?? t.object, avoiding: HeapSnapshot.timerQueueTypes, avoidingText: "the timer queues")
            }
            .padding(PageInsets.detail)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func row(_ title: String, _ value: String) -> some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }

    private func link(_ title: String, _ object: Int) -> some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            ObjectLink(snapshot: snapshot, object: object)
        }
    }
}
