import HeapCore
import SwiftUI

struct ObjectInspector: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot

    var body: some View {
        if let i = session.selectedObject {
            ObjectDetail(snapshot: snapshot, object: i).id(i)
        } else {
            Placeholder("Select an object.")
        }
    }
}

/// Everything the inspector shows for one object, gathered once in the background.
struct ObjectModel: Sendable {
    let typeName: String
    let methodTable: UInt64
    let address: UInt64
    let size: UInt64
    let length: Int?
    let live: Bool
    let state: HeapGraph.ObjectState
    let retained: UInt64
    let regionIndex: Int
    let dominator: Int?
    let fields: RustArray<HeapSnapshot.FieldValue>
    let preview: HeapSnapshot.TextPreview?
    let structElements: RustArray<StructElement>?
    let referenceElements: RustArray<ReferenceElement>?
    let pathSource: Int?
    let rootSlot: String?
    let path: RustArray<Int>
    /// Field or element of the previous object on the path that holds each object; nil for the first.
    let pathNames: RustArray<String?>
    let referrers: RustArray<Int>
    let referrerNames: RustArray<String?>
    let sources: RustArray<Int>
    let references: RustArray<Int32>
    let words: RustArray<HeapSnapshot.Word>
    let dead: DeadObject?
    /// malloc chunks the object points to, and the native bytes it keeps alone.
    let native: NativeObjectMemory?

    init(_ s: HeapSnapshot, _ i: Int) throws {
        let detail = try s.inspectObject(i)
        let info = detail.info
        typeName = detail.typeName
        methodTable = detail.methodTable
        address = info.address
        size = info.size
        length = detail.length
        live = info.live
        state = !info.live ? .dead : info.pending ? .pendingFinalization : .live
        retained = info.retained
        regionIndex = info.region
        dominator = info.dominator >= 0 ? Int(info.dominator) : nil
        fields = detail.fields
        preview = detail.preview
        structElements = detail.structElements
        referenceElements = detail.referenceElements
        pathSource = detail.path.source
        path = detail.path.objects
        pathNames = detail.pathNames
        rootSlot = detail.rootSlot
        referrers = detail.referrers
        referrerNames = detail.referrerNames
        sources = detail.sources
        references = detail.references
        words = detail.words
        native = detail.native
        dead = detail.dead
    }

}

