import HeapCore
import SwiftUI

enum Format {
    static func mb(_ bytes: UInt64) -> String { mb(Double(bytes)) }
    static func mb(_ bytes: Double) -> String { (bytes / 1_048_576).formatted(.number.precision(.fractionLength(1))) + " MB" }

    static func bytes(_ b: UInt64) -> String {
        b >= 1 << 20 ? mb(b) : b >= 1 << 10 ? (Double(b) / 1024).formatted(.number.precision(.fractionLength(1))) + " KB" : "\(b) B"
    }

    static func percent(_ part: Double, _ whole: Double) -> String {
        whole > 0 ? (part / whole).formatted(.percent.precision(.fractionLength(0))) : "–"
    }

    static func hex(_ v: UInt64) -> String { "0x" + String(v, radix: 16) }
}

enum Palette {
    static let live = Color(red: 0.20, green: 0.66, blue: 0.40)
    static let dead = Color(red: 0.93, green: 0.49, blue: 0.20)
    static let free = Color(red: 0.62, green: 0.64, blue: 0.68)
    static let queued = Color(red: 0.95, green: 0.75, blue: 0.20)
    static let unparsed = Color(red: 0.58, green: 0.40, blue: 0.80)
    static let gcStorage = Color(red: 0.25, green: 0.65, blue: 0.65)
    static let headers = Color(red: 0.35, green: 0.35, blue: 0.40)
    static let committedUnused = Color(red: 0.55, green: 0.75, blue: 0.95)
    static let uncommitted = Color.secondary.opacity(0.12)
    static let pending = Color(red: 0.85, green: 0.72, blue: 0.22)

    static func state(_ s: HeapGraph.ObjectState) -> Color {
        switch s {
        case .live: live
        case .pendingFinalization: pending
        case .dead: dead
        }
    }

    static func generation(_ name: String) -> Color {
        switch name {
        case "gen0": Color(red: 1.00, green: 0.76, blue: 0.10)
        case "gen1": Color(red: 0.10, green: 0.78, blue: 0.95)
        case "gen2": Color(red: 0.78, green: 0.35, blue: 0.95)
        case "immortal", "NonGC heap": Color(red: 0.95, green: 0.40, blue: 0.40)
        case "LOH": Color(red: 0.40, green: 0.50, blue: 0.95)
        case "POH": Color(red: 0.35, green: 0.80, blue: 0.60)
        case "SOH": Color(red: 0.78, green: 0.35, blue: 0.95)
        default: Color(white: 0.55)
        }
    }

    /// Colors of native memory kinds, kept apart from the heap's green.
    static func native(_ kind: String) -> Color {
        switch kind {
        case "malloc heap": Color(red: 0.80, green: 0.42, blue: 0.62)
        case "assembly images": Color(red: 0.45, green: 0.58, blue: 0.85)
        case "native libraries": Color(red: 0.78, green: 0.55, blue: 0.35)
        case "thread stack": Color(red: 0.85, green: 0.75, blue: 0.35)
        case "JIT and stub code": Color(red: 0.40, green: 0.75, blue: 0.80)
        case "GC handle table": Color(red: 0.95, green: 0.55, blue: 0.45)
        default: Color(red: 0.62, green: 0.55, blue: 0.75)
        }
    }

    /// A stable color for a name, for queues and types.
    static func hashed(_ name: String) -> Color {
        var h: UInt64 = 1_469_598_103_934_665_603
        for b in name.utf8 { h = (h ^ UInt64(b)) &* 1_099_511_628_211 }
        return Color(hue: Double(h % 360) / 360, saturation: 0.55, brightness: 0.85)
    }
}

/// A labeled number for summary rows.
struct Metric: View {
    let title: String
    let value: String
    var detail: String?
    var color: Color?
    /// Leave the card out when its value is zero.
    var hideZero = false

    var body: some View {
        if !(hideZero && (value == "0" || value == "0 B")) { card }
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                if let color { Circle().fill(color).frame(width: 8, height: 8) }
                Text(title).font(.caption).foregroundStyle(.secondary)
            }
            Text(value).font(.title3.monospacedDigit().weight(.semibold))
            if let detail { Text(detail).font(.caption2).foregroundStyle(.secondary) }
        }
        .frame(width: 170, height: 64, alignment: .topLeading)
        .padding(10)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
    }
}

/// Metric cards in rows: a card that does not fit in the width starts a new row, so the cards never make the page
/// wider than the window.
struct MetricRow: Layout {
    var spacing: CGFloat = 10

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = arrange(subviews, width: proposal.width ?? .infinity)
        return CGSize(width: rows.map(\.width).max() ?? 0, height: rows.last.map { $0.y + $0.height } ?? 0)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for row in arrange(subviews, width: bounds.width) {
            for (k, x) in row.items { subviews[k].place(at: CGPoint(x: bounds.minX + x, y: bounds.minY + row.y), proposal: .unspecified) }
        }
    }

    private func arrange(_ subviews: Subviews, width: CGFloat) -> [(items: [(Int, CGFloat)], y: CGFloat, width: CGFloat, height: CGFloat)] {
        var rows: [(items: [(Int, CGFloat)], y: CGFloat, width: CGFloat, height: CGFloat)] = []
        var items: [(Int, CGFloat)] = [], x: CGFloat = 0, y: CGFloat = 0, height: CGFloat = 0
        for k in subviews.indices {
            let size = subviews[k].sizeThatFits(.unspecified)
            if size.width == 0 { continue }
            if !items.isEmpty, x + size.width > width {
                rows.append((items, y, x - spacing, height))
                y += height + spacing; items = []; x = 0; height = 0
            }
            items.append((k, x)); x += size.width + spacing; height = max(height, size.height)
        }
        if !items.isEmpty { rows.append((items, y, x - spacing, height)) }
        return rows
    }
}

/// A clickable object reference.
struct ObjectLink: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let object: Int
    var showSize = true
    var showType = true

    var body: some View {
        Button {
            session.select(object: object)
        } label: {
            HStack(spacing: 6) {
                Circle().fill(Palette.state(snapshot.graph.state(object))).frame(width: 7, height: 7)
                if showType { Text(TypeNames.short(snapshot.typeName(ofObject: object))).lineLimit(1).truncationMode(.middle) }
                Text(Format.hex(snapshot.heap.addresses[object])).font(.caption.monospaced()).foregroundStyle(.secondary)
                if showSize { Text(Format.bytes(snapshot.heap.size(object))).font(.caption.monospacedDigit()).foregroundStyle(.secondary) }
            }
        }
        .buttonStyle(.link)
    }
}
