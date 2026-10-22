import HeapCore
import SwiftUI

/// File I/O that the memory shows: open files of managed and native code in one list, file paths that native C++
/// objects keep, and the files the process maps.
struct FilesView: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    @State private var text = ""
    @State private var openOrder: TableSort? = TableSort(column: "fd", ascending: true)
    @State private var nativeOrder: TableSort? = TableSort(column: "Path", ascending: true)
    @State private var mappedOrder: TableSort? = TableSort(column: "Kind", ascending: true)
    @State private var selectedFile: String?
    @State private var selectedNative: String?

    enum Mode: String, CaseIterable, Identifiable {
        case open = "Open files", native = "Paths in native objects", mapped = "Mapped files"
        var id: String { rawValue }
    }

    /// An open file of managed code (a SafeFileHandle) or of native code (a C++ file object).
    struct FileRow: Identifiable {
        let id: String
        let fd: Int64?
        /// The descriptor of a native file is read from the integer after its path.
        let fdEstimated: Bool
        let path: String
        let openedBy: String
        let access: String
        let position: Int64?
        let unwritten: String
        let now: String
        let heldBy: String
        let managed: HeapSnapshot.OpenFile?
        let native: HeapSnapshot.NativeFileName?
        var fdValue: Int64 { fd ?? Int64.max }
        var positionValue: Int64 { position ?? -1 }
    }

    struct NativeRow: Identifiable {
        let id: String
        let className: String
        let path: String
        let chunks: [Int]
        let fd: Int64?
        var count: Int { chunks.count }
        var fdValue: Int64 { fd ?? Int64.max }
    }

    var body: some View {
        Computed(snapshot, key: "files page", label: "Reading the files", compute: { try $0.files() }) { model in
            content(model)
        }
    }

    private func rows(_ f: HeapSnapshot.Files) -> [FileRow] {
        let managed = f.open.map { o in
            FileRow(id: "m\(o.handle)", fd: o.fd, fdEstimated: false,
                    path: o.path ?? o.memoryBacking.map { "no file: the memory of a MemoryMappedFile (\(Format.bytes(UInt64($0))), memfd)" } ?? "no path",
                    openedBy: o.stream != nil ? "FileStream" : o.memoryBacking != nil ? "MemoryMappedFile" : "SafeFileHandle",
                    access: o.access ?? "-", position: o.position,
                    unwritten: o.unwritten.map { "\(Format.bytes(UInt64($0))) of \(Format.bytes(UInt64(o.bufferSize ?? 0)))" } ?? "-",
                    now: o.pending ?? (o.extraReferences > 0 ? "\(o.extraReferences) extra references" : "-"), heldBy: o.owner, managed: o, native: nil)
        }
        let native = f.native.filter { $0.className.contains("File") || $0.className.contains("Directory") }.map { n in
            FileRow(id: "n\(n.chunk)/\(n.path)", fd: n.fd, fdEstimated: n.fd != nil, path: n.path, openedBy: n.className, access: "-", position: nil,
                    unwritten: "-", now: "-", heldBy: "native code", managed: nil, native: n)
        }
        return managed + native
    }

    private func content(_ f: HeapSnapshot.Files) -> some View {
        @Bindable var session = session
        let all = rows(f)
        let native = Dictionary(grouping: f.native, by: { $0.className + "\t" + $0.path })
            .map { NativeRow(id: $0.key, className: $0.value[0].className, path: $0.value[0].path, chunks: $0.value.map(\.chunk), fd: $0.value.compactMap(\.fd).first) }
        let data = f.mapped.filter { $0.kind == "data" }
        return VStack(alignment: .leading, spacing: 10) {
            Text("Files").font(.title2.weight(.semibold))
            Text("Managed SafeFileHandle and FileStream objects, native C++ file objects and stored paths, and mapped files. Native descriptors are estimated from the integer after a path in a C++ object. The dump has no kernel file table, so this is not a complete list of open files.")
                .foregroundStyle(.secondary).frame(maxWidth: .infinity, alignment: .leading)
            MetricRow {
                Metric(title: "Open files", value: all.filter { $0.fd != nil }.count.formatted(),
                       detail: "\(f.open.filter { $0.fd != nil }.count) managed, \(all.filter { $0.native != nil && $0.fd != nil }.count) native")
                Metric(title: "Pending", value: f.open.filter { $0.pending != nil }.count.formatted(), detail: "async reads and writes", hideZero: true)
                Metric(title: "Unwritten", value: Format.bytes(UInt64(f.open.reduce(0) { $0 + ($1.unwritten ?? 0) })), detail: "in FileStream buffers", hideZero: true)
                Metric(title: "Native objects", value: native.count.formatted(), detail: "\(Set(f.native.map(\.path)).count) file paths", hideZero: true)
                Metric(title: "Mapped data files", value: data.count.formatted(), detail: Format.bytes(data.reduce(0) { $0 + $1.bytes }), hideZero: true)
            }
            HStack(spacing: 8) {
                Picker("Show", selection: $session.filesMode) { ForEach(Mode.allCases) { Text($0.rawValue).tag($0) } }
                    .labelsHidden().pickerStyle(.segmented).fixedSize()
                TextField("Filter by path, class or owner", text: $text).textFieldStyle(.roundedBorder).frame(maxWidth: 320)
            }
            switch session.filesMode {
            case .open: openFiles(all)
            case .native: nativeObjects(native)
            case .mapped: mappedFiles(f.mapped)
            }
        }
        .padding(16)
    }

    private func matches(_ values: String?...) -> Bool { text.isEmpty || values.contains { ($0 ?? "").localizedCaseInsensitiveContains(text) } }

    private func openFiles(_ all: [FileRow]) -> some View {
        let rows = text.isEmpty ? all : all.filter { matches($0.path, $0.heldBy, $0.openedBy) }
        return SplitPanes(axis: .horizontal, firstMin: 360, secondMin: 280, initial: 0.65) {
            TextTable(rows: rows, columns: [
                .init("fd", width: 55, font: .digits, value: \.fdValue, text: { r in r.fd.map { String($0) + (r.fdEstimated ? "?" : "") } ?? (r.managed != nil ? "closed" : "-") },
                      color: { $0.fd == nil ? .secondaryLabelColor : nil },
                      help: { $0.fdEstimated ? "Estimated from the integer after the path in the C++ object" : "" }),
                .init("Path", width: 340, minWidth: 160, flexible: true, truncation: .byTruncatingMiddle, value: \.path, text: \.path, help: { $0.path }),
                .init("Opened by", width: 170, minWidth: 80, truncation: .byTruncatingMiddle, value: \.openedBy, text: \.openedBy),
                .init("Access", width: 80, value: \.access, text: \.access),
                .init("Position", width: 90, minWidth: 50, font: .digits, value: \.positionValue, text: { $0.position.map { $0.formatted() } ?? "-" }),
                .init("Unwritten", width: 100, minWidth: 60, font: .digits, value: \.unwritten, text: \.unwritten),
                .init("Now", width: 120, minWidth: 60, value: \.now, text: \.now,
                      color: { $0.managed?.pending != nil ? NSColor(Palette.queued) : .secondaryLabelColor },
                      help: { _ in "Extra handle references can come from active I/O or DangerousAddRef, including a file mapping. They do not by themselves prove that an I/O operation is pending." }),
                .init("Held by", width: 220, minWidth: 100, truncation: .byTruncatingMiddle, value: \.heldBy, text: \.heldBy, help: { $0.heldBy }),
            ], sort: $openOrder, selection: selectedFile, select: { selectedFile = $0 })
        } second: {
            if let r = rows.first(where: { $0.id == selectedFile }) { FileDetail(snapshot: snapshot, row: r).id(r.id) } else { Placeholder("Select a file.") }
        }
    }

    private func nativeObjects(_ list: [NativeRow]) -> some View {
        let rows = text.isEmpty ? list : list.filter { matches($0.path, $0.className) }
        return Group {
            if rows.isEmpty {
                Text(snapshot.dump.platform.isLinux ? "No C++ object keeps a file path in a std::string." : "Native objects are read from glibc malloc chunks, so only Linux cores have them.")
                    .foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                SplitPanes(axis: .horizontal, firstMin: 360, secondMin: 280, initial: 0.65) {
                    TextTable(rows: rows, columns: [
                        .init("Path", width: 420, minWidth: 200, flexible: true, truncation: .byTruncatingMiddle, value: \.path, text: \.path, help: { $0.path }),
                        .init("C++ class", width: 260, minWidth: 120, truncation: .byTruncatingMiddle, value: \.className, text: \.className),
                        .init("fd", width: 60, font: .digits, value: \.fdValue, text: { $0.fd.map { "\($0)?" } ?? "-" }),
                        .init("Objects", width: 60, font: .digits, value: \.count, text: { $0.count.formatted() }),
                    ], sort: $nativeOrder, selection: selectedNative, select: { selectedNative = $0 })
                } second: {
                    if let r = rows.first(where: { $0.id == selectedNative }) { NativeChunkPane(snapshot: snapshot, chunk: r.chunks[0]).id(r.id) } else { Placeholder("Select a path.") }
                }
            }
        }
    }

    private func mappedFiles(_ list: RustArray<HeapSnapshot.MappedFile>) -> some View {
        TextTable(rows: text.isEmpty ? AnyRandomAccessCollection(list) : AnyRandomAccessCollection(list.filter { matches($0.path, $0.kind) }), columns: [
            .init("Path", width: 480, minWidth: 200, flexible: true, truncation: .byTruncatingMiddle, value: \.path, text: \.shownPath, help: { $0.shownPath }),
            .init("Kind", width: 100, minWidth: 60, ascending: { ($0.kindOrder, $1.bytes) < ($1.kindOrder, $0.bytes) }, text: \.kind,
                  color: { $0.kind == "data" ? nil : .secondaryLabelColor }),
            .init("Mapped", width: 90, minWidth: 60, font: .digits, value: \.bytes, text: { Format.bytes($0.bytes) }),
        ] + (snapshot.agreeingResidency == nil ? [] : [
            .init("Resident", width: 90, minWidth: 60, font: .digits, value: { $0.resident ?? 0 },
                  text: { Format.bytes($0.resident ?? 0) }, help: { _ in "RSS of the file's mappings per smaps" }),
        ]) + [
            .init("Mappings", width: 70, font: .digits, value: \.mappings, text: { $0.mappings.formatted() }),
        ], sort: $mappedOrder)
    }
}

