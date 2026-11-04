import AppKit
import SwiftUI

/// The column and direction a TextTable is sorted by.
struct TableSort: Equatable {
    var column: String
    var ascending: Bool
}

/// A table of text cells in an NSTableView with plain text fields as cells. A SwiftUI Table hosts each cell in an
/// NSHostingView of its own and keeps the cells of the rows it has shown; taking thousands of those out of the window
/// takes seconds. The table sorts its rows when they or the sort change, not on each update. Without a sort, the rows
/// keep their order.
struct TextTable<Row, ID: Hashable>: NSViewRepresentable {
    @Environment(\.typeNameStyle) private var typeNameStyle
    enum CellFont { case regular, monospaced, digits }

    struct Column {
        let title: String
        let width: CGFloat
        let minWidth: CGFloat
        /// The column takes the width that the other columns leave.
        let flexible: Bool
        let font: CellFont
        let truncation: NSLineBreakMode
        let isTypeName: ((Row) -> Bool)?
        let text: (Row) -> String
        let color: ((Row) -> NSColor?)?
        let help: ((Row) -> String)?
        let ascending: ((Row, Row) -> Bool)?
        /// A bar instead of text: shares of the cell's width with their colors, from the left.
        var bar: ((Row) -> [(share: Double, color: NSColor)])?

        /// A column that shows a bar.
        init<V: Comparable>(_ title: String, width: CGFloat, minWidth: CGFloat = 40, value: @escaping (Row) -> V,
                            bar: @escaping (Row) -> [(share: Double, color: NSColor)]) {
            self.init(title, width: width, minWidth: minWidth, value: value, text: { _ in "" })
            self.bar = bar
        }

        init<V: Comparable>(_ title: String, width: CGFloat, minWidth: CGFloat = 40, flexible: Bool = false, font: CellFont = .regular,
                            truncation: NSLineBreakMode = .byTruncatingTail, value: @escaping (Row) -> V, text: @escaping (Row) -> String,
                            color: ((Row) -> NSColor?)? = nil, help: ((Row) -> String)? = nil, isTypeName: ((Row) -> Bool)? = nil) {
            self.init(title, width: width, minWidth: minWidth, flexible: flexible, font: font, truncation: truncation, ascending: { value($0) < value($1) },
                      text: text, color: color, help: help, isTypeName: isTypeName)
        }

        /// A column that does not sort.
        init(_ title: String, width: CGFloat, minWidth: CGFloat = 40, flexible: Bool = false, font: CellFont = .regular,
             truncation: NSLineBreakMode = .byTruncatingTail, ascending: ((Row, Row) -> Bool)? = nil, text: @escaping (Row) -> String,
             color: ((Row) -> NSColor?)? = nil, help: ((Row) -> String)? = nil, isTypeName: ((Row) -> Bool)? = nil) {
            self.title = title
            self.width = width
            self.minWidth = minWidth
            self.flexible = flexible
            self.font = font
            self.truncation = truncation
            self.isTypeName = isTypeName
            self.text = text
            self.color = color
            self.help = help
            self.ascending = ascending
        }
    }

    let rows: AnyRandomAccessCollection<Row>
    let id: (Row) -> ID
    let columns: [Column]
    @Binding var sort: TableSort?
    var group: ((Row) -> String)?
    var selection: ID?
    /// Called when the user selects a row, or with nil when the user clears the selection. Without it, rows cannot be
    /// selected.
    var select: ((ID?) -> Void)?

