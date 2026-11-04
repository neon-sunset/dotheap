import AppKit
import HeapCore
import SwiftUI

struct DominatorTable: NSViewRepresentable {
    let snapshot: HeapSnapshot
    let graph: OwnerTree
    let rows: [DominatorTree.Row]
    let open: Set<String>
    let wide: Bool
    let compact: Bool
    let typeNameStyle: TypeNameStyle
    let inspect: (OwnerEntry) -> Void
    let action: (DominatorTree.Row) -> Void
    let failed: (Error) -> Void

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        context.coordinator.parent = self
        let table = DominatorList()
        table.headerView = nil
        table.style = .plain
        table.selectionHighlightStyle = .none
        table.rowHeight = 24
        table.intercellSpacing = .zero
        table.gridStyleMask = .solidHorizontalGridLineMask
        table.gridColor = .separatorColor
        table.allowsColumnReordering = false
        table.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        table.addTableColumn(NSTableColumn(identifier: .init("owner")))
        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        table.activate = { [weak coordinator = context.coordinator] row in
            guard let parent = coordinator?.parent, parent.rows.indices.contains(row) else { return }
            parent.action(parent.rows[row])
        }
        let scroll = NSScrollView()
        table.autoresizingMask = [.width]
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        let previous = coordinator.parent!
        coordinator.parent = self
        do { coordinator.total = try graph.entry(graph.rootNode) } catch { failed(error) }
        guard let table = scroll.documentView as? NSTableView else { return }
        let origin = scroll.contentView.bounds.origin
        if !coordinator.loaded || previous.snapshot !== snapshot || previous.wide != wide || previous.compact != compact || previous.typeNameStyle != typeNameStyle {
            table.reloadData()
            coordinator.loaded = true
        } else {
            let old = previous.rows
            var first = 0
            while first < min(old.count, rows.count), old[first].id == rows[first].id { first += 1 }
            var oldEnd = old.count, newEnd = rows.count
            while oldEnd > first, newEnd > first, old[oldEnd - 1].id == rows[newEnd - 1].id {
                oldEnd -= 1; newEnd -= 1
            }
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0
                table.beginUpdates()
                if oldEnd > first { table.removeRows(at: IndexSet(first..<oldEnd), withAnimation: []) }
                if newEnd > first { table.insertRows(at: IndexSet(first..<newEnd), withAnimation: []) }
                table.endUpdates()
            }
            let changed = IndexSet(rows.indices.filter { index in
                guard index < first || index >= newEnd else { return false }
                let row = rows[index]
                if case .more = row.item { return true }
                return previous.open.contains(row.id) != open.contains(row.id)
            })
            if !changed.isEmpty { table.reloadData(forRowIndexes: changed, columnIndexes: IndexSet(integer: 0)) }
        }
        scroll.contentView.scroll(to: origin)
        scroll.reflectScrolledClipView(scroll.contentView)
    }

    @MainActor final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
        var parent: DominatorTable!
        var total: OwnerEntry?
        var loaded = false

        func numberOfRows(in tableView: NSTableView) -> Int { parent.rows.count }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            let identifier = NSUserInterfaceItemIdentifier("dominator-row")
            let view = tableView.makeView(withIdentifier: identifier, owner: self) as? DominatorRowView ?? DominatorRowView()
            view.identifier = identifier
            view.rowID = parent.rows[row].id
            view.disclosureRect = parent.rows[row].hasChildren ? NSRect(x: 12 + CGFloat(parent.rows[row].depth) * 16, y: 3, width: 18, height: 18) : .zero
            return view
        }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            let identifier = NSUserInterfaceItemIdentifier("dominator")
            let cell = tableView.makeView(withIdentifier: identifier, owner: self) as? DominatorCell ?? DominatorCell()
            cell.identifier = identifier
            let item = parent.rows[row]
            do {
                var entry: OwnerEntry?
                var address: UInt64?
                let name: String, symbol: String, size: UInt64?, managed: UInt64, native: UInt64, nativeOwner: Bool
                var color = NSColor.labelColor
                var style = parent.typeNameStyle
                switch item.item {
                case let .node(node):
                    let value = try parent.graph.entry(node)
                    entry = value
                    name = value.name
                    symbol = ownerSymbol(value.kind)
                    size = value.object != nil || value.chunk != nil ? value.size : nil
                    managed = value.managed; native = value.native; nativeOwner = value.chunk != nil
                    if let object = value.object {
                        address = parent.snapshot.heap.addresses[object]
                        color = NSColor(Palette.state(parent.snapshot.graph.state(object)))
                    } else if let chunk = value.chunk {
                        address = try parent.snapshot.nativeGraph.chunks[chunk].userStart
                        color = NSColor(Palette.native("malloc heap"))
                    }
                case let .group(group):
                    name = group.nodes.count.formatted() + (group.kind == nil ? " × " : " chunks · ") + (group.kind?.displayName(group.name) ?? group.name)
                    symbol = "square.stack.3d.up"
                    size = group.size; managed = group.managed; native = group.native; nativeOwner = group.kind != nil
                    if nativeOwner { style = .standard }
                case .more, .loading:
                    if case let .more(_, remaining) = item.item { name = "\(remaining.formatted()) more…" }
                    else { name = "Loading children…" }
                    symbol = ""
                    size = nil; managed = 0; native = 0; nativeOwner = false
                    color = .secondaryLabelColor
                }
                cell.configure(name: name, style: style, symbol: symbol, color: color,
                               entry: entry, address: address, depth: item.depth,
                               expandable: item.hasChildren, expanded: parent.open.contains(item.id),
                               size: size, managed: managed, native: native, nativeOwner: nativeOwner,
                               total: (total?.managed ?? 0) + (total?.native ?? 0),
                               wide: parent.wide, compact: parent.compact,
                               more: { if case .more = item.item { return true }; return false }(),
                               loading: { if case .loading = item.item { return true }; return false }(),
                               toggle: { [weak self] in self?.parent.action(item) },
                               inspect: { [weak self] in if let entry { self?.parent.inspect(entry) } })
            } catch { parent.failed(error) }
            return cell
        }
    }
}

