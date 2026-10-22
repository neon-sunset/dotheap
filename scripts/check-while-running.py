#!/usr/bin/env python3
"""Checks that a dump of the demo app written while it runs (DEMO_WRITES=1, `dotheap dump` on Linux) holds the
memory of one moment. The demo's writer thread fills an array with the round number over and over, from the start to
the end, so in memory of one moment the numbers are round r + 1 up to some place and round r after it. A dump that
mixes pages copied at different times has more than one such place, or numbers that differ by more than one.

usage: scripts/check-while-running.py <dump.zst>
"""
import array, mmap, struct, subprocess, sys, tempfile

MARKER = struct.pack("<q", 0x2E70_6165_6874_6F64)

with tempfile.TemporaryFile() as core:
    subprocess.run(["zstd", "-dc", sys.argv[1]], stdout=core, check=True)
    core.flush()
    m = mmap.mmap(core.fileno(), 0, access=mmap.ACCESS_READ)
    phoff, = struct.unpack_from("<Q", m, 0x20)
    phnum, = struct.unpack_from("<H", m, 0x38)
    loads = []
    for i in range(phnum):
        kind, _, offset, vaddr, _, filesz = struct.unpack_from("<IIQQQQ", m, phoff + i * 56)
        if kind == 1:
            loads.append((vaddr, filesz, offset))

    def read(address, count):
        out = bytearray()
        while len(out) < count:
            for vaddr, size, offset in loads:
                if vaddr <= address < vaddr + size:
                    n = min(count - len(out), vaddr + size - address)
                    out += m[offset + address - vaddr:offset + address - vaddr + n]
                    address += n
                    break
            else:
                sys.exit(f"0x{address:x} is not in the dump")
        return bytes(out)

    # The array: the marker word, then its length, in an 8-byte aligned place of some segment.
    found = None
    for vaddr, size, offset in loads:
        at = m.find(MARKER, offset, offset + size)
        while at >= 0 and found is None:
            if (at - offset) % 8 == 0:
                length, = struct.unpack_from("<q", m, at + 8)
                if 2 < length < 1 << 30:
                    found = (vaddr + at - offset, length)
            at = m.find(MARKER, at + 1, offset + size)
        if found:
            break
    if not found:
        sys.exit("the demo's array is not in the dump (DEMO_WRITES=1 not set?)")
    address, length = found
    values = array.array("q", read(address + 16, (length - 2) * 8))
    high, low = max(values), min(values)
    if high - low > 1:
        sys.exit(f"FAIL  numbers from round {low} to round {high}: pages of different moments")
    place = values.index(low) if high != low else len(values)
    if high != low and max(values[place:]) != low:
        sys.exit(f"FAIL  round {high} after round {low}: pages of different moments")
    print(f"ok    {len(values)} numbers of one moment: round {high} up to element {place + 2}, round {low} after it")
