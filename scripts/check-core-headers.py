#!/usr/bin/env python3
"""Check that malformed ELF and Mach-O headers produce errors, not crashes."""
from pathlib import Path
import struct
import subprocess
import sys
import tempfile


def elf(kind=1, payload=b"", address=0x10000):
    data = bytearray(120) + payload
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HHI", data, 16, 4, 62, 1)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HHH", data, 52, 64, 56, 1)
    struct.pack_into("<IIQQQQQQ", data, 64, kind, 4, 120, address, 0, len(payload), len(payload), 4096)
    return data


def changed(data, offset, fmt, value):
    result = bytearray(data)
    struct.pack_into(fmt, result, offset, value)
    return result


def note(kind, desc):
    return struct.pack("<III", 5, len(desc), kind) + b"CORE\0\0\0\0" + desc


def main():
    binary = str(Path(sys.argv[1]).resolve())
    load = elf(payload=bytes(4096))
    mapped = struct.pack("<QQQQQ", 1, 4096, 0x10000, 0x11000, 0)
    mach = bytearray(32 + 72)
    struct.pack_into("<IIIIIIII", mach, 0, 0xfeedfacf, 0x100000c, 0, 4, 1, 72, 0, 0)
    struct.pack_into("<II", mach, 32, 0x19, 72)
    cases = {
        "elf-program-offset": changed(load, 32, "Q", 2**64 - 1),
        "elf-program-size": changed(load, 54, "H", 8),
        "elf-program-count": changed(load, 56, "H", 65534),
        "elf-load-offset": changed(load, 72, "Q", 2**64 - 1),
        "elf-load-size": changed(load, 96, "Q", 2**64 - 1),
        "elf-memory-size": changed(load, 104, "Q", 1),
        "elf-address-overflow": changed(load, 80, "Q", 2**64 - 1),
        "elf-byte-order": changed(load, 5, "B", 2),
        "elf-executable": changed(load, 16, "H", 2),
        "elf-note-size": elf(4, struct.pack("<III", 5, 4096, 1)),
        "elf-short-registers": elf(4, note(1, bytes(16))),
        "elf-file-count": elf(4, note(0x46494c45, struct.pack("<QQ", 2**64 - 1, 4096))),
        "elf-file-path": elf(4, note(0x46494c45, mapped + b"no-terminator")),
        "elf-file-page-size": elf(4, note(0x46494c45, changed(mapped, 8, "Q", 0) + b"file\0")),
        "elf-file-offset": elf(4, note(0x46494c45, changed(mapped, 32, "Q", 2**64 - 1) + b"file\0")),
        "macho-command-table": changed(mach, 20, "I", 2**32 - 1),
        "macho-command-count": changed(mach, 16, "I", 2**32 - 1),
        "macho-command-size": changed(mach, 36, "I", 4),
        "macho-short-segment": changed(mach, 36, "I", 8),
        "macho-file-size": changed(mach, 80, "Q", 2**64 - 1),
    }
    with tempfile.TemporaryDirectory(prefix="dotheap-headers-") as folder:
        for name, data in cases.items():
            path = Path(folder) / (name + ".dmp")
            path.write_bytes(data)
            result = subprocess.run([binary, "summary", str(path), "--no-cache"],
                                    capture_output=True, text=True, timeout=15)
            assert result.returncode == 1, (name, result.returncode, result.stderr)
            assert result.stderr.strip(), (name, "missing error")
            print(f"{name}: passed")


if __name__ == "__main__":
    main()
