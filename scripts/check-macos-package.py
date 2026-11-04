#!/usr/bin/env python3
import pathlib
import plistlib
import re
import subprocess
import sys
import tarfile
import tempfile


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def check_signature(path):
    run("codesign", "--verify", "--deep", "--strict", str(path))
    signature = subprocess.run(
        ["codesign", "--display", "--verbose=2", str(path)],
        check=True, capture_output=True, text=True,
    )
    assert "Signature=adhoc" in signature.stderr, signature.stderr
    entitlements = plistlib.loads(subprocess.check_output(
        ["codesign", "--display", "--entitlements", "-", "--xml", str(path)],
        stderr=subprocess.DEVNULL,
    ))
    assert entitlements.get("com.apple.security.cs.debugger") is True, entitlements


def check_binary(path):
    assert path.is_file() and path.stat().st_mode & 0o111, path
    assert run("lipo", "-archs", str(path)) == "arm64", path
    for line in run("otool", "-L", str(path)).splitlines()[1:]:
        dependency = line.strip().split(" (", 1)[0]
        assert dependency.startswith(("/usr/lib/", "/System/Library/")), dependency
    commands = run("otool", "-l", str(path))
    minimum = re.search(r"cmd LC_BUILD_VERSION\n(?:(?!Load command).)*?minos\s+(\d+)\.(\d+)", commands, re.S)
    assert minimum and tuple(map(int, minimum.groups())) <= (15, 0), commands


def check_package(archive, version):
    with tarfile.open(archive) as package:
        members = package.getmembers()
        roots = {pathlib.PurePosixPath(member.name).parts[0] for member in members}
        assert roots == {"Dotheap.app", "dotheap", "LICENSE", "THIRD-PARTY-NOTICES.txt"}, roots
        for member in members:
            path = pathlib.PurePosixPath(member.name)
            assert not path.is_absolute() and ".." not in path.parts, path
            assert member.isfile() or member.isdir(), member.name
        with tempfile.TemporaryDirectory() as directory:
            package.extractall(directory, members=members)
            root = pathlib.Path(directory)
            app = root / "Dotheap.app"
            cli = root / "dotheap"
            check_signature(app)
            check_signature(cli)
            check_binary(cli)
            check_binary(app / "Contents/MacOS/DotheapViewer")
            assert run(str(cli), "--version") == version
            run(str(cli), "help")
            with (app / "Contents/Info.plist").open("rb") as stream:
                info = plistlib.load(stream)
            assert info["CFBundleShortVersionString"] == version, info
            assert info["LSMinimumSystemVersion"] == "15.0", info
            for name in ("LICENSE", "THIRD-PARTY-NOTICES.txt"):
                assert (root / name).read_bytes() == (app / "Contents/Resources" / name).read_bytes()
    print(f"Verified {archive}: ARM64, macOS 15, version {version}, signatures and debugger entitlements")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(f"Usage: {sys.argv[0]} ARCHIVE VERSION")
    check_package(pathlib.Path(sys.argv[1]), sys.argv[2])
