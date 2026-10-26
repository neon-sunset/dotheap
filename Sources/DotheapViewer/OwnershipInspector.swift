import HeapCore
import SwiftUI

struct OwnershipInspection: Sendable {
    let managed: UInt64
    let native: UInt64
    let dominator: OwnershipReference?
    let path: [OwnershipReference]

    init(_ snapshot: HeapSnapshot, graph: OwnershipGraph, node: Int) throws {
        managed = graph.retainedManaged[node]
        native = graph.retainedNative[node]
        let parent = graph.idom[node]
        dominator = parent >= 0 ? try OwnershipReference(snapshot, graph: graph, index: Int(parent)) : nil
        path = try graph.pathToRoot(node).dropFirst().map { try OwnershipReference(snapshot, graph: graph, index: $0) }
    }
}

struct OwnershipReference: Identifiable, Sendable {
    let id: Int
    let node: OwnershipNode
    let name: String
    let address: UInt64?

    init(_ snapshot: HeapSnapshot, graph: OwnershipGraph, index: Int) throws {
        id = index
        node = graph.node(index)
        switch node {
        case let .chunk(chunk):
            let native = try snapshot.nativeGraph
            let attribution = try native.attribution(chunk)
            name = attribution.kind.displayName(attribution.label)
            address = native.chunks[chunk].userStart
        default:
            name = try snapshot.ownershipName(index)
            if case let .object(object) = node { address = snapshot.heap.addresses[object] }
            else { address = nil }
        }
    }
}

struct OwnershipLink: View {
    let snapshot: HeapSnapshot
    let reference: OwnershipReference

    var body: some View {
        switch reference.node {
        case let .object(object): ObjectLink(snapshot: snapshot, object: object)
        case let .chunk(chunk):
            NativeLink(snapshot: snapshot, chunk: chunk, name: reference.name)
        case .source: Label(reference.name, systemImage: "arrow.up.to.line").font(.caption)
        case .root: Text("Shared across root sources").font(.caption).foregroundStyle(.secondary)
        }
    }
}