    init<Rows: RandomAccessCollection>(rows: Rows, id: @escaping (Row) -> ID, columns: [Column],
                                      sort: Binding<TableSort?>, group: ((Row) -> String)? = nil, selection: ID? = nil,
                                      select: ((ID?) -> Void)? = nil) where Rows.Element == Row {
        self.rows = AnyRandomAccessCollection(rows)
        self.id = id
        self.columns = columns
        _sort = sort
        self.group = group
        self.selection = selection
        self.select = select
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let table = NSTableView()
        table.style = .inset
        table.usesAlternatingRowBackgroundColors = true
        table.allowsMultipleSelection = false
        table.allowsColumnReordering = false
        table.columnAutoresizingStyle = .noColumnAutoresizing
        table.rowHeight = 22
        context.coordinator.setColumns(columns, of: table)
        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(context.coordinator, selector: #selector(Coordinator.fit), name: NSView.frameDidChangeNotification, object: scroll)
        NotificationCenter.default.addObserver(context.coordinator, selector: #selector(Coordinator.fit), name: NSScroller.preferredScrollerStyleDidChangeNotification,
                                               object: nil)
        context.coordinator.parent = self
        context.coordinator.table = table
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let c = context.coordinator
        let styleChanged = c.style != typeNameStyle
        c.style = typeNameStyle
        c.parent = self
        if let table = c.table {
            if table.numberOfColumns != columns.count {
                c.setColumns(columns, of: table)
                c.applied = false
                c.fit()
            }
            for (k, column) in columns.enumerated() where table.tableColumns[k].title != column.title {
                table.tableColumns[k].title = column.title
            }
        }
        if !c.applied || c.isGrouped != (group != nil) || c.rows.count != rows.count || !c.rows.elementsEqual(rows, by: { id($0) == id($1) }) || c.sort != sort {
            c.rows = rows
            c.apply(sort)
        }
        if styleChanged { c.table?.reloadData() }
        c.showSelection()
    }

    @MainActor final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
        var parent: TextTable!
        weak var table: NSTableView?
        var rows = AnyRandomAccessCollection<Row>([])
        var sorted = AnyRandomAccessCollection<Row>([])
        var sort: TableSort?
        var applied = false
        var style = TypeNameStyle.standard
        var isGrouped = false
        private enum DisplayRow { case header(String), value(Int) }
        private var grouped: [DisplayRow]?
        private var rowOfId: [ID: Int] = [:]
        private var updating = false
        private var fitting = false

        /// Gives the table the columns, in place of the ones it has.
        func setColumns(_ columns: [Column], of table: NSTableView) {
            updating = true
            defer { updating = false }
            for column in table.tableColumns { table.removeTableColumn(column) }
            chosen = [:]
            for (k, c) in columns.enumerated() {
                let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(String(k)))
                column.title = c.title
                column.width = c.width
                column.minWidth = c.minWidth
                column.resizingMask = .userResizingMask
                if c.ascending != nil { column.sortDescriptorPrototype = NSSortDescriptor(key: String(k), ascending: true) }
                table.addTableColumn(column)
            }
        }

        func apply(_ s: TableSort?) {
            guard let table else { return }
            sort = s
            applied = true
            if let s, let k = parent.columns.firstIndex(where: { $0.title == s.column }), let less = parent.columns[k].ascending {
                sorted = AnyRandomAccessCollection(s.ascending ? rows.sorted(by: less) : rows.sorted { less($1, $0) })
                updating = true
                table.sortDescriptors = [NSSortDescriptor(key: String(k), ascending: s.ascending)]
            } else {
                sorted = rows
                updating = true
                table.sortDescriptors = []
            }
            isGrouped = parent.group != nil
            grouped = nil
            if let group = parent.group {
                var seen = Set<String>()
                let titles = rows.map(group).filter { seen.insert($0).inserted }
                let partitions = Dictionary(grouping: sorted, by: group)
                var values: [Row] = [], displayed: [DisplayRow] = []
                rowOfId = [:]
                for title in titles {
                    displayed.append(.header(title))
                    for value in partitions[title] ?? [] {
                        rowOfId[parent.id(value)] = displayed.count
                        displayed.append(.value(values.count))
                        values.append(value)
                    }
                }
                sorted = AnyRandomAccessCollection(values)
                grouped = displayed
            } else {
                rowOfId = Dictionary(sorted.enumerated().map { (parent.id($0.element), $0.offset) }, uniquingKeysWith: { a, _ in a })
            }
            table.reloadData()
            updating = false
        }

