#!/usr/bin/env python3
"""Check heap layouts and GC references through synthetic ELF cores and the CLI."""
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

PAGE, REGION, IMAGE = 0x40000000, 0x40200000, 0x10000000
FIRST, MT = REGION + 36888, PAGE + 0x2000
OFFSETS = {
    "legacy": (80, 88, 112, 128, 136, 144, 152, 168, 176),
    "previous": (80, 88, 112, 136, 144, 152, 160, 176, 184),
    "current": (88, 96, 120, 144, 152, 160, 168, 220, 228),
}
COMMIT = "9df7b3d86e4851514c5a1022ba2e54b16350e6a4"
LAYOUTS = {
    "legacy": "Satori before 3ed9d3c8680",
    "previous": "Satori from 3ed9d3c8680 through dc9a20118d2",
    "current": "Satori since 9df7b3d86e4 (2026-10-15)",
}


def note(name, kind, desc):
    return (struct.pack("<III", len(name), len(desc), kind) + name
            + bytes(-len(name) % 4) + desc + bytes(-len(desc) % 4))


def header(kind, count):
    return struct.pack("<16sHHIQQQIHHHHHH", b"\x7fELF\x02\x01\x01" + bytes(9),
                       kind, 62, 1, 0, 64, 0, 0, 64, 56, count, 0, 0, 0)


def segment(kind, offset, address, size):
    return struct.pack("<IIQQQQQQ", kind, 6, offset, address, 0, size, size, 4096)


def core(path, *, layout="current", base=32, component=0, length=0,
         series=((8, 8),), runs=(), offset=0, refs=None, rooted=False,
         finalizable=False, commit=None, build=None, version="42.42.42.42424",
         empty=False, invalid=None, storage=None):
    used, committed, trackers, prev, nxt, queue, occupancy, count, pinned = OFFSETS[layout]
    memory = bytearray(0x410000 if storage == "gc-data" else 0x210000)

    def write(address, value, fmt="Q"):
        struct.pack_into("<" + fmt, memory, address - PAGE, value)

    size = (base + component * length + 7) & ~7
    addresses = [FIRST, FIRST + size + 80, FIRST + size + 104]
    write(PAGE + 8, PAGE + (1 << 30))
    write(PAGE + 24, REGION)
    write(PAGE + 56, PAGE + 0x400)
    memory[0x401] = 1
    write(REGION + 16, -1 if storage else 2, "i")
    write(REGION + 24, REGION + (1 << 21))
    write(REGION + 32, PAGE)
    write(REGION + used, FIRST - 8 if empty else addresses[-1] + 24)
    write(REGION + committed, REGION + 0x10000)
    write(REGION + queue, 0 if empty or storage == "gc-data" else PAGE + 0x3000)
    write(PAGE + 0x3000, 0 if storage else 4, "I")
    write(REGION + occupancy, REGION + (1 << 21) - FIRST if storage else 0 if empty else size + 48)
    write(REGION + count, 0 if empty or storage else 3, "I")
    write(REGION + pinned, 0 if empty or storage else 1, "B")
    if storage:
        write(REGION + 64, FIRST if storage == "free" else addresses[-1] + 24)
        write(REGION + 72, REGION + (1 << 21))
    if storage == "gc-data":
        free = REGION + (1 << 21)
        memory[0x402] = 1
        write(free + 16, -1, "i")
        write(free + 24, free + (1 << 21))
        write(free + 32, PAGE)
        write(free + 64, free + 36888)
        write(free + 72, free + (1 << 21))
        write(free + used, free + 36880)
        write(free + committed, free + 0x10000)
        write(free + queue, PAGE + 0x3000)
        write(free + occupancy, (1 << 21) - 36888)
    if invalid == "bounds":
        write(REGION + used, REGION + 0x10008)
    if invalid == "committed-past-end":
        write(REGION + committed, REGION + (1 << 22))

    flags = (0x01000000 if series or runs else 0) | (0x80000000 | component if component else 0)
    write(MT, flags, "I")
    write(MT + 4, base, "I")
    slots = []
    if runs:
        write(MT - 8, -len(runs), "q")
        write(MT - 16, offset)
        slot = offset
        for i, (pointers, skip) in enumerate(runs):
            write(MT - 24 - i * 8, pointers, "I")
            write(MT - 20 - i * 8, skip, "I")
        for _ in range(length):
            for pointers, skip in runs:
                slots.extend(range(slot, slot + pointers * 8, 8))
                slot += pointers * 8 + skip
    elif series:
        write(MT - 8, len(series))
        for i, (start, byte_count) in enumerate(series):
            write(MT - 24 - i * 16, byte_count - base, "q")
            write(MT - 16 - i * 16, start)
            slots.extend(range(start, start + byte_count + component * length, 8))
    if invalid == "descriptor":
        write(MT - 8, 0)

    write(FIRST, MT)
    for slot in range(8, size, 8):
        write(FIRST + slot, addresses[1])
    if component:
        write(FIRST + 8, length, "I")
    for slot in slots:
        write(FIRST + slot, 0)
    for slot, target in (refs or {}).items():
        write(FIRST + slot, addresses[target])

    write(PAGE + 0x2100, 0x80000001, "I")
    write(PAGE + 0x2104, 24, "I")
    write(FIRST + size, PAGE + 0x2100)
    write(FIRST + size + 8, 56, "I")
    write(FIRST + size + 12, 32 if layout == "current" else 0, "I")
    write(PAGE + 0x2200, 0x00100000 if finalizable else 0, "I")
    write(PAGE + 0x2204, 24, "I")
    for address in addresses[1:]:
        write(address, PAGE + 0x2200)
    if finalizable:
        write(REGION + trackers, PAGE + 0x4000)
        write(PAGE + 0x4000, 1)
        write(PAGE + 0x4010, addresses[1])

    image = bytearray(4096)
    image[:64] = header(3, int(build is not None))
    if build is not None:
        build_note = note(b"GNU\0", 3, bytes.fromhex(build))
        image[64:120] = segment(4, 256, 256, len(build_note))
        image[256:256 + len(build_note)] = build_note
    if version is not None:
        version_text = ("@(#)Version " + version + (" @Commit: " + commit if commit else "")).encode() + b"\0"
        image[512:512 + len(version_text)] = version_text
    files = struct.pack("<QQQQQ", 1, 4096, IMAGE, IMAGE + 4096, 0) + b"/fixture/libcoreclr.so\0"
    notes = note(b"CORE\0", 0x46494c45, files)
    if rooted:
        status = bytearray(336)
        struct.pack_into("<I", status, 32, 1)
        struct.pack_into("<Q", status, 112, FIRST)
        notes += note(b"CORE\0", 1, status)
    data = bytearray(8192 + len(memory))
    data[:64] = header(4, 3)
    data[64:120] = segment(4, 512, 0, len(notes))
    data[120:176] = segment(1, 4096, IMAGE, len(image))
    data[176:232] = segment(1, 8192, PAGE, len(memory))
    data[512:512 + len(notes)] = notes
    data[4096:8192] = image
    data[8192:] = memory
    path.write_bytes(data)


