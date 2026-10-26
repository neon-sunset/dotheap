import SwiftUI

struct SettingsView: View {
    @AppStorage("typeNameStyle") private var typeNameStyle = TypeNameStyle.standard
    @AppStorage("downloadRuntimeSymbols") private var downloadRuntimeSymbols = true
    @AppStorage("suggestDumpConversion") private var suggestDumpConversion = true

    var body: some View {
        Form {
            SwiftUI.Section("Dumps") {
                Toggle("Suggest converting dumps that require sequential decompression", isOn: $suggestDumpConversion)
            }
            SwiftUI.Section("Display") {
                Picker("Type names", selection: $typeNameStyle) {
                    ForEach(TypeNameStyle.allCases) { Text($0.title).tag($0) }
                }
                Text("Names are abbreviated only when they do not fit. Tables keep one line. Hover to see the full name.")
                    .font(.caption).foregroundStyle(.secondary)
                LabeledContent("Preview") {
                    TypeNameText(name: "System.Collections.Generic.Dictionary<System.String, System.Threading.Tasks.Task>", style: typeNameStyle)
                        .frame(width: 320, alignment: .leading)
                }
            }
            SwiftUI.Section("Symbols") {
                Toggle("Download runtime symbols when a dump opens", isOn: $downloadRuntimeSymbols)
                Text("Download matching .NET debug files from Microsoft's symbol server for native stack names.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .frame(width: 570)
        .fixedSize(horizontal: false, vertical: true)
    }
}
