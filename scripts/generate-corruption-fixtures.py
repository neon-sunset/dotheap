#!/usr/bin/env python3
"""Create heap corruption cases and valid controls for the verifier."""
import importlib.util
import json
from pathlib import Path
import struct
import sys

sys.dont_write_bytecode = True


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


contract = module("contract", "check-heap-contract.py")
compat = module("compat", "generate-compatibility-fixtures.py")
folder = Path(sys.argv[1])
folder.mkdir(parents=True, exist_ok=True)
page, image, first, mt = contract.PAGE, contract.IMAGE, contract.FIRST, contract.MT
cases = []
for gc in ["satori", "coreclr"]:
    for arch in [62, 183]:
        modes = ["valid", "tagged", "generic", "canonical-back-missing", "canonical-class-null", "missing-class", "null-class", "bad-backpointer",
                 "fake-mt", "bad-gcdesc", "integer-data", "unaligned-ref", "interior-ref", "outside-ref",
                 "free-ref", "oversized", "unaligned-size", "zero-mt", "thread-cycle"]
        if gc == "coreclr":
            modes += ["segment-cycle", "segment-boundary", "finalization-boundary"]
        if gc == "satori":
            modes += ["string", "array", "array-too-long", "tracker-cycle", "tracker-overfull", "queue-self-link", "queue-cycle", "many-bad-refs", "struct-array"]
            modes += ["huge-stale-tail", "huge-zero-tail", "huge-object-tail", "huge-leading-filler",
                      "huge-too-long", "huge-bad-mt", "huge-bad-bounds"]
        for mode in modes:
            name = f"{gc}-{arch}-{mode}"
            path = folder / (name + ".dmp")
            if gc == "coreclr":
                compat.coreclr(path, arch, "workstation-regions")
            elif mode.startswith("huge-"):
                compat.huge_region(path, mode)
            elif mode == "many-bad-refs":
                contract.core(path, base=24, component=8, length=1300, series=((16, 0),))
            elif mode == "struct-array":
                contract.core(path, base=24, component=24, length=3, series=(), runs=((1, 16),), offset=16, refs={16: 1})
            elif mode == "queue-cycle":
                contract.core(path, storage="gc-data")
            elif mode in ["array", "array-too-long"]:
                contract.core(path, base=24, component=8, length=2, series=((16, 0),))
            elif mode == "string":
                contract.core(path, base=22, component=2, length=3, series=())
            else:
                contract.core(path, rooted=True, refs={8: 1})
            data = bytearray(path.read_bytes())

            def put(address, value, fmt="Q"):
                offset = 4096 + address - image if image <= address < image + 4096 else 8192 + address - page
                struct.pack_into("<" + fmt, data, offset, value)

            struct.pack_into("<H", data, 18, arch)
            if gc == "satori":
                files = struct.pack("<QQQQQ", 1, 4096, image, image + 4096, 0) + b"/fixture/libcoreclr.so\0"
                status = bytearray(336 if arch == 62 else 392)
                struct.pack_into("<I", status, 32, 1)
                struct.pack_into("<Q", status, 112, first)
                notes = contract.note(b"CORE\0", 0x46494c45, files) + contract.note(b"CORE\0", 1, status)
                data[64:120] = contract.segment(4, 512, 0, len(notes))
                data[512:512 + len(notes)] = notes
            types = {"MethodTable": {"EEClassOrCanonMT": 40}, "EEClass": {"MethodTable": 16},
                     "ThreadStore": {"FirstThreadLink": 8}, "Thread": {"LinkNext": 112}}
            descriptor = json.dumps({"types": types, "globals": {"ThreadStore": page + 0x9000}}).encode()
            data[4096 + 0xb00:4096 + 0xb08] = b"DNCCDAC\0"
            put(image + 0xb0c, len(descriptor), "I")
            put(image + 0xb10, image + 0xc00)
            data[4096 + 0xc00:4096 + 0xc00 + len(descriptor)] = descriptor
            put(page + 0x9000, page + 0x9100)
            for index, table in enumerate([mt, page + 0x2200, page + 0x2300]):
                cls = page + 0x6000 + index * 0x100
                put(table + 40, cls)
                put(cls + 16, table)
            if gc == "coreclr":
                for i in range(7):
                    put(page + 0x5000 + i * 8, page + 0x5100 + (8 if i == 6 else 0))
            expected = None
            if mode == "huge-too-long":
                expected = ("Invalid", "Object size", first + 8)
            elif mode == "huge-bad-mt":
                expected = ("MissingMemory", "Object.MethodTable", first)
            elif mode == "huge-bad-bounds":
                expected = ("Invalid", "GC region", contract.REGION)
            elif mode == "integer-data":
                put(first + 16, 0xdeadbeefdeadbee0)
            elif mode == "many-bad-refs":
                for i in range(1300): put(first + 16 + i * 8, 0xdeadbeefdeadbee0)
                expected = ("Invalid", "Managed reference", first + 16)
            elif mode == "tagged":
                put(first, mt | 3)
            elif mode == "generic":
                put(mt, 0x01000010, "I")
                put(mt + 40, (page + 0x8000) | 1)
                put(page + 0x8028, page + 0x6000)
                put(page + 0x6010, page + 0x8000)
            elif mode == "canonical-back-missing":
                put(mt, 0x01000010, "I")
                put(page + 0x6010, 0x123456789000)
                expected = ("MissingMemory", "MethodTable.EEClassOrCanonMT", 0x123456789028)
            elif mode == "canonical-class-null":
                put(mt, 0x01000010, "I")
                put(mt + 40, (page + 0x8000) | 1)
                expected = ("Invalid", "MethodTable.EEClassOrCanonMT", page + 0x8028)
            elif mode == "missing-class":
                put(mt + 40, 0x123456789000)
                expected = ("MissingMemory", "EEClass.MethodTable", 0x123456789010)
            elif mode == "null-class":
                put(mt + 40, 0)
                expected = ("Invalid", "MethodTable.EEClassOrCanonMT", mt + 40)
            elif mode == "bad-backpointer":
                put(page + 0x6010, page + 0x2200)
                expected = ("Invalid", "EEClass.MethodTable", page + 0x6010)
            elif mode == "fake-mt":
                put(first, page + 0x7000)
                put(page + 0x7004, 32, "I")
                expected = ("Invalid", "MethodTable.EEClassOrCanonMT", page + 0x7028)
            elif mode == "bad-gcdesc":
                put(mt - 8, 0)
                expected = ("Invalid", "GC descriptor", mt - 8)
            elif mode.endswith("-ref"):
                if gc == "coreclr" and mode == "free-ref":
                    put(page + 0x1000, 0)
                    put(page + 0x1008, 0)
                pointer = {"unaligned-ref": first + 113, "interior-ref": first + 120,
                           "outside-ref": 0xdeadbeefdeadbee0, "free-ref": first + 32}[mode]
                put(first + 8, pointer)
                expected = ("Invalid", "Managed reference", first + 8)
            elif mode in ["oversized", "unaligned-size"]:
                put(mt + 4, 65536 if mode == "oversized" else 25, "I")
                expected = ("Invalid", "Object size", mt + 4)
            elif mode == "array-too-long":
                put(first + 8, 1024, "I")
                expected = ("Invalid", "Object size", first + 8)
            elif mode == "zero-mt":
                put(first, 0)
                expected = ("Unverified", "Object.MethodTable", first)
            elif mode == "thread-cycle":
                put(page + 0x9108, page + 0x9270)
                put(page + 0x9270, page + 0x9270)
                expected = ("Invalid", "Thread.LinkNext", page + 0x9270)
            elif mode == "tracker-cycle":
                put(contract.REGION + 120, page + 0x7000)
                put(page + 0x7008, page + 0x7000)
                expected = ("Invalid", "Satori finalization trackers", page + 0x7008)
            elif mode == "tracker-overfull":
                put(contract.REGION + 120, page + 0x7000)
                put(page + 0x7000, 511)
                expected = ("Invalid", "Satori finalization tracker count", page + 0x7000)
            elif mode == "segment-cycle":
                put(page + 0x1830, page + 0x1800)
                expected = ("Invalid", "CoreCLR segment next", page + 0x1830)
            elif mode == "segment-boundary":
                put(page + 0x1800, page + 0x50000)
                expected = ("Invalid", "CoreCLR segment allocated", page + 0x1800)
            elif mode == "finalization-boundary":
                put(page + 0x5000, page + 0x5101)
                expected = ("Invalid", "CoreCLR finalization queue", page + 0x5000)
            elif mode == "queue-cycle":
                r1, r2 = contract.REGION, contract.REGION + (1 << 21)
                for a, b in [(r1, r2), (r2, r1)]:
                    put(a + 144, b)
                    put(a + 152, b)
                    put(a + 160, page + 0x3000)
                expected = ("Invalid", "Satori region next", r2 + 152)
            elif mode == "queue-self-link":
                put(contract.REGION + 152, contract.REGION)
                expected = ("Invalid", "Satori region next", contract.REGION + 152)
            path.write_bytes(data)
            cases.append(dict(name=name, expected=expected, objects=1 if mode.startswith("huge-") else None))
(folder / "expected.json").write_text(json.dumps(cases, indent=2) + "\n")
