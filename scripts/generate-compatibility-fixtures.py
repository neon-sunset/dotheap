#!/usr/bin/env python3
"""Write synthetic heaps and their independently specified analysis results."""
import importlib.util
import json
from pathlib import Path
import sys
import struct

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("heap_contract", Path(__file__).with_name("check-heap-contract.py"))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def gc_storage(path, layout, kind):
    contract.core(path, layout=layout, storage="gc-data")
    data = bytearray(path.read_bytes())
    page, region, first = contract.PAGE, contract.REGION, contract.FIRST
    used, committed, _, _, _, queue, _, _, _ = contract.OFFSETS[layout]

    def write(address, value, fmt="Q"):
        struct.pack_into("<" + fmt, data, 8192 + address - page, value)

    heap, pool = page + 0x10000, page + 0x12000
    write(page + 32, heap)
    write(heap, heap)
    for i in range(9):
        q = page + 0x3000 if i == 0 else page + 0x11000 + i * 128
        write(heap + 8 + i * 8, q)
        write(q, 0, "I")
    write(page + 0x3000 + 64, region + (1 << 21))
    write(page + 0x3000 + 72, region + (1 << 21))
    write(page + 0x3000 + 80, 1)
    write(region + queue, 0)
    write(region + used, region + (1 << 21))
    write(region + committed, region + (1 << 21))
    if kind == "finalization-storage":
        size, q = 8192 * 16, heap + 0x300
        write(region + 64, first + size)
        write(q + 128, 8191)
        write(q + 144, first)
        write(q + 152, heap)
    else:
        write(region + 64, first + 502 * 4096)
        write(heap + 80, pool)
        write(pool, 0 if kind == "unrooted-work-chunks" else first)
        for i in range(502):
            chunk = first + i * 4096
            write(chunk, 0)
            write(chunk + 8, chunk + 4096 if i < 501 else 0)
    path.write_bytes(data)


def sharded_gc_storage(folder):
    folder.mkdir(parents=True, exist_ok=True)
    cases = [(1, 0, "valid")] + [(4, i, "valid") for i in range(4)]
    cases += [(4, 3, kind) for kind in (
        "empty", "bad-mask", "overflow-mask", "bad-queue-mask", "bad-queue-kind", "truncated", "bad-head")]
    for architecture, alignment in ((62, 64), (183, 128)):
        for count, occupied, kind in cases:
            path = folder / f"{architecture}-{count}-{occupied}-{kind}.dmp"
            gc_storage(path, "current", "work-chunks")
            data = bytearray(path.read_bytes())
            page, first = contract.PAGE, contract.FIRST

            def write(address, value, fmt="Q"):
                struct.pack_into("<" + fmt, data, 8192 + address - page, value)

            struct.pack_into("<H", data, 18, architecture)
            heap, pool = page + 0x10000, page + 0x12000
            for i in range(9):
                queue = page + 0x15000 + i * 0x1000
                shards = count if i == 0 else 1
                write(heap + 8 + i * 8, queue)
                write(queue, shards - 1)
                if i == 0:
                    shard = queue + alignment + occupied * 2 * alignment
                    free = contract.REGION + (1 << 21)
                    write(shard + alignment, free)
                    write(shard + alignment + 8, free)
                    write(shard + alignment + 16, 1)
                    write(free + contract.OFFSETS["current"][5], shard)
            write(pool, count - 1)
            write(pool + 128 + occupied * 128, 0 if kind == "empty" else first)
            if kind == "bad-mask":
                write(pool, 2)
            elif kind == "overflow-mask":
                write(pool, (1 << 64) - 1)
            elif kind == "bad-queue-mask":
                write(page + 0x16000, 2)
            elif kind == "bad-queue-kind":
                write(page + 0x15000 + alignment + 2 * alignment, 1, "I")
            elif kind == "truncated":
                write(heap + 80, page + 0x40ff80)
                write(page + 0x40ff80, count - 1)
            elif kind == "bad-head":
                write(pool + 128 + occupied * 128, first + 8)
            path.write_bytes(data)


