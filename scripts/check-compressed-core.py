#!/usr/bin/env python3
"""Reject corrupt compressed memory and recover from a corrupt compressed cache."""
import json
from pathlib import Path
import struct
import subprocess
import sys
import tempfile


def core(size):
    header = bytearray(4096)
    header[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HHI", header, 16, 4, 62, 1)
    struct.pack_into("<Q", header, 32, 64)
    struct.pack_into("<HHH", header, 52, 64, 56, 1)
    struct.pack_into("<IIQQQQQQ", header, 64, 1, 6, 4096, 0x10000, 0, size - 4096, size - 4096, 4096)
    return header + b"s" * (size - 4096)


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="dotheap-compressed-") as temporary:
        folder = Path(temporary)

        def compress(data, name):
            path = folder / name
            path.write_bytes(data)
            return subprocess.check_output(["zstd", "-q", "--check", "-c", str(path)])

        def run(command, dump, cache, *args):
            return subprocess.run([binary, command, str(dump), *args, "--cache", str(cache), "--json"],
                                  capture_output=True, text=True, timeout=30)

        data = core(8192)
        header = compress(data[:4096], "header")
        memory = bytearray(compress(data[4096:], "memory"))
        memory[-1] ^= 1  # Keep the frame header valid; damage its checksum.
        dump, cache = folder / "corrupt.dmp.zst", folder / "corrupt-cache"
        dump.write_bytes(header + memory)
        result = run("memory", dump, cache)
        assert result.returncode == 1 and not result.stdout, result
        assert "decompress" in result.stderr, result.stderr
        assert not (cache / "manifest.json").exists()
        print("compressed-corrupt-frame: passed")

        dump, cache = folder / "valid.dmp.zst", folder / "valid-cache"
        dump.write_bytes(compress(core(17 << 20), "large-core"))
        result = run("memory", dump, cache)
        assert result.returncode == 0, result.stderr
        assert sum(row["nonZero"] for row in json.loads(result.stdout)["byKind"]) == (17 << 20) - 4096
        blocks = cache / "core.blocks"
        with blocks.open("r+b") as f:
            first = f.read(1)
            f.seek(0)
            f.write(bytes([first[0] ^ 1]))
        result = run("bytes", dump, cache, "0x10000", "--count", "8")
        assert result.returncode == 1 and not result.stdout, result
        assert "decompress" in result.stderr, result.stderr
        assert not (cache / "manifest.json").exists()
        assert not (cache / "core.dump").exists()
        print("compressed-corrupt-cache: passed")

        result = run("bytes", dump, cache, "0x10000", "--count", "8")
        assert result.returncode == 0, result.stderr
        assert "73 73 73 73 73 73 73 73" in result.stdout, result.stdout
        print("compressed-cache-recovery: passed")


if __name__ == "__main__":
    main()