def column(cache, name, fmt, count=None):
    data = (cache / (name + ".col")).read_bytes()
    if count is not None:
        data = data[:count * struct.calcsize("<" + fmt)]
    return [v[0] for v in struct.iter_unpack("<" + fmt, data)]


def check(binary, directory, name, *, expected=(), error=None, **options):
    dump, cache = directory / (name + ".dmp"), directory / (name + "-cache")
    core(dump, **options)

    def run(command):
        return subprocess.run([binary, command, str(dump), "--cache", str(cache), "--json"],
                              capture_output=True, text=True)

    result = run("summary")
    if error:
        assert result.returncode != 0 and error in result.stderr, (name, result.stderr)
    else:
        assert result.returncode == 0, (name, result.stderr)
        summary = json.loads(result.stdout)
        assert summary["dump"]["gcLayout"] == LAYOUTS[options.get("layout", "current")], (name, summary["dump"])
        assert not any(w.startswith("the runtime ") for w in summary["warnings"]), (name, summary["warnings"])
        empty = options.get("empty", False) or options.get("storage") is not None
        assert summary["dump"]["objects"] == (0 if empty else 3), (name, summary["dump"])
        assert summary["dump"]["pendingFinalizationObjects"] == int(options.get("finalizable", False))
        if options.get("storage"):
            assert summary["managedHeap"]["unparsed"] == (0 if options["storage"] == "free" else 160), name
            assert summary["managedHeap"]["freeGaps"] == (160 if options["storage"] == "free" else 0), name
        if not empty:
            assert column(cache, "references.start", "Q") == [0, len(expected), len(expected), len(expected)], name
            assert column(cache, "references.items", "i", len(expected)) == list(expected), name
            if not options.get("finalizable", False):
                rooted = options.get("rooted", False)
                live = [int(rooted), int(rooted and 1 in expected), int(rooted and 2 in expected)]
                assert column(cache, "live", "B") == live, name
                if rooted:
                    retained = column(cache, "retained", "Q")
                    size = column(cache, "sizes", "I")[0]
                    assert retained[0] == size + 24 * len(set(expected)), (name, retained)
                else:
                    garbage = run("garbage")
                    assert garbage.returncode == 0, (name, garbage.stderr)
                    graph = json.loads(garbage.stdout)
                    assert graph["unreferencedDeadObjects"] == 3 - len(set(expected)), (name, graph)
    print(name + ": passed")


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="dotheap-contract-") as temporary:
        directory = Path(temporary)
        for layout in OFFSETS:
            check(binary, directory, layout, layout=layout, finalizable=True)
            check(binary, directory, layout + "-gc-data", layout=layout, storage="gc-data")
            check(binary, directory, layout + "-gc-data-invalid-descriptor", layout=layout,
                  storage="gc-data", invalid="descriptor")
            check(binary, directory, layout + "-header-being-changed", layout=layout,
                  storage="gc-data", invalid="committed-past-end")
            check(binary, directory, layout + "-free", layout=layout, storage="free")
            check(binary, directory, layout + "-build-a", layout=layout,
                  build="00112233445566778899aabbccddeeff", commit=COMMIT)
            check(binary, directory, layout + "-build-b", layout=layout,
                  build="ffeeddccbbaa99887766554433221100", commit="abcdef0123456789", version="10.0.999.12345")
            check(binary, directory, layout + "-no-version", layout=layout, version=None)
        check(binary, directory, "empty-current", empty=True)
        check(binary, directory, "ambiguous", layout="previous", empty=True, error="layout")
        check(binary, directory, "ambiguous-with-commit", layout="previous", empty=True, commit=COMMIT, error="layout")
        check(binary, directory, "invalid-bounds", invalid="bounds", error="layout")
        check(binary, directory, "invalid-descriptor", invalid="descriptor", error="GC descriptor")
        for rooted in [False, True]:
            suffix = "-rooted" if rooted else "-dead"
            check(binary, directory, "scalar" + suffix, rooted=rooted)
            check(binary, directory, "reference" + suffix, rooted=rooted, refs={8: 1}, expected=[1])
            check(binary, directory, "no-pointers" + suffix, rooted=rooted, series=())
            check(binary, directory, "multiple-series" + suffix, rooted=rooted, base=48,
                  series=((24, 8), (8, 8)), refs={8: 1, 24: 2}, expected=[1, 2])
            check(binary, directory, "reference-array" + suffix, rooted=rooted, base=24,
                  component=8, length=2, series=((16, 0),), refs={16: 1, 24: 2}, expected=[1, 2])
            check(binary, directory, "multidimensional-array" + suffix, rooted=rooted, base=40,
                  component=8, length=2, series=((32, 0),), refs={32: 1, 40: 2}, expected=[1, 2])
            check(binary, directory, "struct-array" + suffix, rooted=rooted, base=24,
                  component=32, length=2, series=(), offset=24, runs=((1, 8), (1, 8)),
                  refs={24: 1, 40: 2, 56: 2, 72: 1}, expected=[1, 2, 2, 1])
        check(binary, directory, "empty-reference-array", base=24, component=8, series=((16, 0),))
        check(binary, directory, "empty-struct-array", base=24, component=16, series=(),
              offset=16, runs=((1, 8),))
        check_cache_identity(binary, directory)


def check_cache_identity(binary, directory):
    cache = directory / "shared-cache"
    for mode in ("raw", "indexed", "blocks"):
        for name, rooted in (("first", True), ("second", False), ("second", True)):
            folder = directory / name
            folder.mkdir(exist_ok=True)
            dump = folder / "same.dmp"
            core(dump, rooted=rooted, refs={8: 1})
            if mode == "blocks":
                with dump.open("r+b") as f:
                    f.truncate(17 << 20)
            if mode != "raw":
                compressed = dump.with_suffix(".dmp.zst")
                subprocess.run(["zstd", "-q", "-f", str(dump), "-o", str(compressed)], check=True)
                dump = compressed
            os.utime(dump, (1700000000, 1700000000))
            result = subprocess.run([binary, "summary", str(dump), "--cache", str(cache), "--json"],
                                    capture_output=True, text=True, timeout=30)
            assert result.returncode == 0, (mode, name, result.stderr)
            assert column(cache, "live", "B") == ([1, 1, 0] if rooted else [0, 0, 0]), (mode, name)
        assert (cache / "core.blocks").exists() == (mode == "blocks"), mode
        print(f"cache-identity-{mode}: passed")


if __name__ == "__main__":
    main()
