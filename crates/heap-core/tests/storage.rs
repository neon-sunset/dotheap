use heap_core::{
    column::ColumnStore,
    compressed::{BLOCK_SIZE, Source, compress},
};
use std::io::Write;

#[test]
fn replacing_a_column_preserves_an_open_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = ColumnStore::new(Some(dir.path().into())).unwrap();
    let first = store
        .copy(&[1u64, 2, 3], Some("objects"))
        .unwrap()
        .freeze()
        .unwrap();
    let opened = store.open::<u64>("objects").unwrap();
    let second = store
        .copy(&[5u64, 8, 13, 21], Some("objects"))
        .unwrap()
        .freeze()
        .unwrap();
    assert_eq!(&first[..], &[1, 2, 3]);
    assert_eq!(&opened[..], &[1, 2, 3]);
    assert_eq!(&second[..], &[5, 8, 13, 21]);
    assert_eq!(&store.open::<u64>("objects").unwrap()[..], &[5, 8, 13, 21]);
    store.make::<u8>(0, Some("empty")).unwrap();
    assert_eq!(store.open::<u8>("empty").unwrap().len(), 0);
}

#[test]
fn compressed_frames_cross_boundaries_and_support_parallel_readers() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("input");
    let target = dir.path().join("input.zst");
    let bytes: Vec<_> = (0..BLOCK_SIZE * 3 + 71)
        .map(|i| ((i * 19 + i / 17) & 255) as u8)
        .collect();
    std::fs::write(&source, &bytes).unwrap();
    compress(&source, &target, |_, _| {}).unwrap();
    let memory = Source::open(&target, Some(&dir.path().join("cache"))).unwrap();
    std::thread::scope(|scope| {
        for k in 0..8 {
            let memory = &memory;
            let bytes = &bytes;
            scope.spawn(move || {
                for i in 0..128 {
                    let at = (i * 10471 + k * 193) % bytes.len();
                    let n = 4096.min(bytes.len() - at);
                    assert_eq!(memory.bytes(at, n).unwrap(), &bytes[at..at + n]);
                }
            });
        }
    });
    assert_eq!(
        memory.bytes(BLOCK_SIZE - 11, 40).unwrap(),
        &bytes[BLOCK_SIZE - 11..BLOCK_SIZE + 29]
    );
    let mut scanned = Vec::new();
    memory.scan(0, bytes.len(), |_, part| scanned.extend_from_slice(part));
    assert_eq!(scanned, bytes);
    memory.check().unwrap();
    assert!(!dir.path().join("cache/core.blocks").exists());
    assert!(compress(&source, &target, |_, _| {}).is_err());
}

#[test]
fn damaged_compressed_frame_fails_and_invalidates_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.zst");
    let cache = dir.path().join("cache");
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.set_pledged_src_size(Some(8192)).unwrap();
    encoder.write_all(&vec![7; 8192]).unwrap();
    let mut bytes = encoder.finish().unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(&path, bytes).unwrap();
    let memory = Source::open(&path, Some(&cache)).unwrap();
    assert!(memory.bytes(0, 8).is_none());
    assert!(
        memory
            .check()
            .unwrap_err()
            .to_string()
            .contains("decompress")
    );
    assert!(!cache.join("core.dump").exists());
}

#[test]
fn large_frames_are_reblocked_and_all_zero_blocks_need_no_payload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.zst");
    let cache = dir.path().join("cache");
    let mut bytes = vec![0; BLOCK_SIZE * 129 + 71];
    for block in [1, 63, 64, 65, 127, 128, 129] {
        bytes[block * BLOCK_SIZE + 17] = block as u8;
    }
    std::fs::write(&path, zstd::bulk::compress(&bytes, 1).unwrap()).unwrap();
    let memory = Source::open(&path, Some(&cache)).unwrap();
    assert_eq!(memory.len(), bytes.len());
    assert_eq!(
        memory.bytes(BLOCK_SIZE, 64).unwrap(),
        &bytes[BLOCK_SIZE..BLOCK_SIZE + 64]
    );
    assert_eq!(memory.bytes(0, 64).unwrap(), &[0; 64]);
    assert!(cache.join("core.blocks").metadata().unwrap().len() < 1024);
    let mut scanned = Vec::new();
    memory.scan(0, bytes.len(), |_, part| scanned.extend_from_slice(part));
    assert_eq!(scanned, bytes);
    memory.check().unwrap();
    drop(memory);
    let reopened = Source::open(&path, Some(&cache)).unwrap();
    assert_eq!(reopened.bytes(0, bytes.len()).unwrap(), bytes);

    let input = dir.path().join("input");
    let output = dir.path().join("batched.zst");
    std::fs::write(&input, &bytes).unwrap();
    compress(&input, &output, |_, _| {}).unwrap();
    assert_eq!(
        zstd::stream::decode_all(std::fs::File::open(output).unwrap()).unwrap(),
        bytes
    );
}