struct ObjectDetail: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let object: Int
    @State private var model: ObjectModel?

    var body: some View {
        Group {
            if let m = model { content(m) } else { ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity) }
        }
        .task(id: object) {
            let s = snapshot, i = object
            do {
                model = try await Task.detached { try ObjectModel(s, i) }.value
            } catch {
                session.error = analysisErrorMessage(error)
            }
        }
    }

    private func content(_ m: ObjectModel) -> some View {
        let region = snapshot.heap.regions[m.regionIndex]
        return ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(m.typeName).font(.headline).textSelection(.enabled)
                Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 3) {
                    row("Address", Format.hex(m.address))
                    row("Method table", Format.hex(m.methodTable))
                    row("Size", Format.bytes(m.size))
                    if let n = m.length { row("Length", n.formatted()) }
                    switch m.state {
                    case .live: row("State", "live")
                    case .pendingFinalization: row("State", "pending finalization")
                    case .dead: row("State", "dead (unreachable, not swept)")
                    }
                    row("Retained", Format.bytes(m.retained))
                    GridRow {
                        Text("Region").foregroundStyle(.secondary)
                        Button("\(Format.hex(region.start)) \(region.generationName), \(Glossary.queue(region.queue).name)") {
                            session.show(region: m.regionIndex)
                        }.buttonStyle(.link).help(Glossary.queue(region.queue).help)
                    }
                    if let d = m.dominator {
                        GridRow {
                            Text("Dominator").foregroundStyle(.secondary)
                            if d < snapshot.heap.count { ObjectLink(snapshot: snapshot, object: d) } else { Text(snapshot.nodeName(d)).lineLimit(2) }
                        }
                    }
                }
                .font(.callout.monospacedDigit())

                if !m.fields.isEmpty {
                    group("Fields") { FieldTree(snapshot: snapshot, values: m.fields) }
                }

                if let native = m.native {
                    group("Native memory (\(native.chunks.count) malloc chunks, keeps \(Format.bytes(native.kept)) alone)") {
                        ForEach(native.chunks, id: \.id) { c in
                            HStack(spacing: 6) {
                                if let f = c.field { Text("." + PathList.propertyName(f)).font(.caption.monospaced()).foregroundStyle(.secondary) }
                                Text(c.label).font(.caption).lineLimit(1).truncationMode(.middle)
                                Text(Format.bytes(c.size)).font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                            }
                        }
                    }
                }

                if let preview = m.preview {
                    group("Contents: \(preview.kind)\(preview.truncated ? ", first part" : "")") {
                        // Its own scroll area, so long contents scroll under the pointer instead of the whole inspector.
                        ScrollView(.vertical) {
                            if let text = preview.text {
                                Text(text).font(.callout.monospaced()).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                            }
                            if !preview.hex.isEmpty {
                                Text(preview.hex.joined(separator: "\n")).font(.caption.monospaced()).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                            }
                        }
                        .padding(6)
                        .frame(maxHeight: 280)
                        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 4))
                    }
                }

                if let elements = m.structElements {
                    group("Elements (\(m.length ?? 0))") {
                        ForEach(elements, id: \.index) { e in
                            Expandable {
                                Text("[\(e.index)] ").font(.caption.monospaced()).foregroundStyle(.secondary)
                                    + Text(e.fields.prefix(3).map { "\($0.field.name) \($0.display)" }.joined(separator: ", ")).font(.caption.monospaced())
                            } content: {
                                FieldTree(snapshot: snapshot, values: e.fields)
                            }
                        }
                    }
                } else if let elements = m.referenceElements {
                    group("Elements (\(m.length ?? 0))") {
                        ForEach(elements, id: \.index) { e in
                            HStack(spacing: 8) {
                                Text("[\(e.index)]").font(.caption.monospaced()).foregroundStyle(.secondary).frame(width: 50, alignment: .trailing)
                                if let t = e.target { ObjectLink(snapshot: snapshot, object: t) } else { Text(e.value == 0 ? "null" : Format.hex(e.value)).font(.caption.monospaced()).foregroundStyle(.secondary) }
                            }
                        }
                    }
                }

                if let dead = m.dead {
                    group("Dead object") {
                        Text("Holds \(Format.bytes(dead.holds)) of garbage; \(dead.referrers.count) dead objects reference it.").font(.caption).foregroundStyle(.secondary)
                        if dead.path.count > 1 {
                            Text("Path from the unreferenced dead object where its garbage starts").font(.caption.weight(.semibold)).padding(.top, 2)
                            PathList(snapshot: snapshot, path: dead.path, names: dead.pathNames)
                        } else {
                            Text("No dead object references it: its garbage starts here.").font(.caption).foregroundStyle(.secondary)
                        }
                        if !dead.referrers.isEmpty {
                            Text("Dead referrers").font(.caption.weight(.semibold)).padding(.top, 2)
                            ForEach(dead.referrers.prefix(100), id: \.self) { ObjectLink(snapshot: snapshot, object: $0) }
                        }
                    }
                }

                if m.live {
                    group("Path to root (shortest)") {
                        if let source = m.pathSource {
                            Label(snapshot.describe(source: source), systemImage: "arrow.up.to.line").font(.caption).lineLimit(2)
                            if let slot = m.rootSlot {
                                Text(slot).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(3).padding(.leading, 20).textSelection(.enabled)
                            }
                        }
                        PathList(snapshot: snapshot, path: m.path, names: m.pathNames)
                    }
                }

                group("Referenced by (\(m.referrers.count) objects, \(m.sources.count) root sources)") {
                    ForEach(m.sources, id: \.self) { s in
                        Label(snapshot.describe(source: s), systemImage: "arrow.up.to.line").font(.caption).lineLimit(2)
                    }
                    ForEach(Array(m.referrers.enumerated()), id: \.offset) { k, r in
                        HStack(spacing: 6) {
                            ObjectLink(snapshot: snapshot, object: r)
                            if let name = m.referrerNames[k] { Text(PathList.propertyName(name)).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1) }
                        }
                    }
                }

                if !m.references.isEmpty {
                    group("References (\(m.references.count))") {
                        ForEach(m.references, id: \.self) { r in ObjectLink(snapshot: snapshot, object: Int(r)) }
                    }
                }

                Expandable {
                    Text("Memory: \(m.words.count) words of the object as they are in the dump").font(.subheadline.weight(.semibold))
                } content: {
                    ForEach(m.words) { w in
                        HStack(spacing: 8) {
                            Text("+\(String(w.offset, radix: 16))").foregroundStyle(.secondary).frame(width: 44, alignment: .trailing)
                            Text(String(format: "%016llx", w.value))
                            if w.offset == 0 { Text("method table").foregroundStyle(.secondary) }
                            if let t = w.target { ObjectLink(snapshot: snapshot, object: t) }
                        }
                        .font(.caption.monospaced())
                    }
                }
            }
            .padding(12)
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }

    private func group(_ title: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.subheadline.weight(.semibold))
            content()
        }
    }
}

/// Fields with their values; struct fields open into their own fields.
struct FieldTree: View {
    let snapshot: HeapSnapshot
    let values: RustArray<HeapSnapshot.FieldValue>

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(values) { f in
                if f.children.isEmpty {
                    row(f).padding(.leading, 16)
                } else {
                    Expandable { row(f) } content: { FieldTree(snapshot: snapshot, values: f.children) }
                }
            }
        }
    }

    private func row(_ f: HeapSnapshot.FieldValue) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(f.field.name).font(.callout.monospaced()).frame(minWidth: 120, alignment: .leading)
                .help("\(f.field.typeName) at offset +\(String(f.field.offset, radix: 16)), declared in \(f.field.declaringType)")
            if let t = f.target {
                ObjectLink(snapshot: snapshot, object: t)
            } else {
                Text(f.display).font(.callout.monospaced()).foregroundStyle(f.display == "null" ? .secondary : .primary).textSelection(.enabled).lineLimit(2)
            }
        }
    }
}

/// Objects from a root or an unreferenced object down to the selected one, each with the field that holds it.
struct PathList: View {
    let snapshot: HeapSnapshot
    let path: RustArray<Int>
    let names: RustArray<String?>

    var body: some View {
        ForEach(Array(path.enumerated()), id: \.offset) { k, o in
            HStack(spacing: 4) {
                Image(systemName: "arrow.turn.down.right").font(.caption2).foregroundStyle(.secondary)
                if let name = names[k] { Text(Self.propertyName(name)).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle).help(Self.propertyName(name)) }
                ObjectLink(snapshot: snapshot, object: o)
            }
        }
    }

    static func propertyName(_ path: String) -> String { TypeNames.propertyName(path) }
}