@MainActor private final class DominatorList: NSTableView {
    var activate: ((Int) -> Void)?
    private weak var pressedRow: DominatorRowView?
    private weak var pressedButton: NSButton?
    private var pressedID: String?

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        let row = row(at: convert(event.locationInWindow, from: nil))
        guard row >= 0, let view = rowView(atRow: row, makeIfNecessary: false) as? DominatorRowView else { return }
        pressedRow = view
        pressedID = view.rowID
        if let cell = self.view(atColumn: 0, row: row, makeIfNecessary: false) {
            for case let button as NSButton in cell.subviews where !button.isHidden {
                if button.bounds.contains(button.convert(event.locationInWindow, from: nil)) {
                    pressedButton = button
                    button.highlight(true)
                    return
                }
            }
        }
        window?.makeFirstResponder(self)
        view.isPressed = true
    }

    override func mouseDragged(with event: NSEvent) {
        if let button = pressedButton {
            button.highlight(button.bounds.contains(button.convert(event.locationInWindow, from: nil)))
        } else if let row = pressedRow {
            row.isPressed = row.rowID == pressedID && row.bounds.contains(row.convert(event.locationInWindow, from: nil))
        }
    }

    override func mouseUp(with event: NSEvent) {
        let view = pressedRow, button = pressedButton, id = pressedID
        pressedRow = nil; pressedButton = nil; pressedID = nil
        view?.isPressed = false
        button?.highlight(false)
        guard let view, view.rowID == id else { return }
        if let button {
            if button.bounds.contains(button.convert(event.locationInWindow, from: nil)), let action = button.action {
                NSApp.sendAction(action, to: button.target, from: button)
            }
        } else if view.bounds.contains(view.convert(event.locationInWindow, from: nil)) {
            let row = row(for: view)
            if row >= 0 { activate?(row) }
        }
    }
}

