import RustHeapCore

extension TaskGraphs {
    public typealias Node = TaskNode
    public typealias Graph = TaskGraph
    public typealias Group = TaskGroup
    public static func fieldName(_ name: String) -> String? { RustHeapCore.stateMachineFieldName(name: name) }

    public func graph(containing object: Int) -> (group: Int, graph: Int)? {
        for (groupIndex, group) in groups.enumerated() {
            for (graphIndex, graph) in group.graphs.enumerated() {
                if graph.roots.descendants.contains(where: { $0.objects.contains(object) || $0.this.contains(object) || $0.resources.contains(object) }) {
                    return (groupIndex, graphIndex)
                }
            }
        }
        return nil
    }
}

extension Collection where Element == TaskNode {
    var descendants: [TaskNode] {
        var result: [TaskNode] = []
        var stack = reversed().map { $0 }
        while let node = stack.popLast() {
            result.append(node)
            stack.append(contentsOf: node.children.reversed())
        }
        return result
    }
}

extension TaskNode {
    public typealias Kind = TaskNodeKind
    public var isCall: Bool { kind == .method || kind == .task || kind == .thread }
}

extension TaskGraph: Identifiable {
    public var id: Int { roots[0].objects[0] }
    public var tasks: Int { roots.reduce(0) { $0 + $1.objects.count } }
    public var isShared: Bool { waitedOn != nil && tasks > 1 }
}

extension TaskGroup: Identifiable {
    public typealias State = TaskState
    public var outermostCalls: Int { graphs.reduce(0) { $0 + $1.tasks } }
    public var isShared: Bool { graphs.first?.isShared ?? false }
    public var outermost: String {
        Self.list(graphs.first?.roots.map { $0.detail.isEmpty ? $0.title : $0.title + " (" + $0.detail + ")" } ?? [])
    }
    public var suspendedOn: String {
        guard let first = graphs.first else { return "" }
        if !first.suspensions.isEmpty { return Self.list(first.suspensions.map(\.title)) }
        let cycles = first.roots.descendants.filter { $0.kind == .cycle }
        if !cycles.isEmpty { return Self.list(cycles.map(\.title)) }
        return first.runningCalls > 0 ? "running" : first.queuedCalls > 0 ? "queued for a thread pool thread" : ""
    }
    public var summary: String {
        var parts: [String] = []
        var level = graphs.first.map { Array($0.roots) } ?? []
        while !level.isEmpty {
            parts.append(Self.list(level.map(\.text)))
            level = level.flatMap(\.children)
        }
        return (parts.count > 4 ? Array(parts.prefix(2)) + ["...", parts.last!] : parts).joined(separator: " → ")
    }

    private static func list(_ items: [String]) -> String {
        var seen = Set<String>()
        let unique = items.filter { seen.insert($0).inserted }
        return unique.prefix(2).joined(separator: " + ") + (unique.count > 2 ? " + \(unique.count - 2) more" : "")
    }
}

extension TaskState {
    public var rawValue: String {
        switch self {
        case .running: "running"
        case .queued: "queued"
        case .waiting: "waiting"
        case .neverResumes: "never resumes"
        case .cycle: "waits on itself"
        case .dropped: "dropped"
        }
    }
}
