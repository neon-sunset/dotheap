import AppKit
import SwiftUI

enum TypeNameStyle: String, CaseIterable, Identifiable {
    case standard, initials, expanded
    var id: Self { self }

    init?(rawValue: String) {
        switch rawValue {
        case "standard": self = .standard
        case "initials", "typeOnly", "namespaceAndType": self = .initials
        case "expanded": self = .expanded
        default: return nil
        }
    }

    var title: String {
        switch self {
        case .standard: "Truncate"
        case .initials: "Abbreviate namespaces"
        case .expanded: "Wrap to two lines"
        }
    }

    func display(_ name: String, width: CGFloat, font: NSFont, singleLine: Bool = false) -> String {
        func fits(_ value: Substring) -> Bool {
            (String(value) as NSString).size(withAttributes: [.font: font]).width <= width
        }
        guard !fits(name[...]) else { return name }
        switch self {
        case .standard: return name
        case .initials: return Self.abbreviate(name)
        case .expanded:
            guard !singleLine else { return name }
            let boundaries = name.indices.filter { index in
                let c = name[index]
                return c == "." || c == "+" || c == "<" || c == "," || c == " "
                    || (c == ":" && index > name.startIndex && name[name.index(before: index)] == ":")
            }.map { name.index(after: $0) }
            guard let split = boundaries.last(where: { $0 < name.endIndex && fits(name[..<$0]) }) else { return name }
            return String(name[..<split]).trimmingCharacters(in: .whitespaces)
                + "\n" + name[split...].trimmingCharacters(in: .whitespaces)
        }
    }

    static func abbreviate(_ name: String) -> String {
        var result = "", token = ""
        func appendToken() {
            let parts = token.split(separator: ".", omittingEmptySubsequences: false)
            if parts.count < 2 || parts.contains(where: \.isEmpty) { result += token }
            else { result += parts.dropLast().map { String($0.prefix(1)) }.joined(separator: ".") + "." + parts.last! }
            token = ""
        }
        for c in name {
            if c.isLetter || c.isNumber || "._+`$".contains(c) { token.append(c) }
            else { appendToken(); result.append(c) }
        }
        appendToken()
        return result
    }
}

private struct TypeNameStyleKey: EnvironmentKey {
    static let defaultValue = TypeNameStyle.standard
}

extension EnvironmentValues {
    var typeNameStyle: TypeNameStyle {
        get { self[TypeNameStyleKey.self] }
        set { self[TypeNameStyleKey.self] = newValue }
    }
}

struct TypeNameText: NSViewRepresentable {
    let name: String
    let style: TypeNameStyle
    var color: NSColor = .labelColor
    var singleLine = false

    func makeNSView(context: Context) -> TypeNameField {
        let field = TypeNameField(labelWithString: "")
        field.font = .systemFont(ofSize: NSFont.systemFontSize)
        field.lineBreakMode = .byTruncatingMiddle
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        return field
    }

    func updateNSView(_ field: TypeNameField, context: Context) {
        field.name = name
        field.style = style
        field.singleLine = singleLine
        field.textColor = color
        field.toolTip = name
        field.setAccessibilityValue(name)
        field.needsLayout = true
        field.invalidateIntrinsicContentSize()
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView: TypeNameField, context: Context) -> CGSize? {
        nsView.size(for: proposal.width)
    }
}

final class TypeNameField: NSTextField {
    var name = ""
    var style = TypeNameStyle.standard
    var singleLine = false

    private var textFont: NSFont { font ?? .systemFont(ofSize: NSFont.systemFontSize) }

    func size(for width: CGFloat?) -> CGSize {
        let natural = (name as NSString).size(withAttributes: [.font: textFont])
        let width = max(0, min(floor(width ?? .infinity), ceil(natural.width)))
        let text = style.display(name, width: width, font: textFont, singleLine: singleLine)
        let lines: CGFloat = text.contains("\n") ? 2 : 1
        return CGSize(width: width, height: ceil(natural.height) * lines)
    }

    override func layout() {
        maximumNumberOfLines = singleLine || style != .expanded ? 1 : 2
        let text = style.display(name, width: max(0, alignmentRect(forFrame: bounds).width), font: textFont, singleLine: singleLine)
        if stringValue != text { stringValue = text }
        super.layout()
    }
}