        func showSelection() {
            guard let table else { return }
            let row = parent.selection.flatMap { rowOfId[$0] }
            guard row != (table.selectedRow >= 0 ? table.selectedRow : nil) else { return }
            updating = true
            if let row {
                table.selectRowIndexes([row], byExtendingSelection: false)
                table.scrollRowToVisible(row)
            } else {
                table.deselectAll(nil)
            }
            updating = false
        }

        /// Column widths the user set by dragging; the others are the widths the columns ask for.
        private var chosen: [Int: CGFloat] = [:]

        /// Fits the columns to the scroll view's width, less the width of a vertical scroller that takes space, shown or
        /// not. The clip view's width changes when such a scroller shows or hides, and fitting to it can show or hide
        /// the scroller again. Columns the user resized keep their widths. With room to spare, the flexible column takes
        /// it; without, the flexible column gives up width first, then the other columns in proportion to what they have
        /// above their minimum widths. What is still too wide scrolls horizontally.
        @objc func fit() {
            guard !fitting, let table, let scroll = table.enclosingScrollView, table.numberOfColumns == parent.columns.count else { return }
            fitting = true
            defer { fitting = false }
            let columns = parent.columns
            let available = NSScrollView.contentSize(forFrameSize: scroll.frame.size, horizontalScrollerClass: nil, verticalScrollerClass: NSScroller.self,
                                                     borderType: scroll.borderType, controlSize: .regular, scrollerStyle: scroll.scrollerStyle).width
            var widths = columns.indices.map { chosen[$0] ?? columns[$0].width }
            let free = columns.indices.filter { chosen[$0] == nil }
            let flexible = free.first { columns[$0].flexible }
            func shrink(by excess: CGFloat) {
                if excess < 0 {
                    if let k = flexible { widths[k] -= excess }
                    return
                }
                var excess = excess
                if let k = flexible {
                    let give = min(excess, max(0, widths[k] - columns[k].minWidth))
                    widths[k] -= give
                    excess -= give
                }
                let slack = free.reduce(CGFloat(0)) { $0 + max(0, widths[$1] - columns[$1].minWidth) }
                guard excess > 0, slack > 0 else { return }
                let share = min(1, excess / slack)
                for k in free { widths[k] -= max(0, widths[k] - columns[k].minWidth) * share }
            }
            func apply() {
                for k in columns.indices where table.tableColumns[k].width != widths[k] { table.tableColumns[k].width = widths[k] }
                table.tile()
            }
            // A column's rectangle is its width and the spacing after it; the table has the same inset on both sides.
            let inset = table.rect(ofColumn: 0).minX
            let spacing = table.rect(ofColumn: 0).width - table.tableColumns[0].width
            shrink(by: widths.reduce(0, +) + spacing * CGFloat(columns.count) + 2 * inset - available)
            apply()
            // The table may need a few more points than the estimate.
            for _ in 0..<3 {
                let over = table.rect(ofColumn: columns.count - 1).maxX + inset - available
                guard over > 0.5 else { break }
                shrink(by: over)
                apply()
            }
        }

        func tableViewColumnDidResize(_ notification: Notification) {
            guard !fitting, !updating, let column = notification.userInfo?["NSTableColumn"] as? NSTableColumn, let k = Int(column.identifier.rawValue),
                  k < parent.columns.count else { return }
            chosen[k] = column.width
            fit()
        }

        func numberOfRows(in tableView: NSTableView) -> Int { grouped?.count ?? sorted.count }

        private func value(at row: Int) -> Row? {
            let index: Int
            if let grouped {
                guard case let .value(value) = grouped[row] else { return nil }
                index = value
            } else { index = row }
            return sorted[sorted.index(sorted.startIndex, offsetBy: index)]
        }

