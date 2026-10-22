# Development

See [Build](../README.md#build) for CLI and Viewer builds. Run the commands below from the repository
root. The script checks need Python 3 and the `zstd` command; the Swift tests run on macOS.

## Tests

Run the Rust suite on each supported platform:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Rebuild the bridge before Swift tests:

```sh
scripts/build-bridge.sh debug
swift test
```

The bridge script also prepares `swift build`. Its default profile is `release`.

Build the CLI before running the file-format and command checks:

```sh
cargo build --release --locked -p dotheap
python3 scripts/check-heap-contract.py target/release/dotheap
python3 scripts/check-core-headers.py target/release/dotheap
python3 scripts/check-compressed-core.py target/release/dotheap
python3 scripts/check-runtime-dump.py target/release/dotheap
```

On Linux, check live capture while mappings, files and threads change:

```sh
cargo build --release --locked -p heap-core --example live_dump_checks
sudo python3 Tests/LiveDump/check.py target/release/examples/live_dump_checks --cancellation
```

Synthetic tests cover Satori layouts, CoreCLR workstation and server heaps on x64 and arm64,
GC reference slots, finalization, dependent handles, dominators, metadata, compressed storage,
cache recovery and CLI output. Capture tests also truncate or remove mapped files, replace or
terminate the target, race output creation, and cancel capture.

## Real dumps and comparisons

`demo/` creates known managed and native allocations for `scripts/check-demo.sh`. A self-contained
publish (`dotnet publish demo -r <rid> --self-contained`) selects Satori through the
`PublishWithSatoriGC` SDK; other builds use stock CoreCLR. CI checks stock workstation GC, stock
server GC and Satori dumps on Linux, plus cross-platform reading. `scripts/check-while-running.py`
checks capture of a process that keeps writing memory.

To compare builds, record a reference and replay its commands with the candidate. Replay uses the
same addresses, type names, thread IDs and limits. Keep the cache directories separate:

```sh
python3 scripts/record-outputs.py ./reference core.dmp before --limit 0 -- --cache ./cache-before
python3 scripts/record-outputs.py ./candidate core.dmp after --cases before/commands.json -- --cache ./cache-after
python3 scripts/compare-outputs.py before after
```

The comparison checks every JSON value and array order. It prints the paths of differences and
returns a nonzero status if any differ. `times.txt` records command durations separately.
Recordings contain data from the dump and must be kept private.

## .NET version compatibility

The supported 10.0.1–10.0.13 range was checked against the corresponding release tags in
[dotnet/runtime](https://github.com/dotnet/runtime/compare/v10.0.1...v10.0.13) and
[dotnet/aspnetcore](https://github.com/dotnet/aspnetcore/compare/v10.0.1...v10.0.13).
The comparison covered 60 source files at every tag:

- Kestrel's request-state and HTTP-method enums are unchanged. The fields used for request and
  connection analysis remain compatible. The HTTP/3 abort field added in 10.0.13 does not affect
  request analysis, which reads fields by name.
- GC diagnostic variable and offset tables, GC descriptors, handle-table layouts, and the object,
  MethodTable, EEClass and FieldDesc fields used by Dotheap remain compatible. Changes to GC and
  thread internals are covered by the dump's GC offset tables and cDAC descriptor.
- Changes to managed socket and thread-pool scheduling internals preserve the fields Dotheap
  reads. Managed field offsets come from the dump, so adding or moving a field does not require
  a fixed-offset update in Dotheap.

Real-dump tests cover .NET 10.0.1, 10.0.9, 10.0.12 and 10.0.13, and ASP.NET Core 10.0.1, 10.0.12
and 10.0.13. Before extending the range, compare the affected source layouts, field names and
enum values, and test a dump from the new release. Keep warnings for releases outside the checked
range, previews and release candidates. Satori layout detection remains structural.

## Code and packaging

`crates/heap-core` owns dump parsing, analysis, caches and capture. `crates/dotheap` provides CLI
arguments and output. `crates/heap-bridge` exposes snapshot and query APIs through a generated C
ABI. `Sources/RustHeapCore` projects Rust storage into Swift, and `Sources/HeapCoreBridge` adapts
it for the SwiftUI Viewer. See [Swift and Rust interop](interop.md) for ownership, generated files
and bridge validation.

The workflows in `.github/workflows/` contain platform build and packaging steps. Release archives
and the Viewer include the [project license](../LICENSE) and
[third-party notices](../THIRD-PARTY-NOTICES.txt). After dependency updates, refresh the notices with:

```sh
python3 scripts/generate-notices.py
```

`third-party/` also records the Rust 1.94 standard library, LLVM runtime, musl and Swift
notices used for release builds, and the .NET/Satori source acknowledgments. Update these
when changing release toolchains. Runtime layouts, GC descriptors and metadata formats are
read from the dump using compatibility code in `heap-core`; Dotheap does not bundle .NET or Satori.

## Releases

1. Update the workspace version in `Cargo.toml` and the workspace entries in `Cargo.lock`.
2. Merge the changes and wait for CI to pass on that commit.
3. Tag that commit `v<version>`. The release workflow requires passing CI on the same commit
   and creates a draft release with source, binaries, license notices and `SHA256SUMS`.
4. Check the packaged binaries, the source-built Homebrew install and the release notes.
   Publish the draft using those same artifacts.
5. Update the version and source archive checksum in the Homebrew cask.

The macOS Homebrew cask downloads the versioned source archive, runs `scripts/build-macos.sh`,
installs the Viewer in `/Applications`, and links the CLI into Homebrew's `bin` directory.
Homebrew manages upgrades and removal of both. Rust and Swift build directories are removed
after installation. Xcode 16 or later is required. Locally built apps use an ad-hoc signature;
downloadable macOS binaries are not notarized.

The README screenshot uses `demo/` dumps: stock GC on Linux at the top, Satori on macOS at the
bottom. `scripts/shoot.sh` captures the Viewer; `scripts/make-collage.swift` combines the images.