@MainActor final class DominatorRowView: NSTableRowView {
    var rowID = "" { didSet { if rowID != oldValue { isPressed = false } } }
    private(set) var isHovered = false { didSet { if isHovered != oldValue { needsDisplay = true } } }
    var isPressed = false { didSet { if isPressed != oldValue { needsDisplay = true } } }
    var disclosureRect = NSRect.zero
    private var hoverArea: NSTrackingArea?

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let hoverArea { removeTrackingArea(hoverArea) }
        let area = NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeInActiveApp, .inVisibleRect, .enabledDuringMouseDrag], owner: self, userInfo: nil)
        addTrackingArea(area)
        hoverArea = area
        isHovered = window.map { visibleRect.contains(convert($0.mouseLocationOutsideOfEventStream, from: nil)) } ?? false
    }

    override func mouseEntered(with event: NSEvent) { isHovered = true }
    override func mouseExited(with event: NSEvent) { isHovered = false }

    override func drawBackground(in dirtyRect: NSRect) {
        super.drawBackground(in: dirtyRect)
        if (isHovered || isPressed) && !disclosureRect.isEmpty {
            NSColor.labelColor.withAlphaComponent(0.06).setFill()
            NSBezierPath(roundedRect: disclosureRect, xRadius: 4, yRadius: 4).fill()
        }
    }
}

@MainActor final class DominatorCell: NSTableCellView {
    private let disclosure = NSButton()
    private let symbol = NSImageView()
    private let name = TypeNameField(labelWithString: "")
    private let address = NSTextField(labelWithString: "")
    private let inspectButton = NSButton()
    private let size = NSTextField(labelWithString: "")
    private let retained = NSTextField(labelWithString: "")
    private let percent = NSTextField(labelWithString: "")
    private var depth = 0
    private var wide = true
    private var compact = false
    private var addressSize: CGSize = .zero
    private var toggle: (() -> Void)?
    private var inspect: (() -> Void)?

