use heap_core::{
    compressed::{BLOCK_SIZE, Source},
    core_dump::{CoreDump, FileMapping, Layout, Platform, Segment},
    runtime::RuntimeInfo,
};
use std::{collections::HashSet, fs};

#[test]
fn runtime_signatures_and_words_cross_unaligned_compressed_frames() {
    let directory = tempfile::tempdir().unwrap();
    let mut bytes = vec![0; BLOCK_SIZE * 3];
    let base = 0x100000;
    let version = b"@(#)Version 10.0.99-test\0";
    let version_at = BLOCK_SIZE - 5;
    bytes[version_at..version_at + version.len()].copy_from_slice(version);
    let vars_at = BLOCK_SIZE * 2 - 32;
    for (offset, value) in [
        (0, 0x102u64),
        (8, 512),
        (16, 5),
        (24, base + 128),
        (32, base + 136),
    ] {
        bytes[vars_at + offset..vars_at + offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    for compressed in [false, true] {
        let path = directory
            .path()
            .join(if compressed { "image.zst" } else { "image" });
        let encoded = if compressed {
            let mut encoded = Vec::new();
            let mut previous = 0;
            for boundary in [
                version_at + 2,
                version_at + 5,
                vars_at + 3,
                vars_at + 19,
                bytes.len(),
            ] {
                encoded.extend_from_slice(
                    &zstd::bulk::compress(&bytes[previous..boundary], 1).unwrap(),
                );
                previous = boundary;
            }
            encoded
        } else {
            bytes.clone()
        };
        fs::write(&path, encoded).unwrap();
        let dump = CoreDump {
            path: path.clone(),
            source: Source::open(&path, Some(&directory.path().join("cache"))).unwrap(),
            layout: Layout {
                platform: Platform::LinuxX64,
                segments: vec![Segment {
                    address: base,
                    size: bytes.len() as u64,
                    file_offset: 0,
                    writable: true,
                    executable: false,
                }],
                files: vec![FileMapping {
                    start: base,
                    end: base + bytes.len() as u64,
                    file_offset: 0,
                    path: "/libcoreclr.so".into(),
                    deleted: false,
                }],
                threads: Vec::new(),
                exited_thread_stacks: HashSet::new(),
                shared_cache_ranges: Vec::new(),
            },
        };
        assert_eq!(
            dump.find_bytes(base, base + bytes.len() as u64, b"@(#)Version "),
            Some(base + version_at as u64)
        );
        let runtime = RuntimeInfo::find(&dump).unwrap();
        assert!(runtime.version.as_ref().unwrap().contains("10.0.99-test"));
        assert_eq!(runtime.gc_dac_vars, Some(base + vars_at as u64));
        dump.check().unwrap();
    }
}
