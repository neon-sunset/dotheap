use heap_core::{
    column::ColumnStore,
    compressed::{BLOCK_SIZE, Source, compress},
};
use std::io::{Read, Write};

#[test]
fn conversion_is_seekable_lossless_and_leaves_the_source_intact() {
    use heap_core::compressed::{convert, needs_conversion};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("original.zst");
    let target = dir.path().join("converted.zst");
    let bytes: Vec<_> = (0..(17 << 20) + 31)
        .map(|i| (i * 17 + i / 123) as u8)
        .collect();
    let original = zstd::bulk::compress(&bytes, 1).unwrap();
    std::fs::write(&source, &original).unwrap();
    let smaps = b"1000-2000 rw-p 00000000 00:00 0\nRss: 4 kB\n";
    std::fs::write(dir.path().join("original.smaps"), smaps).unwrap();
    assert!(needs_conversion(&source).unwrap());
    assert!(convert(&source, Some(&target), |_| false).is_err());
    assert!(!target.exists());
    assert!(convert(&source, Some(&source), |_| true).is_err());
    let mut last = 0.0;
    heap_core::analysis::convert_dump(&source, Some(&target), |fraction| {
        assert!(fraction >= last && fraction <= 1.0);
        last = fraction;
        true
    })
    .unwrap();
    assert_eq!(last, 1.0);
    assert_eq!(
        std::fs::read(dir.path().join("converted.smaps")).unwrap(),
        smaps
    );
    assert!(!needs_conversion(&target).unwrap());
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(std::fs::File::open(&target).unwrap())
        .unwrap()
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, bytes);
    let cache = dir.path().join("cache");
    let source = Source::open(&target, Some(&cache)).unwrap();
    for start in [0, 4093, BLOCK_SIZE - 3, bytes.len() - 8] {
        assert_eq!(source.bytes(start, 8).unwrap(), &bytes[start..start + 8]);
    }
    assert!(!cache.join("core.blocks").exists());
    assert_eq!(
        std::fs::read(dir.path().join("original.zst")).unwrap(),
        original
    );
}

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

#[test]
fn replacement_preserves_the_source_on_cancellation_failure_and_concurrent_changes() {
    use heap_core::compressed::{convert, needs_conversion};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("replace.zst");
    let bytes: Vec<_> = (0..(17 << 20) + 7)
        .map(|i| (i * 31 + i / 271) as u8)
        .collect();
    let original = zstd::bulk::compress(&bytes, 1).unwrap();
    for cutoff in [0.0, 0.5, 0.99] {
        std::fs::write(&source, &original).unwrap();
        assert!(convert(&source, None, |fraction| fraction < cutoff).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    let damaged = &original[..original.len() - 3];
    std::fs::write(&source, damaged).unwrap();
    assert!(convert(&source, None, |_| true).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), damaged);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    std::fs::write(&source, &original).unwrap();
    let other = dir.path().join("other.zst");
    std::fs::write(&other, b"a newer file").unwrap();
    let mut changed = false;
    let result = convert(&source, None, |fraction| {
        if fraction >= 0.99 && !changed {
            std::fs::rename(&other, &source).unwrap();
            changed = true;
        }
        true
    });
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("changed during conversion")
    );
    assert_eq!(std::fs::read(&source).unwrap(), b"a newer file");

    std::fs::write(&source, &original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let mut old_reader = std::fs::File::open(&source).unwrap();
    convert(&source, None, |_| true).unwrap();
    assert!(!needs_conversion(&source).unwrap());
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(std::fs::File::open(&source).unwrap())
        .unwrap()
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, bytes);
    let mut saved_original = Vec::new();
    old_reader.read_to_end(&mut saved_original).unwrap();
    assert_eq!(saved_original, original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&source).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[cfg(unix)]
#[test]
fn replacement_keeps_a_symbolic_link_to_the_dump() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.zst");
    let link = dir.path().join("link.zst");
    let bytes = vec![3; (17 << 20) + 7];
    std::fs::write(&source, zstd::bulk::compress(&bytes, 1).unwrap()).unwrap();
    std::os::unix::fs::symlink(&source, &link).unwrap();
    heap_core::compressed::convert(&link, None, |_| true).unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!heap_core::compressed::needs_conversion(&source).unwrap());
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(std::fs::File::open(&link).unwrap())
        .unwrap()
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, bytes);
}
