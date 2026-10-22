// swift-tools-version: 6.0
import Foundation
import PackageDescription

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path

let package = Package(
    name: "dotheap",
    platforms: [.macOS(.v15)],
    products: [.executable(name: "DotheapViewer", targets: ["DotheapViewer"])],
    targets: [
        .systemLibrary(name: "RustHeapCoreFFI", path: "Sources/RustHeapCoreFFI"),
        .target(
            name: "RustHeapCore", dependencies: ["RustHeapCoreFFI"],
            path: "Sources/RustHeapCore",
            linkerSettings: [
                .unsafeFlags(["-L" + root + "/.build/rust/lib"]),
                .linkedLibrary("heap_bridge"),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
            ]
        ),
        .target(name: "HeapCore", dependencies: ["RustHeapCore"], path: "Sources/HeapCoreBridge"),
        .executableTarget(name: "DotheapViewer", dependencies: ["HeapCore"]),
        .testTarget(name: "HeapCoreBridgeTests", dependencies: ["HeapCore"]),
        .testTarget(name: "DotheapViewerTests", dependencies: ["DotheapViewer"]),
    ],
    swiftLanguageModes: [.v5]
)
