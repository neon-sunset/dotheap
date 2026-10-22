#!/bin/sh
# Builds the viewer in release mode and wraps it in Dotheap.app at the repository root. Extra arguments go to swift build.
set -eu
cd "$(dirname "$0")/.."
scripts/build-bridge.sh release
# Swift Build links with --sysroot, and clang then writes the deployment target as the SDK version. macOS runs an app
# built for an older SDK with the window style of that release, so clang gets the SDK with -isysroot.
sdk=$(xcrun --sdk macosx --show-sdk-path)
swift build -c release --product DotheapViewer -Xswiftc -Xclang-linker -Xswiftc -isysroot -Xswiftc -Xclang-linker -Xswiftc "$sdk" "$@"
app=Dotheap.app
version=$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
binary_dir=$(swift build -c release --show-bin-path "$@")
cp "$binary_dir/DotheapViewer" "$app/Contents/MacOS/"
cp assets/Dotheap.icns "$app/Contents/Resources/"
cp LICENSE THIRD-PARTY-NOTICES.txt "$app/Contents/Resources/"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>DotheapViewer</string>
    <key>CFBundleIconFile</key><string>Dotheap</string>
    <key>CFBundleIdentifier</key><string>io.github.neon-sunset.dotheap</string>
    <key>CFBundleName</key><string>Dotheap</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>NSHumanReadableCopyright</key><string>Copyright (C) 2026 Arseniy Zlobintsev</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Core dump</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array><string>io.github.neon-sunset.dotheap.dmp</string><string>io.github.neon-sunset.dotheap.zst</string></array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key><string>smaps</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array><string>io.github.neon-sunset.dotheap.smaps</string></array>
        </dict>
    </array>
    <key>UTImportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key><string>io.github.neon-sunset.dotheap.dmp</string>
            <key>UTTypeDescription</key><string>Core dump</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string></array>
            <key>UTTypeTagSpecification</key><dict><key>public.filename-extension</key><array><string>dmp</string></array></dict>
        </dict>
        <dict>
            <key>UTTypeIdentifier</key><string>io.github.neon-sunset.dotheap.zst</string>
            <key>UTTypeDescription</key><string>Zstandard file</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string></array>
            <key>UTTypeTagSpecification</key><dict><key>public.filename-extension</key><array><string>zst</string></array></dict>
        </dict>
        <dict>
            <key>UTTypeIdentifier</key><string>io.github.neon-sunset.dotheap.smaps</string>
            <key>UTTypeDescription</key><string>smaps</string>
            <key>UTTypeConformsTo</key><array><string>public.data</string><string>public.plain-text</string></array>
            <key>UTTypeTagSpecification</key><dict><key>public.filename-extension</key><array><string>smaps</string></array></dict>
        </dict>
    </array>
</dict>
</plist>
PLIST
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
echo "$app"
