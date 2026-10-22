import HeapCore
import SwiftUI

struct FindingsView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot

    static let heapUse: [DiagnosticCheck] = [.collections, .strings, .zeros, .boxed]
    static let native: Set = ["native-held-by-dead", "malloc-arenas"]

    var body: some View {
        @Bindable var session = session
        if let reports = session.reports {
            SplitPanes(axis: .horizontal, firstMin: 200, secondMin: 500, firstIdeal: 230) {
                List(selection: Binding(get: { session.finding }, set: { session.finding = $0 ?? session.finding })) {
                    SwiftUI.Section("Runtime") {
                        ForEach(reports.filter { $0.name == "verify" }, id: \.name) { r in
                            Text(r.title).tag(r.name).badge(r.findings.count)
                        }
                    }
                    SwiftUI.Section("Application") {
                        ForEach(reports.filter { $0.name != "verify" && !Self.native.contains($0.name) }, id: \.name) { r in
                            Text(r.name == "finalizer-backlog" ? "Finalizers" : r.title).tag(r.name).badge(r.findings.count)
                        }
                    }
                    SwiftUI.Section("Native memory") {
                        ForEach(reports.filter { Self.native.contains($0.name) }, id: \.name) { r in
                            Text(r.title).tag(r.name).badge(r.findings.count)
                        }
                    }
                    SwiftUI.Section("Heap use") {
                        ForEach(Self.heapUse) { Text($0.rawValue).tag($0.rawValue) }
                    }
                }
            } second: {
                detail(reports)
            }
        } else if let report = session.integrityReport {
            ReportView(snapshot: snapshot, report: report)
        } else {
            ProgressView("Preparing findings").frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder private func detail(_ reports: RustArray<CheckReport>) -> some View {
        if let check = DiagnosticCheck(rawValue: session.finding) {
            HeapUseView(snapshot: snapshot, check: check).id(check)
        } else if let r = reports.first(where: { $0.name == session.finding }) ?? reports.first {
            if r.name == "finalizer-backlog" {
                VStack(spacing: 0) {
                    ReportView(snapshot: snapshot, report: r).frame(height: r.sections.allSatisfy(\.rows.isEmpty) ? 150 : 300)
                    Divider()
                    HeapUseView(snapshot: snapshot, check: .finalizers)
                }
            } else if r.name == "async" {
                PageFindings(report: r, page: "Tasks") { session.section = .tasks }
            } else if r.name == "blocked" || r.name == "locks" {
                PageFindings(report: r, page: "Threads") {
                    session.threadsMode = r.name == "locks" ? .locks : .pool
                    session.section = .threads
                }
            } else if r.name == "timers" {
                PageFindings(report: r, page: "Timers") { session.section = .timers }
            } else if r.name == "connections" {
                PageFindings(report: r, page: "Network") { session.section = .network }
            } else {
                ReportView(snapshot: snapshot, report: r)
            }
        }
    }
}

private struct PageFindings: View {
    let report: CheckReport
    let page: String
    let open: () -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(report.title).font(.title2.weight(.semibold))
                Text(report.explanation).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                if report.findings.isEmpty {
                    Text("Nothing found.").foregroundStyle(.secondary)
                } else {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(Array(report.findings.enumerated()), id: \.offset) { _, f in
                            Label(f.text, systemImage: "exclamationmark.triangle.fill").foregroundStyle(Palette.dead).textSelection(.enabled)
                        }
                    }
                }
                Button("Open \(page)", action: open)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct ReportView: View {
    let snapshot: HeapSnapshot
    let report: CheckReport

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(report.title).font(.title2.weight(.semibold))
                Text(report.explanation).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
                if !report.findings.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(Array(report.findings.enumerated()), id: \.offset) { _, f in
                            Label(f.text, systemImage: "exclamationmark.triangle.fill").foregroundStyle(Palette.dead).textSelection(.enabled)
                        }
                    }
                }
                ForEach(Array(report.sections.enumerated()), id: \.offset) { _, section in
                    if section.rows.contains(where: { !$0.children.isEmpty }) {
                        SectionTree(snapshot: snapshot, section: section)
                    } else {
                        SectionTable(snapshot: snapshot, section: section)
                    }
                }
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