    init() {
        super.init(frame: .zero)
        for view in [disclosure, symbol, name, address, inspectButton, size, retained, percent] { addSubview(view) }
        for button in [disclosure, inspectButton] {
            button.isBordered = false
            button.bezelStyle = .smallSquare
            button.target = self
        }
        disclosure.identifier = .init("disclosure")
        disclosure.action = #selector(toggleRow)
        (disclosure.cell as? NSButtonCell)?.highlightsBy = []
        inspectButton.identifier = .init("inspect")
        inspectButton.action = #selector(inspectRow)
        inspectButton.image = NSImage(systemSymbolName: "arrow.up.right.square", accessibilityDescription: "Inspect allocation")
        inspectButton.contentTintColor = .linkColor
        inspectButton.toolTip = "Inspect allocation"
        name.font = .systemFont(ofSize: NSFont.systemFontSize)
        name.singleLine = true
        name.lineBreakMode = .byTruncatingMiddle
        address.font = .monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        address.textColor = .secondaryLabelColor
        for field in [size, retained, percent] {
            field.font = .monospacedDigitSystemFont(ofSize: NSFont.systemFontSize, weight: .regular)
            field.alignment = .right
        }
        percent.textColor = .secondaryLabelColor
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    override func mouseDown(with event: NSEvent) { enclosingScrollView?.documentView?.mouseDown(with: event) }
    override func mouseDragged(with event: NSEvent) { enclosingScrollView?.documentView?.mouseDragged(with: event) }
    override func mouseUp(with event: NSEvent) { enclosingScrollView?.documentView?.mouseUp(with: event) }

    func configure(name: String, style: TypeNameStyle, symbol: String, color: NSColor,
                   entry: OwnerEntry?, address: UInt64?, depth: Int, expandable: Bool, expanded: Bool,
                   size: UInt64?, managed: UInt64, native: UInt64, nativeOwner: Bool, total: UInt64,
                   wide: Bool, compact: Bool, more: Bool, loading: Bool = false, toggle: @escaping () -> Void, inspect: @escaping () -> Void) {
        self.depth = depth; self.wide = wide; self.compact = compact
        self.toggle = toggle; self.inspect = inspect
        self.name.name = name; self.name.style = style
        self.name.textColor = more ? .linkColor : loading ? .secondaryLabelColor : .labelColor
        self.name.toolTip = name
        self.name.setAccessibilityValue(name)
        disclosure.isHidden = !expandable
        disclosure.image = NSImage(systemSymbolName: expanded ? "chevron.down" : "chevron.right", accessibilityDescription: expanded ? "Collapse" : "Expand")
        disclosure.contentTintColor = .secondaryLabelColor
        let allocation = entry?.object != nil || entry?.chunk != nil
        self.symbol.image = NSImage(systemSymbolName: allocation ? (nativeOwner ? "memorychip" : "circle.fill") : symbol, accessibilityDescription: nil)
        self.symbol.contentTintColor = color
        inspectButton.isHidden = !allocation && entry?.targets.count != 1
        inspectButton.toolTip = allocation ? "Inspect allocation" : "Inspect field value"
        inspectButton.setAccessibilityLabel(inspectButton.toolTip)
        self.address.stringValue = address.map(Format.hex) ?? ""
        addressSize = address == nil ? .zero : self.address.frame(forAlignmentRect: NSRect(origin: .zero, size: self.address.intrinsicContentSize)).size
        self.size.stringValue = size.map(Format.bytes) ?? ""
        self.retained.stringValue = more || loading ? "" : Format.retained(managed: managed, native: native, nativeOwner: nativeOwner)
        self.retained.toolTip = "Managed: " + Format.bytes(managed) + "\nNative: " + Format.bytes(native)
        self.percent.stringValue = more || loading ? "" : Format.percent(Double(managed + native), Double(total))
        self.size.isHidden = !wide; self.percent.isHidden = !wide
        needsLayout = true
    }

    override func layout() {
        super.layout()
        var right = bounds.width - 16
        func place(_ field: NSView, width: CGFloat) {
            field.frame = NSRect(x: right - width, y: 3, width: width, height: 18)
            right -= width + 4
        }
        if wide { place(percent, width: 70) }
        place(retained, width: compact ? 130 : 230)
        if wide { place(size, width: 90) }
        let left = 12 + CGFloat(depth) * 16
        disclosure.frame = NSRect(x: left, y: 3, width: 18, height: 18)
        symbol.frame = NSRect(x: left + 22, y: 7, width: 12, height: 12)
        let inspectWidth: CGFloat = inspectButton.isHidden ? 0 : 22
        let available = max(0, right - left - 42 - inspectWidth)
        let addressWidth = min(addressSize.width, available)
        let nameSpace = max(0, available - addressWidth - 6)
        let nameSize = name.size(for: nameSpace)
        let nameWidth = min(nameSpace, name.frame(forAlignmentRect: NSRect(origin: .zero, size: nameSize)).width)
        name.frame = NSRect(x: left + 38, y: 3, width: nameWidth, height: 18)
        let baseline = name.alignmentRect(forFrame: name.frame).maxY - name.firstBaselineOffsetFromTop
        address.frame = NSRect(x: name.frame.maxX + 4, y: baseline + address.firstBaselineOffsetFromTop - addressSize.height,
                               width: addressWidth, height: addressSize.height)
        inspectButton.frame = NSRect(x: address.frame.maxX + 2, y: 3, width: inspectWidth, height: 18)
        name.needsLayout = true
    }

    @objc private func toggleRow() { toggle?() }
    @objc private func inspectRow() { inspect?() }
}
