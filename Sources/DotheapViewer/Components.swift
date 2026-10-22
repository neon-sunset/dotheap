import HeapCore
import SwiftUI

struct ReadView<Content: View>: View {
    private let result: Result<Content, Error>

    init(@ViewBuilder content: () throws -> Content) {
        result = Result(catching: content)
    }

    var body: some View {
        switch result {
        case .success(let content): content
        case .failure(let error):
            Text(analysisErrorMessage(error)).foregroundStyle(.red).textSelection(.enabled).padding()
        }
    }
}

/// Shows a snapshot query and reuses its cached result.
struct Computed<T, Content: View>: View {
    let snapshot: HeapSnapshot
    let key: String
    let label: String
    let compute: (HeapSnapshot) throws -> T
    let content: (T) -> Content

    init(_ snapshot: HeapSnapshot, key: String, label: String, compute: @escaping (HeapSnapshot) throws -> T, @ViewBuilder content: @escaping (T) -> Content) {
        self.snapshot = snapshot
        self.key = key
        self.label = label
        self.compute = compute
        self.content = content
    }

    var body: some View {
        ComputedResult(input: self)
            .id(key)
            .id(ObjectIdentifier(snapshot))
    }
}

private struct ComputedResult<T, Content: View>: View {
    let input: Computed<T, Content>
    @State private var value: T?
    @State private var error: String?

    init(input: Computed<T, Content>) {
        self.input = input
        _value = State(initialValue: input.snapshot.memoized(input.key))
    }

    var body: some View {
        if let value {
            input.content(value)
        } else if let error {
            Text(error).foregroundStyle(.red).textSelection(.enabled).padding()
        } else {
            ProgressView(input.label).frame(maxWidth: .infinity, maxHeight: .infinity).task {
                let s = input.snapshot, k = input.key, c = input.compute
                do {
                    let result = try await Task.detached { try s.memo(k) { try c(s) } }.value
                    try Task.checkCancellation()
                    value = result
                } catch {
                    guard !Task.isCancelled else { return }
                    self.error = analysisErrorMessage(error)
                }
            }
        }
    }
}

/// Two panes with a divider that the pointer drags to resize them; the first pane keeps its size when the contents
/// change. HSplitView and VSplitView host each pane in an AppKit view of its own, and since macOS 26 each of those
/// adds the widths of the sidebar and the inspector to its minimum width again.
struct SplitPanes<First: View, Second: View>: View {
    let axis: Axis
    var firstMin: CGFloat = 200
    var secondMin: CGFloat = 200
    /// The first pane's share of the space until the divider is moved, or its size with `firstIdeal`.
    var initial: CGFloat = 0.5
    var firstIdeal: CGFloat?
    /// Without the second pane, the first one takes the whole space.
    var showSecond = true
    @ViewBuilder let first: () -> First
    @ViewBuilder let second: () -> Second

    // The panes are built here, outside the geometry reader and the divider's state: a resize of the window or a drag
    // of the divider places the panes again and does not build them again.
    var body: some View {
        SplitPanesLayout(axis: axis, firstMin: firstMin, secondMin: secondMin, initial: initial, firstIdeal: firstIdeal,
                         first: first(), second: showSecond ? second() : nil)
    }
}

private struct SplitPanesLayout<First: View, Second: View>: View {
    let axis: Axis
    let firstMin: CGFloat
    let secondMin: CGFloat
    let initial: CGFloat
    let firstIdeal: CGFloat?
    let first: First
    let second: Second?
    @State private var size: CGFloat?
    @State private var dragStart: CGFloat?

    var body: some View {
        GeometryReader { geo in
            let total = axis == .horizontal ? geo.size.width : geo.size.height
            let current = clamp(size ?? firstIdeal ?? total * initial, total: total)
            let layout = axis == .horizontal ? AnyLayout(HStackLayout(spacing: 0)) : AnyLayout(VStackLayout(spacing: 0))
            // A list or scroll view draws its background into the safe area its edge touches, such as the area under
            // the toolbar. Each pane draws only inside its own frame, as in an AppKit split view.
            layout {
                first
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                    .frame(width: axis == .horizontal && second != nil ? current : nil, height: axis == .vertical && second != nil ? current : nil)
                    .clipped()
                if let second {
                    divider(current: current, total: total)
                    second.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading).clipped()
                }
            }
        }
    }

    private func clamp(_ value: CGFloat, total: CGFloat) -> CGFloat {
        max(min(firstMin, total), min(value, total - secondMin - 1))
    }

    private func divider(current: CGFloat, total: CGFloat) -> some View {
        Rectangle().fill(.separator)
            .frame(width: axis == .horizontal ? 1 : nil, height: axis == .vertical ? 1 : nil)
            .padding(axis == .horizontal ? .horizontal : .vertical, 3)
            .contentShape(Rectangle())
            .padding(axis == .horizontal ? .horizontal : .vertical, -3)
            .pointerStyle(axis == .horizontal ? .columnResize : .rowResize)
            .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .global)
                .onChanged { value in
                    let start = dragStart ?? current
                    dragStart = start
                    size = clamp(start + (axis == .horizontal ? value.translation.width : value.translation.height), total: total)
                }
                .onEnded { _ in dragStart = nil })
            .zIndex(1)
    }
}

/// The text of an empty pane, in the middle of the pane.
struct Placeholder: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text).foregroundStyle(.secondary).multilineTextAlignment(.center).padding(16)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// A list of objects that open in the inspector, with an optional value per object.
struct SampleList<Samples: RandomAccessCollection>: View where Samples.Element == Int {
    let snapshot: HeapSnapshot
    let title: String
    let samples: Samples
    var value: ((Int) -> String)?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.subheadline.weight(.semibold)).lineLimit(2)
            if samples.isEmpty {
                Text("No objects").foregroundStyle(.secondary)
            }
            List(samples, id: \.self) { i in
                HStack {
                    ObjectLink(snapshot: snapshot, object: i)
                    Spacer()
                    if let value { Text(value(i)).monospacedDigit().foregroundStyle(.secondary) }
                }
            }
        }
        .padding(8)
    }
}

/// A header that opens and closes its content on a click anywhere on the header (not only the chevron).
struct Expandable<Label: View, Content: View>: View {
    @State private var expanded: Bool
    let label: () -> Label
    let content: () -> Content

    init(expanded: Bool = false, @ViewBuilder label: @escaping () -> Label, @ViewBuilder content: @escaping () -> Content) {
        _expanded = State(initialValue: expanded)
        self.label = label
        self.content = content
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Button {
                expanded.toggle()
            } label: {
                HStack(spacing: 4) {
                    Image(systemName: "chevron.right").font(.caption2.weight(.semibold)).rotationEffect(.degrees(expanded ? 90 : 0))
                        .foregroundStyle(.secondary).frame(width: 12)
                    label()
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if expanded { content().padding(.leading, 16) }
        }
    }
}

/// A titled section with an explanation.
struct Titled<Content: View>: View {
    let title: String
    let detail: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.title3.weight(.semibold))
            Text(detail).font(.callout).foregroundStyle(.secondary)
            content()
        }
    }
}