#[test]
fn variable_frames_and_reused_windows_preserve_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frames.zst");
    let mut bytes = Vec::new();
    let mut packed = Vec::new();
    for (i, size) in [71, 1, BLOCK_SIZE * 2 + 19, 7, BLOCK_SIZE - 5]
        .into_iter()
        .enumerate()
    {
        let frame: Vec<_> = (0..size).map(|k| ((k * 19 + i * 43) & 255) as u8).collect();
        packed.extend(zstd::bulk::compress(&frame, 1).unwrap());
        bytes.extend(frame);
    }
    std::fs::write(&path, packed).unwrap();
    let source = Source::open(&path, Some(&dir.path().join("cache"))).unwrap();
    let mut scratch = Vec::new();
    for at in (0..bytes.len()).step_by(67) {
        let size = 83.min(bytes.len() - at);
        let (start, window) = source.window(at, size, Some(scratch)).unwrap();
        assert_eq!(
            &window[at - start..at - start + size],
            &bytes[at..at + size]
        );
        scratch = match window {
            std::borrow::Cow::Owned(buffer) => buffer,
            std::borrow::Cow::Borrowed(_) => Vec::new(),
        };
    }
    assert!(
        source
            .window(bytes.len(), 0, Some(scratch))
            .unwrap()
            .1
            .is_empty()
    );
    assert!(source.window(bytes.len(), 1, None).is_none());
    assert!(source.window(bytes.len() + 1, 0, None).is_none());
    source.check().unwrap();
}

#[test]
fn changed_source_cannot_reuse_compressed_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dump.zst");
    let cache = dir.path().join("cache");
    std::fs::write(&path, zstd::bulk::compress(&vec![1; 4096], 1).unwrap()).unwrap();
    {
        let first = Source::open(&path, Some(&cache)).unwrap();
        assert_eq!(first.bytes(0, 1), Some(&[1][..]));
    }
    let replacement = dir.path().join("replacement");
    std::fs::write(
        &replacement,
        zstd::bulk::compress(&vec![2; 4096], 1).unwrap(),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let second = Source::open(&path, Some(&cache)).unwrap();
    assert_eq!(second.bytes(0, 1), Some(&[2][..]));
}

#[test]
fn an_old_snapshot_cannot_invalidate_a_rebuilt_cache() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("dump.zst");
    let cache = directory.path().join("cache");
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.set_pledged_src_size(Some(4096)).unwrap();
    encoder.write_all(&vec![7; 4096]).unwrap();
    let mut corrupt = encoder.finish().unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    std::fs::write(&path, corrupt).unwrap();
    let old = Source::open(&path, Some(&cache)).unwrap();
    let replacement = directory.path().join("replacement");
    std::fs::write(
        &replacement,
        zstd::bulk::compress(&vec![9; 4096], 1).unwrap(),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let current = Source::open(&path, Some(&cache)).unwrap();
    std::fs::write(cache.join("manifest.json"), b"new manifest").unwrap();
    let marker = std::fs::read(cache.join("core.dump")).unwrap();
    assert!(old.bytes(0, 8).is_none());
    assert!(old.check().is_err());
    assert_eq!(std::fs::read(cache.join("core.dump")).unwrap(), marker);
    assert_eq!(
        std::fs::read(cache.join("manifest.json")).unwrap(),
        b"new manifest"
    );
    assert_eq!(current.bytes(0, 8).unwrap(), &[9; 8]);
    current.check().unwrap();
}