        func tableView(_ tableView: NSTableView, isGroupRow row: Int) -> Bool {
            if let grouped, case .header = grouped[row] { return true }
            return false
        }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            if let grouped, case let .header(title) = grouped[row] {
                let identifier = NSUserInterfaceItemIdentifier("group")
                let cell = tableView.makeView(withIdentifier: identifier, owner: nil) as? NSTableCellView
                    ?? makeCell(identifier, Column("", width: 200, text: { _ in "" }))
                cell.textField?.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .semibold)
                cell.textField?.stringValue = title
                cell.textField?.toolTip = title
                return cell
            }
            guard let tableColumn, let k = Int(tableColumn.identifier.rawValue), k < parent.columns.count else { return nil }
            let column = parent.columns[k]
            guard let r = value(at: row) else { return nil }
            if let bar = column.bar {
                let view = tableView.makeView(withIdentifier: tableColumn.identifier, owner: nil) as? BarView ?? BarView()
                view.identifier = tableColumn.identifier
                view.segments = bar(r)
                return view
            }
            let identifier = NSUserInterfaceItemIdentifier(tableColumn.identifier.rawValue + (column.isTypeName == nil ? "/text" : "/type"))
            let cell = tableView.makeView(withIdentifier: identifier, owner: nil) as? NSTableCellView ?? makeCell(identifier, column)
            let text = column.text(r), isTypeName = column.isTypeName?(r) == true
            if let field = cell.textField as? TypeNameField {
                field.name = text
                field.style = isTypeName ? style : .standard
                field.needsLayout = true
                field.setAccessibilityValue(field.name)
            } else { cell.textField?.stringValue = text }
            cell.textField?.textColor = column.color?(r) ?? .labelColor
            cell.textField?.toolTip = column.help?(r) ?? (isTypeName ? text : nil)
            return cell
        }

        private func makeCell(_ identifier: NSUserInterfaceItemIdentifier, _ column: Column) -> NSTableCellView {
            let cell = NSTableCellView()
            cell.identifier = identifier
            let text: NSTextField
            if column.isTypeName != nil {
                let field = TypeNameField(labelWithString: "")
                field.singleLine = true
                text = field
            } else { text = NSTextField(labelWithString: "") }
            let size = NSFont.systemFontSize
            text.font = switch column.font {
            case .regular: .systemFont(ofSize: size)
            case .monospaced: .monospacedSystemFont(ofSize: size, weight: .regular)
            case .digits: .monospacedDigitSystemFont(ofSize: size, weight: .regular)
            }
            text.lineBreakMode = column.truncation
            text.translatesAutoresizingMaskIntoConstraints = false
            text.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            cell.addSubview(text)
            cell.textField = text
            NSLayoutConstraint.activate([
                text.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 2),
                text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -2),
                text.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
            ])
            return cell
        }

        func tableView(_ tableView: NSTableView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]) {
            guard !updating, let d = tableView.sortDescriptors.first, let k = d.key.flatMap(Int.init), k < parent.columns.count else { return }
            parent.sort = TableSort(column: parent.columns[k].title, ascending: d.ascending)
        }

        func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool { parent.select != nil && value(at: row) != nil }

        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !updating, let table else { return }
            parent.select?(table.selectedRow >= 0 ? value(at: table.selectedRow).map(parent.id) : nil)
        }
    }
}

/// A horizontal bar of colored parts, 10 points high, on a faint background.
final class BarView: NSView {
    var segments: [(share: Double, color: NSColor)] = [] { didSet { needsDisplay = true } }

    override func draw(_ dirtyRect: NSRect) {
        let rect = NSRect(x: bounds.minX + 2, y: bounds.midY - 5, width: max(0, bounds.width - 4), height: 10)
        NSBezierPath(roundedRect: rect, xRadius: 2, yRadius: 2).setClip()
        NSColor.secondaryLabelColor.withAlphaComponent(0.12).setFill()
        rect.fill()
        var x = rect.minX
        for s in segments {
            let w = rect.width * s.share
            s.color.setFill()
            NSRect(x: x, y: rect.minY, width: w, height: rect.height).fill()
            x += w
        }
    }
}

extension TextTable where Row: Identifiable, ID == Row.ID {
    init<Rows: RandomAccessCollection>(rows: Rows, columns: [Column], sort: Binding<TableSort?>, group: ((Row) -> String)? = nil,
                                      selection: ID? = nil, select: ((ID?) -> Void)? = nil) where Rows.Element == Row {
        self.init(rows: rows, id: \.id, columns: columns, sort: sort, group: group, selection: selection, select: select)
    }
}