private struct SectionTable: View {
    let snapshot: HeapSnapshot
    let section: CheckReport.Section
    /// The column the rows are sorted by (nil: the check's order), and the direction.
    @State private var column: Int?
    @State private var ascending = false

    private var rows: AnyRandomAccessCollection<CheckReport.Row> {
        guard let k = column else { return AnyRandomAccessCollection(section.rows) }
        return AnyRandomAccessCollection(section.rows.sorted { a, b in
            guard k < a.cells.count, k < b.cells.count else { return false }
            let before = Self.less(a.cells[k], b.cells[k])
            return ascending ? before : Self.less(b.cells[k], a.cells[k])
        })
    }

    private static func less(_ a: CheckReport.Cell, _ b: CheckReport.Cell) -> Bool {
        switch (a, b) {
        case let (.int(x), .int(y)): x < y
        case let (.bytes(x), .bytes(y)): x < y
        case let (.address(x), .address(y)): x < y
        case let (.text(x), .text(y)): x.localizedStandardCompare(y) == .orderedAscending
        case (.none, _): true
        default: false
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(section.title).font(.headline)
            if !section.explanation.isEmpty { Text(section.explanation).font(.callout).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading) }
            if section.columns.isEmpty {
                EmptyView()
            } else if section.rows.isEmpty {
                Text("Nothing found.").font(.callout).foregroundStyle(.secondary)
            } else {
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 4) {
                    GridRow {
                        ForEach(Array(section.columns.enumerated()), id: \.offset) { k, name in
                            Button {
                                if column == k { ascending.toggle() } else { column = k; ascending = false }
                            } label: {
                                HStack(spacing: 2) {
                                    Text(name)
                                    if column == k { Image(systemName: ascending ? "chevron.up" : "chevron.down") }
                                }
                                .font(.caption.weight(.semibold)).foregroundStyle(column == k ? .primary : .secondary)
                            }
                            .buttonStyle(.plain).help("Sort by \(name)")
                        }
                        Text("example").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    }
                    Divider()
                    ForEach(Array(rows.prefix(200).enumerated()), id: \.offset) { _, row in
                        GridRow {
                            ForEach(Array(row.cells.enumerated()), id: \.offset) { _, cell in CellView(cell: cell) }
                            Examples(snapshot: snapshot, samples: row.samples)
                        }
                    }
                }
                let omitted = section.omitted + max(0, section.rows.count - 200)
                if omitted > 0 { Text("\(omitted.formatted()) additional rows omitted.").font(.caption).foregroundStyle(.secondary) }
            }
        }
    }
}

private struct CellView: View {
    let cell: CheckReport.Cell
    var none = "-"

    var body: some View {
        switch cell {
        case let .text(t): Text(t).lineLimit(2).textSelection(.enabled).help(t)
        case let .int(i): Text(i.formatted()).monospacedDigit().gridColumnAlignment(.trailing)
        case let .bytes(b): Text(Format.bytes(b)).monospacedDigit().gridColumnAlignment(.trailing)
        case let .address(a): Text(Format.hex(a)).font(.callout.monospaced()).textSelection(.enabled)
        case .none: Text(none).foregroundStyle(.secondary)
        }
    }
}

/// The first sample object of a row, and a menu with the others.
private struct Examples: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let samples: RustArray<Int>
    var showType = true
    @State private var showOthers = false

    var body: some View {
        HStack(spacing: 8) {
            if let first = samples.first { ObjectLink(snapshot: snapshot, object: first, showSize: false, showType: showType) }
            if samples.count > 1 {
                Button("\(samples.count - 1) more") { showOthers = true }
                    .buttonStyle(.link).font(.caption)
                    .popover(isPresented: $showOthers) {
                        List(samples.dropFirst(), id: \.self) { i in ObjectLink(snapshot: snapshot, object: i) }
                            .frame(width: 520, height: min(400, CGFloat(samples.count) * 26 + 20))
                    }
            }
        }
    }
}

