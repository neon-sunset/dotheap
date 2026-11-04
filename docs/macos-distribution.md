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
and links the CLI into Homebrew's `bin` directory. Installation requires no Rust,
Swift, or Xcode toolchain. The CLI links only to macOS libraries.

## Build and verify

```sh
scripts/build-macos.sh
scripts/package-macos.sh
```

The build signs both executables ad hoc with `assets/Debugger.entitlements`.
Packaging copies these signed files without stripping or signing them again. It
also produces the separate CLI archive and Viewer ZIP for direct downloads.

`scripts/check-macos-package.py` extracts the combined archive and checks its paths,
executable permissions, ARM64 architecture, macOS deployment targets, system-library
dependencies, CLI and app versions, licenses, signatures, and debugger entitlements.
CI and the release workflow run these checks before uploading the archives.

```sh
python3 scripts/check-macos-package.py dist/dotheap-macos-arm64-bundle.tar.gz 0.2.5
```

The package script accepts an output directory, app path, and CLI path as optional
arguments. To combine existing release binaries, verify their published checksums
first and use a checkout with the same version and license files. Publish the exact
verified archive: recreating an archive can change its checksum.

## Homebrew updates

A version tag must match `Cargo.toml`, and CI must have passed on that commit.
The release workflow creates a draft with all packages and `SHA256SUMS`. A manual
workflow run only uploads workflow artifacts.

Publish the release, including the combined archive and its `SHA256SUMS` entry,
before updating `neon-sunset/homebrew-tap`. Set the cask version and SHA-256 to that
archive's values. Do not use the source archive's checksum or `sha256 :no_check`.
Homebrew can prepare the version and checksum change with:

```sh
brew bump-cask-pr --write-only --version X.Y.Z neon-sunset/tap/dotheap
```

Review the resulting checksum against the downloaded archive and `SHA256SUMS`.
The cask's `github_latest` livecheck uses the published release, not draft-release
tags. It detects versions; it does not update the tap automatically.

Users run `brew upgrade --cask dotheap` for a new version. A cask change at the same
version does not trigger an upgrade; use `brew reinstall --cask dotheap` to replace
that installation. Homebrew removes the app and CLI link on uninstall and retains
dump files, preferences, and analysis caches.

The tap's `tests/check-dotheap.py` exercises checksum rejection, installation in
`/Applications`, upgrade from the source cask's directory layout, reinstall,
installation in an app directory containing spaces, and uninstall. It uses unique
app and CLI names and a temporary tap and trust configuration. The source-layout
fixture contains prebuilt files; it does not compile the old release.

## Quarantine and Gatekeeper

The fully qualified command installs and grants Homebrew trust for this cask:

```sh
brew install --cask neon-sunset/tap/dotheap
```

These binaries have ad-hoc signatures and the debugger entitlement. They have no
Developer ID signature or Apple notarization. The cask's `postflight_steps` runs
`/usr/bin/xattr -dr com.apple.quarantine` on the installed app and staged CLI.
The app path uses `{{appdir}}`, so custom installation directories are supported.
The CLI path uses `{{staged_path}}`; Homebrew links that executable into `bin`.
`-dr` succeeds when the attribute is already absent. Other attributes and the
embedded signatures remain intact. Command failures stop the installation.

This removes the quarantine that Homebrew applies to the downloaded app and CLI,
so they can run without a first-launch Gatekeeper approval. It does not change the
system Gatekeeper setting or grant debugger access. Capturing a live process still
requires macOS debugger access; opening an existing dump does not require it.

The hook uses Homebrew's supported install-step API. It does not make these
binaries notarized or eligible for the official `homebrew/cask` repository, whose
policy requires Gatekeeper acceptance. This distribution uses `neon-sunset/tap`.

The tap's test verifies that quarantine exists before the hook and is absent after
installation, while executable hashes, signatures, and entitlements are preserved.
It installs with no prior cask trust and no interactive input, then executes the
installed CLI. With `--launch`, it opens the Viewer through Launch Services,
checks that the application finished launching, and terminates that test instance.

References:

- [Homebrew tap trust](https://docs.brew.sh/Tap-Trust#installing-from-a-tap)
- [Homebrew Cask Cookbook](https://docs.brew.sh/Cask-Cookbook#stanza-flight_steps)
- [Official cask acceptance requirements](https://docs.brew.sh/Acceptable-Casks#platform-compatibility-and-macos-security-protections)
