import Foundation
import RustHeapCoreFFI

struct RustAllocation: ~Copyable {
    let value: DHOwned
    deinit { value.destroy?(value.data) }
}

final class RustOwner: @unchecked Sendable {
    private let allocation: RustAllocation
    let pointer: UnsafeRawPointer

    init(_ value: DHOwned) {
        precondition(value.data != nil && value.destroy != nil)
        pointer = value.data!
        allocation = RustAllocation(value: value)
    }
}

public struct RustArray<Element>: RandomAccessCollection, @unchecked Sendable {
    private let owner: RustOwner?
    let storage: DHSlice
    private let read: (UnsafeRawPointer, RustOwner) -> Element
    public var startIndex: Int { 0 }
    public var endIndex: Int { Int(storage.len) }

    public init() {
        owner = nil
        storage = DHSlice()
        read = { _, _ in preconditionFailure("Empty Rust array") }
    }

    init(_ storage: DHSlice, owner: RustOwner,
         read: @escaping (UnsafeRawPointer, RustOwner) -> Element) {
        self.storage = storage
        self.owner = owner
        self.read = read
    }

    public subscript(index: Int) -> Element {
        precondition(index >= 0 && index < endIndex, "Rust array index is out of range")
        return withExtendedLifetime(owner) {
            read(storage.data!.advanced(by: index * Int(storage.stride)), owner!)
        }
    }

    public func slice(_ range: Range<Int>) -> Self {
        precondition(range.lowerBound >= 0 && range.upperBound <= count)
        guard let owner else { return Self() }
        return Self(DHSlice(data: storage.data?.advanced(by: range.lowerBound * Int(storage.stride)),
                            len: UInt(range.count), stride: storage.stride), owner: owner, read: read)
    }
}

extension RustArray where Element == UInt8 {
    public var booleans: RustArray<Bool> {
        guard let owner else { return RustArray<Bool>() }
        return RustArray<Bool>(storage, owner: owner) { pointer, _ in pointer.load(as: UInt8.self) != 0 }
    }
}

extension RustArray: Equatable where Element: Equatable {
    public static func == (lhs: Self, rhs: Self) -> Bool {
        guard lhs.storage.len == rhs.storage.len else { return false }
        if lhs.storage.data == rhs.storage.data && lhs.storage.stride == rhs.storage.stride { return true }
        return lhs.elementsEqual(rhs)
    }
}

public struct RustMap<Key, Value>: @unchecked Sendable, Equatable {
    private let pointer: UnsafeRawPointer
    private let owner: RustOwner
    private let get: (UnsafeRawPointer, Key) -> Value?
    private let equal: (UnsafeRawPointer, UnsafeRawPointer) -> Bool

    init(pointer: UnsafeRawPointer, owner: RustOwner,
         get: @escaping (UnsafeRawPointer, Key) -> Value?,
         equal: @escaping (UnsafeRawPointer, UnsafeRawPointer) -> Bool) {
        self.pointer = pointer
        self.owner = owner
        self.get = get
        self.equal = equal
    }

    public subscript(key: Key) -> Value? {
        withExtendedLifetime(owner) { get(pointer, key) }
    }

    public static func == (lhs: Self, rhs: Self) -> Bool {
        if lhs.pointer == rhs.pointer { return true }
        return withExtendedLifetime((lhs.owner, rhs.owner)) { lhs.equal(lhs.pointer, rhs.pointer) }
    }
}

func take(_ outcome: DHOutcome) throws -> RustOwner {
    if outcome.error.data != nil {
        let error = RustOwner(outcome.error)
        throw AnalysisError.Failed(message: withExtendedLifetime(error) {
            readString(dh_read_String(error.pointer))
        })
    }
    return RustOwner(outcome.value)
}

func checked(_ outcome: DHOutcome) -> RustOwner {
    do { return try take(outcome) }
    catch { preconditionFailure("\(error)") }
}

func readString(_ bytes: DHSlice) -> String {
    String(decoding: UnsafeBufferPointer(start: bytes.data?.assumingMemoryBound(to: UInt8.self),
                                        count: Int(bytes.len)), as: UTF8.self)
}

func withRustString<T>(_ string: String, _ body: (DHSlice) throws -> T) rethrows -> T {
    var string = string
    return try string.withUTF8 { bytes in
        try body(DHSlice(data: bytes.baseAddress, len: UInt(bytes.count), stride: 1))
    }
}

func withRustStrings<T>(_ strings: [String], _ body: (DHSlice) throws -> T) rethrows -> T {
    func borrow(_ index: Int, _ slices: inout [DHSlice]) throws -> T {
        if index == strings.count {
            return try slices.withUnsafeBufferPointer {
                try body(DHSlice(data: $0.baseAddress, len: UInt($0.count), stride: UInt(MemoryLayout<DHSlice>.stride)))
            }
        }
        return try withRustString(strings[index]) { slice in
            slices.append(slice)
            defer { slices.removeLast() }
            return try borrow(index + 1, &slices)
        }
    }
    var slices: [DHSlice] = []
    slices.reserveCapacity(strings.count)
    return try borrow(0, &slices)
}

func withOptionalRustString<T>(_ string: String?, _ body: (DHOptionalString) throws -> T) rethrows -> T {
    guard let string else { return try body(DHOptionalString(present: false, value: DHSlice())) }
    return try withRustString(string) { try body(DHOptionalString(present: true, value: $0)) }
}

func withRustPointers<T>(_ pointers: [UnsafeRawPointer], _ body: (DHSlice) throws -> T) rethrows -> T {
    try pointers.withUnsafeBufferPointer {
        try body(DHSlice(data: $0.baseAddress, len: UInt($0.count), stride: UInt(MemoryLayout<UnsafeRawPointer>.stride)))
    }
}

public enum AnalysisError: Error { case Failed(message: String) }

public protocol Progress: AnyObject, Sendable {
    func update(phase: String, fraction: Double)
    func cancelled() -> Bool
}

private final class ProgressOwner {
    let progress: any Progress
    init(_ progress: any Progress) { self.progress = progress }
}

func transferProgress(_ progress: any Progress) -> DHProgress {
    DHProgress(context: Unmanaged.passRetained(ProgressOwner(progress)).toOpaque(),
        update: { context, phase, fraction in
            Unmanaged<ProgressOwner>.fromOpaque(context!).takeUnretainedValue().progress
                .update(phase: readString(phase), fraction: fraction)
        },
        cancelled: { context in
            Unmanaged<ProgressOwner>.fromOpaque(context!).takeUnretainedValue().progress.cancelled()
        },
        destroy: { context in Unmanaged<ProgressOwner>.fromOpaque(context!).release() })
}