extension HeapSnapshot.MappedFile: Identifiable {
    public var id: String { path }
    /// Data files first, then assemblies, then native code.
    var kindOrder: Int { ["data": 0, "assembly": 1, "native code": 2][kind] ?? 3 }
    var shownPath: String { deleted ? path + " (deleted)" : path }
}

/// The selected open file: the objects of a managed file, or the C++ object of a native one.
private struct FileDetail: View {
    let snapshot: HeapSnapshot
    let row: FilesView.FileRow

    var body: some View {
        if let n = row.native {
            NativeChunkPane(snapshot: snapshot, chunk: n.chunk)
        } else if let o = row.managed {
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    Text(row.path).font(.headline).textSelection(.enabled)
                    Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                        GridRow { Text("Handle").foregroundStyle(.secondary); ObjectLink(snapshot: snapshot, object: o.handle) }
                        if let s = o.stream { GridRow { Text("Stream").foregroundStyle(.secondary); ObjectLink(snapshot: snapshot, object: s) } }
                        GridRow {
                            Text("Held by").foregroundStyle(.secondary)
                            HStack(spacing: 8) {
                                Text(o.owner)
                                if let h = o.ownerObject { ObjectLink(snapshot: snapshot, object: h, showSize: false) }
                            }
                        }
                    }
                    RootPath(snapshot: snapshot, object: o.ownerObject ?? o.stream ?? o.handle)
                }
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

/// A malloc chunk shown in place, with a button to open it on the Native Memory page.
private struct NativeChunkPane: View {
    @Environment(Session.self) private var session
    let snapshot: HeapSnapshot
    let chunk: Int

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Spacer()
                Button("Show in Native Memory") { session.show(.chunk(chunk), in: snapshot) }
            }
            .padding(8)
            Computed(snapshot, key: "native graph wrapper", label: "Reading native memory", compute: { try $0.nativeGraph }) { graph in
                ChunkDetail(snapshot: snapshot, graph: graph, chunk: chunk)
            }
        }
    }
}

extension HeapSnapshot.OpenFile: Identifiable { public var id: Int { handle } }
