import HeapCore
import SwiftUI

struct VerificationView: View {
    @Environment(\.dismiss) private var dismiss
    let path: String
    let snapshot: HeapSnapshot?
    var showsDone = true
    @State private var result: HeapVerification?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Heap integrity").font(.title2.weight(.semibold))
                Spacer()
                if showsDone { Button("Done") { dismiss() }.keyboardShortcut(.cancelAction) }
            }
            Text((path as NSString).lastPathComponent).foregroundStyle(.secondary)
            if let result {
                Text("\(result.objects.formatted()) objects, \(result.methodTables.formatted()) MethodTables, \(result.referenceSlots.formatted()) reference slots examined")
                if result.unparsedBytes > 0 { Text("\(Format.bytes(result.unparsedBytes)) unparsed; these bytes were not verified.").foregroundStyle(.secondary) }
                Text("\(result.invalid.formatted()) invalid, \(result.missingMemory.formatted()) missing memory, \(result.unverified.formatted()) not verified")
                    .fontWeight(.semibold)
                Text("Invalid values can indicate corruption or a capture during a GC or runtime update. Missing data does not establish corruption.")
                    .foregroundStyle(.secondary)
                List {
                    ForEach(0..<result.diagnostics.count, id: \.self) { i in
                        let d = result.diagnostics[i]
                        VStack(alignment: .leading, spacing: 5) {
                            HStack {
                                Text(d.structure).fontWeight(.semibold)
                                switch d.kind {
                                case .invalid: Text("Invalid").foregroundStyle(.red)
                                case .missingMemory: Text("Missing memory").foregroundStyle(.secondary)
                                case .unverified: Text("Not verified").foregroundStyle(.secondary)
                                }
                            }
                            HStack(spacing: 12) {
                                Text("Address \(Format.hex(d.address))")
                                if let object = d.object { Text("Object \(Format.hex(object))") }
                                if let value = d.value { Text("Value \(Format.hex(value))") }
                            }
                            .font(.callout.monospaced())
                            Text(d.message)
                        }
                        .textSelection(.enabled)
                        .padding(.vertical, 4)
                    }
                }
                let omitted = result.invalid + result.missingMemory + result.unverified - result.diagnostics.count
                if omitted > 0 { Text("\(omitted.formatted()) additional diagnostics; totals include them.").foregroundStyle(.secondary) }
                ForEach(0..<result.limitations.count, id: \.self) { i in
                    Text(result.limitations[i]).font(.callout).foregroundStyle(.secondary)
                }
            } else if let error {
                Text(error).foregroundStyle(.red).textSelection(.enabled)
                Spacer()
            } else {
                ProgressView("Verifying heap integrity").frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .padding(PageInsets.content)
        .frame(minWidth: 760, idealWidth: 900, minHeight: 520, idealHeight: 650)
        .task(id: path) {
            result = nil
            error = nil
            do {
                result = try await Task.detached(priority: .userInitiated) {
                    if let snapshot { return try snapshot.heapVerification() }
                    return try verifyHeap(path: path)
                }.value
            } catch { self.error = analysisErrorMessage(error) }
        }
    }
}