/// A section whose rows have rows below them: each row indented under its parent, with a button that shows or hides
/// the rows below it.
private struct SectionTree: View {
    let snapshot: HeapSnapshot
    let section: CheckReport.Section
    /// Paths of rows whose rows below are hidden; at first, rows with more than 5 rows below them.
    @State private var collapsed: Set<[Int]>
    static let visibleLimit = 500

    init(snapshot: HeapSnapshot, section: CheckReport.Section) {
        self.snapshot = snapshot
        self.section = section
        var paths = Set<[Int]>()
        func walk(_ rows: RustArray<CheckReport.Row>, _ path: [Int]) {
            for (k, row) in rows.enumerated() {
                if row.children.count > 5 { paths.insert(path + [k]) } else { walk(row.children, path + [k]) }
            }
        }
        walk(section.rows, [])
        _collapsed = State(initialValue: paths)
    }

    struct Line: Identifiable {
        let path: [Int]
        let row: CheckReport.Row
        var id: [Int] { path }
    }

    /// Every row, with whether it shows: a row shows when no row above it is closed.
    private var lines: [(line: Line, shown: Bool)] {
        var out: [(line: Line, shown: Bool)] = []
        func walk(_ rows: RustArray<CheckReport.Row>, _ path: [Int], _ shown: Bool) {
            for (k, row) in rows.enumerated() where out.count < Self.visibleLimit {
                let p = path + [k]
                out.append((Line(path: p, row: row), shown))
                walk(row.children, p, shown && !collapsed.contains(p))
            }
        }
        walk(section.rows, [], true)
        return out
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(section.title).font(.headline)
                Spacer()
                Button("Expand All") { collapsed = [] }.controlSize(.small)
                Button("Collapse All") { collapsed = Set(section.rows.indices.map { [$0] }) }.controlSize(.small)
            }
            if !section.explanation.isEmpty { Text(section.explanation).font(.callout).foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading) }
            let lines = lines
            let treeColumn = section.rows.first?.cells.firstIndex { if case .text = $0 { true } else { false } } ?? section.columns.count - 1
            // Closed rows stay in the grid at zero height, so the columns keep their widths when rows open and close.
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 0) {
                GridRow {
                    ForEach(section.columns, id: \.self) { Text($0).font(.caption.weight(.semibold)).foregroundStyle(.secondary) }
                    Text("example").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                }
                .padding(.bottom, 3)
                Divider()
                ForEach(lines, id: \.line.id) { line, shown in
                    GridRow {
                        ForEach(Array(line.row.cells.enumerated()), id: \.offset) { k, cell in
                            if k == treeColumn {
                                HStack(spacing: 4) {
                                    Color.clear.frame(width: CGFloat(line.path.count - 1) * 16, height: 1)
                                    Image(systemName: "chevron.right").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
                                        .rotationEffect(.degrees(collapsed.contains(line.path) ? 0 : 90)).frame(width: 14)
                                        .opacity(line.row.children.isEmpty ? 0 : 1)
                                    CellView(cell: cell, none: "")
                                }
                                .contentShape(Rectangle())
                                .onTapGesture {
                                    guard !line.row.children.isEmpty else { return }
                                    if collapsed.contains(line.path) { collapsed.remove(line.path) } else { collapsed.insert(line.path) }
                                }
                            } else {
                                CellView(cell: cell, none: "")
                            }
                        }
                        Examples(snapshot: snapshot, samples: line.row.samples, showType: false)
                    }
                    .padding(.vertical, shown ? 2 : 0)
                    .frame(height: shown ? nil : 0)
                    .clipped()
                    .opacity(shown ? 1 : 0)
                    .allowsHitTesting(shown)
                }
            }
            if lines.count >= Self.visibleLimit { Text("Only the first \(Self.visibleLimit) rows are shown.").font(.caption).foregroundStyle(.secondary) }
        }
    }
}
