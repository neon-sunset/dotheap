use heap_core::{
    column::ColumnStore,
    core_dump::{CoreDump, FileMapping},
    heap::ManagedHeap,
    native_memory::{HEAP_MAX, NativeMemory},
};
use std::sync::Arc;

fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn dump(bytes: &[u8], address: u64, machine: u16) -> (tempfile::TempDir, CoreDump) {
    let mut core = vec![0; 4096 + bytes.len()];
    core[..6].copy_from_slice(b"\x7fELF\x02\x01");
    for (offset, value) in [(16, 4u16), (18, machine), (52, 64), (54, 56), (56, 1)] {
        core[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [(20, 1u32), (64, 1), (68, 6)] {
        core[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (32, 64),
        (72, 4096),
        (80, address),
        (96, bytes.len() as u64),
        (104, bytes.len() as u64),
        (112, 4096),
    ] {
        put64(&mut core, offset, value);
    }
    core[4096..].copy_from_slice(bytes);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("heap.dmp");
    std::fs::write(&path, core).unwrap();
    (directory, CoreDump::open(&path, None, None).unwrap())
}

fn native(mut dump: CoreDump, main: bool) -> NativeMemory {
    if main {
        let base = dump.segments[0].address;
        dump.layout.files.push(FileMapping {
            start: base - 0x20000,
            end: base - 0x10000,
            file_offset: 0,
            path: "/demo".into(),
            deleted: false,
        });
    }
    let heap = ManagedHeap::new(Arc::new(dump), None, ColumnStore::default()).unwrap();
    NativeMemory::new(&heap.dump, &heap, &[])
}

#[test]
fn allocation_data_at_an_arena_boundary_does_not_split_the_main_heap() {
    let mut bytes = vec![0; 4 << 20];
    let base = HEAP_MAX - 0x10000;
    put64(&mut bytes, 8, 0x20001);
    let mut at = 0x20000;
    for _ in 0..400 {
        put64(&mut bytes, at + 8, 8209);
        bytes[at + 16..at + 8208].fill(0x5a);
        at += 8208;
    }
    let top_size = bytes.len() - at;
    put64(&mut bytes, at + 8, top_size as u64 | 1);
    for (offset, value) in [
        (0, 0xffffe89ef160),
        (8, HEAP_MAX + 32),
        (16, 1),
        (24, 1),
        (32, base + 0x1000),
    ] {
        put64(&mut bytes, 0x10000 + offset, value);
    }
    for machine in [62, 183] {
        let (_directory, dump) = dump(&bytes, base, machine);
        assert_eq!(dump.segments.len(), 1);
        let memory = native(dump, true);
        assert_eq!(memory.malloc_heaps.len(), 1);
        let heap = &memory.malloc_heaps[0];
        assert!(heap.main_arena && heap.walk_complete);
        assert_eq!(heap.in_use_chunks, 401);
        assert_eq!(heap.in_use_bytes, 0x20000 + 400 * 8208);
        assert_eq!(heap.top_bytes, top_size as u64);
    }
}

#[test]
fn arena_heap_sizes_must_cover_whole_pages() {
    let base = HEAP_MAX - 0x10000;
    for (size, protected, valid) in [
        (1, 1, false),
        (0x1001, 0x2000, false),
        (0x1000, 0x1001, false),
        (0x2000, 0x1000, false),
        (0x1000, HEAP_MAX + 4096, false),
        (0x1000, 0x2000, true),
        (0x10000, 0x20000, true),
    ] {
        let mut bytes = vec![0; 2 << 20];
        for (offset, value) in [(0, HEAP_MAX + 48), (16, size), (24, protected), (32, 4096)] {
            put64(&mut bytes, 0x10000 + offset, value);
        }
        let (_directory, dump) = dump(&bytes, base, 183);
        let ranges: Vec<_> = dump.segments.iter().map(|s| (s.address, s.end())).collect();
        if valid {
            assert_eq!(
                ranges,
                [
                    (base, HEAP_MAX),
                    (HEAP_MAX, HEAP_MAX + protected),
                    (HEAP_MAX + protected, base + bytes.len() as u64),
                ]
            );
        } else {
            assert_eq!(ranges, [(base, base + bytes.len() as u64)]);
        }
        let memory = native(dump, false);
        assert_eq!(memory.malloc_heaps.len(), usize::from(valid));
    }
}

#[test]
fn main_heap_detection_accepts_a_free_predecessor_with_a_matching_footer() {
    for (flags, footer, valid) in [
        (0, 64, true),
        (0, 32, false),
        (2, 64, false),
        (4, 64, false),
    ] {
        let mut bytes = vec![0; 4096];
        for i in 0..8 {
            put64(&mut bytes, i * 64 + 8, 65);
        }
        put64(&mut bytes, 2 * 64, footer);
        put64(&mut bytes, 2 * 64 + 8, 64 | flags);
        put64(&mut bytes, 8 * 64 + 8, (4096 - 8 * 64) | 1);
        let (_directory, dump) = dump(&bytes, HEAP_MAX - 0x10000, 62);
        let memory = native(dump, true);
        assert_eq!(memory.malloc_heaps.len(), usize::from(valid));
        if valid {
            let heap = &memory.malloc_heaps[0];
            assert!(heap.main_arena && heap.walk_complete);
            assert_eq!(heap.in_use_chunks, 7);
            assert_eq!(heap.free_chunks, 1);
            assert_eq!(heap.free_bytes, 64);
        }
    }
}