def huge_region(path, mode):
    contract.core(path, base=24, component=2, length=8, series=())
    data = bytearray(path.read_bytes())
    data.extend(bytes(8192 + 0x600000 - len(data)))
    data[176:232] = contract.segment(1, 8192, contract.PAGE, len(data) - 8192)

    def put(address, value, fmt="Q"):
        struct.pack_into("<" + fmt, data, 8192 + address - contract.PAGE, value)

    filler = 24 if mode == "huge-leading-filler" else 0
    size = (1 << 21) - 36888 - 16 if filler else (1 << 21) + 24
    obj = contract.FIRST + filler
    end = obj + size
    used = end + 920088
    committed = (used + 4095) & ~4095
    put(contract.REGION + 24, contract.REGION + (1 << 22))
    put(contract.REGION + 88, used)
    put(contract.REGION + 96, committed)
    put(contract.REGION + 168, size)
    put(contract.REGION + 220, 1, "I")
    if filler:
        put(contract.FIRST, contract.PAGE + 0x2100)
        put(contract.FIRST + 8, 0, "I")
    put(obj, contract.MT)
    put(obj + 8, (size - 24) // 2, "I")
    if mode == "huge-object-tail":
        put(end, contract.PAGE + 0x2200)
    elif mode != "huge-zero-tail":
        put(end, contract.PAGE + 0x7000)
        put(contract.PAGE + 0x7004, 31749, "I")
    if mode == "huge-too-long":
        put(obj + 8, 1 << 23, "I")
    elif mode == "huge-bad-mt":
        put(obj, 0x123456789000)
    elif mode == "huge-bad-bounds":
        put(contract.REGION + 88, committed + 8)
        # An intact second region lets layout detection resolve the damaged header.
        data.extend(bytes(1 << 21))
        data[176:232] = contract.segment(1, 8192, contract.PAGE, len(data) - 8192)
        data[8192 + 0x403] = 1
        other = contract.REGION + (1 << 22)
        put(other + 16, 2, "i")
        put(other + 24, other + (1 << 21))
        put(other + 32, contract.PAGE)
        put(other + 88, other + 36880)
        put(other + 96, other + 0x10000)
    path.write_bytes(data)


def native_allocations(path):
    contract.core(path, refs={8: 1}, rooted=True)
    data = bytearray(path.read_bytes())
    sizes = [4096, 8192, 4096, 8192]
    base, at = 0x700004000000, 48
    memory = bytearray(32768)
    struct.pack_into("<QQQQ", memory, 0, 0x710000000000, 0, len(memory), len(memory))
    struct.pack_into("<H", data, 56, 4)
    data[232:288] = contract.segment(1, len(data), base, len(memory))
    struct.pack_into("<Q", data, 4096 + 0x900, base + at + 16)
    for i, size in enumerate(sizes):
        struct.pack_into("<Q", memory, at + 8, size | 5)
        if i < 2:
            struct.pack_into("<Q", memory, at + 16, base + at + size + 16)
        at += size
    struct.pack_into("<Q", memory, at + 8, (len(memory) - at) | 1)
    data.extend(memory)
    path.write_bytes(data)


def static_fields(path, mode):
    contract.core(path, base=24, component=8, length=4, series=((16, 0),),
                  refs={16: 1, 24: 2, 32: 1, 40: 2}, rooted=True)
    data = bytearray(path.read_bytes())
    page, image = contract.PAGE, contract.IMAGE

    def put(address, value, fmt="Q"):
        offset = 4096 + address - image if image <= address < image + 4096 else 8192 + address - page
        struct.pack_into("<" + fmt, data, offset, value)

    if mode == "owned":
        owner, target = contract.FIRST + 136, contract.FIRST + 160
        root = data.index(struct.pack("<Q", contract.FIRST), 512, 4096)
        struct.pack_into("<Q", data, root, owner)
        leaf = page + 0x2200
        put(leaf, 0x01000000, "I")
        put(leaf - 8, 1)
        put(leaf - 16, 8)
        put(leaf - 24, -16, "q")
        put(owner + 8, contract.FIRST)
        for offset in (16, 24, 32, 40):
            put(contract.FIRST + offset, target)

    types = {
        "DynamicStaticsInfo": {"!": 24, "GCStatics": 0},
        "MethodTable": {"AuxiliaryData": 32},
        "Module": {"TypeDefToMethodTableMap": 64},
        "ModuleLookupMap": {"Next": 0, "TableData": 8, "Count": 16, "SupportedFlagsMask": 24},
        "EEClass": {"FieldDescList": 24, "NumStaticFields": 52},
        "FieldDesc": {"!": 16},
    }
    descriptor = json.dumps({"types": types, "globals": {}}).encode()
    data[4096 + 0x400:4096 + 0x408] = b"DNCCDAC\0"
    put(image + 0x40c, len(descriptor), "I")
    put(image + 0x410, image + 0x800)
    data[4096 + 0x800:4096 + 0x800 + len(descriptor)] = descriptor
    put(contract.MT, 0x810a0008, "I")
    put(contract.MT + 24, page + 0x5000)
    put(contract.MT + 48, page + 0x2200)
    put(page + 0x5048, page + 0x5100)
    put(page + 0x5050, 2 if mode == "types" else 1, "I")
    for i in range(2 if mode == "types" else 1):
        mt, aux = page + 0x6000 + i * 0x200, page + 0x7000 + i * 0x100
        put(page + 0x5100 + i * 8, mt)
        put(mt + 8, 2 | ((i + 1) << 8), "I")
        put(mt + 24, page + 0x5000)
        put(mt + 32, aux)
        put(mt + 40, mt + 0x80)
        put(mt + 0x98, mt + 0xc0)
        fields = 4 if mode in ("fields", "owned") else 2
        put(mt + 0xb4, fields, "H")
        put(aux - 24, (contract.FIRST + 16 + i * 16) | 1)
        for field in range(fields):
            put(mt + 0xc8 + field * 16, (1 << 24) | (field + 1), "I")
            put(mt + 0xcc + field * 16, (0x12 << 27) | (field * 8), "I")
    path.write_bytes(data)


def generate(folder):
    folder.mkdir(parents=True, exist_ok=True)
    cases = []

    def add(name, expected=(), error=None, **options):
        contract.core(folder / (name + ".dmp"), **options)
        empty = bool(options.get("empty") or options.get("storage"))
        cases.append({"name": name, "layout": contract.LAYOUTS[options.get("layout", "current")],
                      "error": error, "objects": 0 if empty else 3, "references": list(expected),
                      "rooted": options.get("rooted", False), "finalizable": options.get("finalizable", False),
                      "storage": options.get("storage")})

    for layout in contract.OFFSETS:
        for kind in ("work-chunks", "unrooted-work-chunks", "finalization-storage"):
            gc_storage(folder / f"{layout}-{kind}.dmp", layout, kind)
        add(layout, layout=layout, finalizable=True)
        for storage in ("gc-data", "free"):
            add(layout + "-" + storage, layout=layout, storage=storage)
        add(layout + "-gc-data-invalid-descriptor", layout=layout, storage="gc-data", invalid="descriptor")
        add(layout + "-changing-header", layout=layout, storage="gc-data", invalid="committed-past-end")
        add(layout + "-irrelevant-build-id", layout=layout, build="00112233445566778899aabbccddeeff", commit=contract.COMMIT)
        add(layout + "-no-version", layout=layout, version=None)
    add("empty-current", empty=True)
    add("ambiguous", layout="previous", empty=True, error="layout")
    add("ambiguous-with-commit", layout="previous", empty=True, commit=contract.COMMIT, error="layout")
    add("invalid-bounds", invalid="bounds", error="layout")
    add("invalid-descriptor", invalid="descriptor", error="GC descriptor")
    for rooted in (False, True):
        suffix = "-rooted" if rooted else "-dead"
        add("scalar" + suffix, rooted=rooted)
        add("reference" + suffix, rooted=rooted, refs={8: 1}, expected=[1])
        add("no-pointers" + suffix, rooted=rooted, series=())
        add("multiple-series" + suffix, rooted=rooted, base=48, series=((24, 8), (8, 8)), refs={8: 1, 24: 2}, expected=[1, 2])
        add("reference-array" + suffix, rooted=rooted, base=24, component=8, length=2, series=((16, 0),), refs={16: 1, 24: 2}, expected=[1, 2])
        add("multidimensional-array" + suffix, rooted=rooted, base=40, component=8, length=2, series=((32, 0),), refs={32: 1, 40: 2}, expected=[1, 2])
        add("struct-array" + suffix, rooted=rooted, base=24, component=32, length=2, series=(), offset=24, runs=((1, 8), (1, 8)), refs={24: 1, 40: 2, 56: 2, 72: 1}, expected=[1, 2, 2, 1])
    add("empty-reference-array", base=24, component=8, series=((16, 0),))
    add("empty-struct-array", base=24, component=16, series=(), offset=16, runs=((1, 8),))
    (folder / "expected.json").write_text(json.dumps(cases, indent=2) + "\n")

    for mode in ("fields", "types", "unnamed", "owned"):
        static_fields(folder / f"static-shared-{mode}.dmp", mode)

    references = bytearray((folder / "reference-array-rooted.dmp").read_bytes())
    method_table = 8192 + contract.MT - contract.PAGE
    struct.pack_into("<I", references, method_table, 0x810A0008)
    struct.pack_into("<Q", references, method_table + 48, contract.PAGE + 0x2200)
    (folder / "query-reference-array.dmp").write_bytes(references)

    path = folder / "query-zero-array.dmp"
    contract.core(path, base=24, component=1, length=1024, series=(), rooted=True)
    data = bytearray(path.read_bytes())
    struct.pack_into("<I", data, 8192 + contract.MT - contract.PAGE, 0x800A0001)
    start = 8192 + contract.FIRST - contract.PAGE + 16
    data[start:start + 1024] = bytes(1 if i % 16 == 0 else 0 for i in range(1024))
    for i in range(8):
        at = 8192 + 0x6000 + i * 64
        struct.pack_into("<II", data, at, 0x800A0008, 24)
        struct.pack_into("<Q", data, at + 48, contract.PAGE + (0x6000 + (i + 1) * 64 if i < 7 else 0x2100))
    at = 8192 + 0x7000
    struct.pack_into("<II", data, at, 0x800A0008, 24)
    struct.pack_into("<Q", data, at + 48, contract.PAGE + 0x7000)
    generic = 8192 + 0x8000
    struct.pack_into("<II", data, generic, 0x30, 24)
    struct.pack_into("<Q", data, generic + 48, contract.PAGE + 0x8108)
    struct.pack_into("<I", data, 8192 + 0x8104, (32 << 16) | 1)
    struct.pack_into("<Q", data, 8192 + 0x8108, contract.PAGE + 0x8200)
    for i in range(32):
        struct.pack_into("<Q", data, 8192 + 0x8200 + i * 8, contract.PAGE + 0x9000 + i * 64)
    path.write_bytes(data)

    contract.core(folder / "native-pointer-fields.dmp", base=96, series=(), rooted=True)
    contract.core(folder / "native-pointer-array.dmp", base=24, component=40, length=2, series=(), rooted=True)
    contract.core(folder / "native-pointer-elements.dmp", base=24, component=8, length=2, series=(), rooted=True)
    contract.core(folder / "native-pointer-multidimensional.dmp", base=40, component=8, length=2, series=(), rooted=True)
    native_allocations(folder / "native-allocations.dmp")

    for architecture in (62, 183):
        for mode in ("workstation-regions", "workstation-segments", "server-regions"):
            coreclr(folder / f"coreclr-{architecture}-{mode}.dmp", architecture, mode)
        for mode in ("regular", "large", "shared", "bad-backpointer", "missing-descriptor",
                     "unknown-thread", "null-locals", "overflow-locals", "ambiguous"):
            satori_thread(folder / f"satori-thread-{architecture}-{mode}.dmp", architecture, mode)


def satori_thread(path, architecture, mode):
    contract.core(path, refs={8: 1})
    data = bytearray(path.read_bytes())
    page, image, region = contract.PAGE, contract.IMAGE, contract.REGION

    def put(address, value, fmt="Q"):
        offset = 4096 + address - image if image <= address < image + 4096 else 8192 + address - page
        struct.pack_into("<" + fmt, data, offset, value)

    types = {
        "ThreadStore": {"FirstThreadLink": 8},
        "Thread": {"RuntimeThreadLocals": 32, "LinkNext": 112, "OSId": 296},
        "RuntimeThreadLocals": {"AllocContext": 0},
        "EEAllocContext": {"GCAllocationContext": 8},
    }
    descriptor = json.dumps({"types": types, "globals": {"ThreadStore": page + 0x5000}}).encode()
    if mode != "missing-descriptor":
        data[4096 + 0x400:4096 + 0x408] = b"DNCCDAC\0"
        put(image + 0x40c, len(descriptor), "I")
        put(image + 0x410, image + 0x800)
        data[4096 + 0x800:4096 + 0x800 + len(descriptor)] = descriptor
    thread, locals_address = page + 0x7000, page + 0xa000
    put(page + 0x5000, page + 0x6000)
    put(page + 0x6008, thread + 112)
    put(thread + 296, 4002 if mode == "unknown-thread" else 4001)
    put(thread + 32, 0 if mode == "null-locals" else (1 << 64) - 4 if mode == "overflow-locals" else locals_address)
    slot = locals_address + 8 + (40 if mode == "large" else 32)
    if mode == "shared":
        slot = page + 0xb000
    put(region + 48, slot)
    put(slot, region + 8 if mode == "bad-backpointer" else region)
    put(region + 16, 0, "i")
    put(region + contract.OFFSETS["current"][5], 0)
    tids = [4001]
    if mode == "ambiguous":
        other = page + 0x8000
        put(thread + 112, other + 112)
        put(other + 32, locals_address)
        put(other + 296, 4003)
        tids.append(4003)
    files = struct.pack("<QQQQQ", 1, 4096, image, image + 4096, 0) + b"/fixture/libcoreclr.so\0"
    notes = contract.note(b"CORE\0", 0x46494c45, files)
    for tid in tids:
        status = bytearray(336 if architecture == 62 else 392)
        struct.pack_into("<I", status, 32, tid)
        struct.pack_into("<Q", status, 112, contract.FIRST)
        notes += contract.note(b"CORE\0", 1, status)
    struct.pack_into("<H", data, 18, architecture)
    data[64:120] = contract.segment(4, 512, 0, len(notes))
    data[512:512 + len(notes)] = notes
    path.write_bytes(data)


def coreclr(path, architecture, mode):
    contract.core(path, rooted=True, refs={8: 1})
    data = bytearray(path.read_bytes())
    page, image, first = contract.PAGE, contract.IMAGE, contract.FIRST

    def put(address, value, fmt="Q"):
        offset = 4096 + address - image if image <= address < image + 4096 else 8192 + address - page
        struct.pack_into("<" + fmt, data, offset, value)

    put(page + 8, 0)
    put(page + 24, 0)
    dac = image + 0x400
    put(dac, 0x0402)
    put(dac + 8, 64)
    put(dac + 16, 5)

    def variable(index, address):
        put(dac + 24 + index * 8, address)

    regions = mode.endswith("regions")
    server = mode.startswith("server")
    variable(0, image + 0x800)
    variable(1, image + 0x801)
    put(image + 0x800, int(regions), "B")
    put(image + 0x801, int(server), "B")
    table, segment = page + 0x1000, page + 0x1800
    variable(3, table)
    variable(28, page + 0x3000)
    for i, value in enumerate((0, 16, 24)):
        put(page + 0x3000 + i * 4, value, "i")
    put(table + 2 * 64 + 16, segment)
    put(segment, first + 160)
    put(segment + 8, contract.REGION + 0x10000)
    put(segment + 16, contract.REGION + 0x10000)
    put(segment + 32, first)
    # The allocation context covers the free gap between the first and second objects.
    put(table, first + 32)
    put(table + 8, first + 88)
    if not regions:
        variable(7, page + 0x3100)
        variable(12, page + 0x3108)
        put(page + 0x3100, segment)
        put(page + 0x3108, first + 160)
        put(table + 24, first + 136)
        put(table + 64 + 24, first + 112)

    finalization, slots = page + 0x5000, page + 0x5100
    variable(15, page + 0x3110)
    put(page + 0x3110, finalization)
    put(finalization + 4 * 8, slots)
    put(finalization + 6 * 8, slots + 8)
    put(finalization + 7 * 8, slots)
    put(slots, first + 136)
    put(page + 0x2200, 0x00100000, "I")

    frozen = page + 0x10000
    frozen_segment = page + 0x1900
    put(table + 4 * 64 + 16, frozen_segment)
    put(frozen_segment, frozen + 24)
    put(frozen_segment + 8, frozen + 4096)
    put(frozen_segment + 16, frozen + 4096)
    put(frozen_segment + 32, frozen)
    put(frozen_segment + 40, 1)
    put(frozen, page + 0x2300)
    put(page + 0x2304, 24, "I")

    if server:
        variable(19, page + 0x3200)
        variable(20, page + 0x3208)
        variable(27, page + 0x3300)
        put(page + 0x3200, 2, "I")
        put(page + 0x3208, page + 0x3210)
        for i in range(22):
            put(page + 0x3300 + i * 4, -1, "i")
        for i, value in ((0, 0), (1, 8), (2, 16), (18, 64), (21, 400)):
            put(page + 0x3300 + i * 4, value, "i")
        for i in range(2):
            heap = page + 0x4000 + i * 0x400
            put(page + 0x3210 + i * 8, heap)
            if i == 0:
                start = 8192 + table - page
                data[8192 + heap + 64 - page:8192 + heap + 64 - page + 320] = data[start:start + 320]
                put(heap + 16, finalization)
            else:
                extra, extra_segment = page + 0x12000, page + 0x1a00
                put(heap + 64 + 2 * 64 + 16, extra_segment)
                put(extra_segment, extra + 24)
                put(extra_segment + 8, extra + 4096)
                put(extra_segment + 16, extra + 4096)
                put(extra_segment + 32, extra)
                put(extra, page + 0x2300)
        put(dac + 24 + 35 * 8 + 16, image + 0x808)
        put(image + 0x808, 1, "I")

    struct.pack_into("<H", data, 18, architecture)
    files = struct.pack("<QQQQQ", 1, 4096, image, image + 4096, 0) + b"/fixture/libcoreclr.so\0"
    status = bytearray(336 if architecture == 62 else 392)
    struct.pack_into("<I", status, 32, 1)
    struct.pack_into("<Q", status, 112, first)
    notes = contract.note(b"CORE\0", 0x46494c45, files) + contract.note(b"CORE\0", 1, status)
    data[64:120] = contract.segment(4, 512, 0, len(notes))
    data[512:512 + len(notes)] = notes
    path.write_bytes(data)


if __name__ == "__main__":
    if "--sharded-gc-storage" in sys.argv[2:]:
        sharded_gc_storage(Path(sys.argv[1]))
    else:
        generate(Path(sys.argv[1]))
