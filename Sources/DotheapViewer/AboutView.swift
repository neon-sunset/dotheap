import AppKit
import SwiftUI

struct AboutCommands: Commands {
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .appInfo) {
            Button("About Dotheap") { openWindow(id: "about") }
        }
    }
}

struct AboutView: View {
    @State private var license: LicenseDocument?

    var body: some View {
        VStack(spacing: 12) {
            Image(nsImage: NSApplication.shared.applicationIconImage)
                .resizable().frame(width: 80, height: 80)
            Text("Dotheap").font(.title.weight(.semibold))
            if let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String {
                Text("Version \(version)").foregroundStyle(.secondary)
            }
            Text(Bundle.main.object(forInfoDictionaryKey: "NSHumanReadableCopyright") as? String
                 ?? "Copyright (C) 2026 Arseniy Zlobintsev")
            Text("Licensed under GNU AGPLv3 (AGPL-3.0-only).")
            Link("Source Code", destination: URL(string: "https://github.com/neon-sunset/dotheap")!)
            HStack {
                Button("License…") { license = .project }
                Button("Third-Party Licenses…") { license = .thirdParty }
            }
            .padding(.top, 4)
        }
        .padding(24)
        .frame(width: 440)
        .textSelection(.enabled)
        .sheet(item: $license) { LicenseView(document: $0) }
    }
}

enum LicenseDocument: String, Identifiable {
    case project, thirdParty

    var id: Self { self }
    var title: String { self == .project ? "Dotheap License" : "Third-Party Licenses" }
    var filename: String { self == .project ? "LICENSE" : "THIRD-PARTY-NOTICES.txt" }

    func read() throws -> String {
        guard let url = Bundle.main.url(forResource: filename, withExtension: nil) else {
            throw CocoaError(.fileReadNoSuchFile, userInfo: [NSFilePathErrorKey: filename])
        }
        return try String(contentsOf: url, encoding: .utf8)
    }
}

private struct LicenseView: View {
    let document: LicenseDocument
    @Environment(\.dismiss) private var dismiss
    @State private var contents = ""
    @State private var error: String?
    @State private var findRequest = 0

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(document.title).font(.headline)
            if let error {
                ContentUnavailableView("Cannot Open License", systemImage: "doc.text", description: Text(error))
            } else {
                LicenseTextView(contents: contents, findRequest: findRequest)
            }
            HStack {
                Button("Find…") { findRequest += 1 }
                    .keyboardShortcut("f").disabled(contents.isEmpty)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 820, height: 600)
        .task {
            do { contents = try document.read() }
            catch { self.error = error.localizedDescription }
        }
    }
}

private struct LicenseTextView: NSViewRepresentable {
    let contents: String
    let findRequest: Int

    final class Coordinator {
        var findRequest = 0
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSTextView.scrollableTextView()
        scrollView.borderType = .bezelBorder
        scrollView.findBarPosition = .aboveContent
        let textView = scrollView.documentView as! NSTextView
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = false
        textView.usesFindBar = true
        textView.isIncrementalSearchingEnabled = true
        textView.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        textView.textContainerInset = NSSize(width: 12, height: 12)
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        let textView = scrollView.documentView as! NSTextView
        if textView.string != contents { textView.string = contents }
        if context.coordinator.findRequest != findRequest {
            context.coordinator.findRequest = findRequest
            let item = NSMenuItem()
            item.tag = NSTextFinder.Action.showFindInterface.rawValue
            textView.window?.makeFirstResponder(textView)
            textView.performFindPanelAction(item)
        }
    }
}
