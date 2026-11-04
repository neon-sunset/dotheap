# macOS distribution

The macOS release contains `dotheap-macos-arm64-bundle.tar.gz` for Apple silicon
running macOS 15 or later. Its top-level entries are:

```text
Dotheap.app/
dotheap
LICENSE
THIRD-PARTY-NOTICES.txt
```

The Homebrew cask installs the app in `/Applications` (or the configured `--appdir`)
and links the CLI into Homebrew's `bin` directory. Both binaries use macOS system
libraries.

## Build and verify

```sh
scripts/build-macos.sh
scripts/package-macos.sh
```

The build signs both executables ad hoc with `assets/Debugger.entitlements`.
Packaging preserves these signatures and produces the combined archive, a separate
CLI archive, and a Viewer ZIP. Optional arguments select the output directory,
app path, and CLI path.

`scripts/check-macos-package.py` extracts the combined archive and checks its paths,
executable permissions, ARM64 architecture, macOS deployment targets, system-library
dependencies, CLI and app versions, licenses, signatures, and debugger entitlements.
CI and the release workflow run these checks before uploading the archives.

## Homebrew updates

A version tag must match `Cargo.toml`, and CI must have passed on that commit.
CI retains its tested binaries and macOS packages for 14 days. The release workflow
uses those artifacts from a successful run on the tagged commit to create a draft
with all packages and `SHA256SUMS`. A manual workflow run uploads the packages as
workflow artifacts.

Publish the release, including the combined archive and its `SHA256SUMS` entry,
before updating `neon-sunset/homebrew-tap`. Set the cask version and SHA-256 to that
archive's values. Homebrew can prepare the version and checksum change with:

```sh
brew bump-cask-pr --write-only --version X.Y.Z neon-sunset/tap/dotheap
```

Review the resulting checksum against the downloaded archive and `SHA256SUMS`.
The cask's `github_latest` livecheck reports the latest published release version.

The tap's `tests/check-dotheap.py` checks checksums, installation, upgrades,
reinstalls, custom app directories, and removal. It uses temporary cask, app, and
CLI names and a temporary trust configuration.

## Quarantine and Gatekeeper

The fully qualified command installs and grants Homebrew trust for this cask:

```sh
brew install --cask neon-sunset/tap/dotheap
```

The binaries are ad-hoc signed with the debugger entitlement. The cask's
`postflight_steps` runs `/usr/bin/xattr -dr com.apple.quarantine` on the installed
app and staged CLI. The app path uses `{{appdir}}`, and the CLI path uses
`{{staged_path}}`. Homebrew links the staged executable into `bin`.

Quarantine removal lets the app and CLI run immediately after installation.
Other attributes and the embedded signatures remain intact. The binaries are
unnotarized, and live process capture requires macOS debugger access.

The tap's test verifies quarantine removal and checks executable hashes,
signatures, entitlements, and installed CLI execution. It installs with no prior
cask trust and no interactive input. With `--launch`, it opens the Viewer through
Launch Services, checks that the application finished launching, and terminates
the test instance.

References:

- [Homebrew tap trust](https://docs.brew.sh/Tap-Trust#installing-from-a-tap)
- [Homebrew Cask Cookbook](https://docs.brew.sh/Cask-Cookbook#stanza-flight_steps)
