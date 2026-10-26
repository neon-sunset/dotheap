use heap_core::{analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions};
use std::{fs, path::Path, process::Command};

#[test]
fn captured_macos_residency_survives_caching_and_allows_an_external_override() {
    use heap_core::{
        core_dump::CoreDump,
        smaps::{CapturedResidency, SmapsMapping},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("capture.dmp");
    let note = CapturedResidency {
        version: 1,
        physical_footprint: Some(32768),
        mappings: vec![SmapsMapping {
            start: 0x10000,
            end: 0x20000,
            path: "vmmap: Anonymous memory".into(),
            rss: 16384,
            anon_huge: 0,
            private_bytes: 16384,
            swap: 16384,
            readable: true,
        }],
    };
    let payload = serde_json::to_vec(&note).unwrap();
    let mut bytes = vec![0u8; 144 + 65536];
    let offset = bytes.len() as u64;
    for (at, value) in [
        (0, 0xfeedfacfu32),
        (4, 0x100000c),
        (12, 4),
        (16, 2),
        (20, 112),
        (32, 0x19),
        (36, 72),
        (88, 3),
        (92, 3),
        (104, 0x31),
        (108, 40),
    ] {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (at, value) in [
        (56, 0x10000u64),
        (64, 0x10000),
        (72, 144),
        (80, 65536),
        (128, offset),
        (136, payload.len() as u64),
    ] {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes[112..128].copy_from_slice(b"dotheap.resident");
    bytes.extend_from_slice(&payload);
    fs::write(&path, &bytes).unwrap();
    let cache = CacheChoice::Folder(directory.path().join("cache"));
    for from_cache in [false, true] {
        let snapshot = HeapSnapshot::open(
            &path,
            vec![],
            None,
            AnalysisOptions::default(),
            cache.clone(),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(snapshot.from_cache, from_cache);
        assert_eq!(
            snapshot.dump.residency.as_ref().unwrap().physical_footprint,
            Some(32768)
        );
        let mappings = snapshot.smaps.as_ref().unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].rss, 16384);
        assert_eq!(mappings[0].swap, 16384);
    }
    let external = directory.path().join("other.smaps");
    fs::write(&external, "10000-20000 rw-p 00000000 00:00 0\nRss: 32 kB\n").unwrap();
    let snapshot = HeapSnapshot::open(
        &path,
        vec![],
        Some(external),
        AnalysisOptions::default(),
        cache,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(snapshot.smaps.as_ref().unwrap()[0].rss, 32768);
    bytes[136..144].copy_from_slice(&u64::MAX.to_le_bytes());
    fs::write(&path, &bytes).unwrap();
    assert!(CoreDump::open(&path, None, None).is_err());
}

#[test]
fn residency_is_reused_and_file_rss_follows_the_matching_smaps() {
    let directory = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/generate-compatibility-fixtures.py");
    assert!(
        Command::new(if cfg!(windows) { "python" } else { "python3" })
            .arg(script)
            .arg(directory.path())
            .status()
            .unwrap()
            .success()
    );
    let dump = directory.path().join("reference-rooted.dmp");
    let open = |smaps, cache| {
        HeapSnapshot::open(
            &dump,
            vec![],
            smaps,
            AnalysisOptions::default(),
            cache,
            |_, _| {},
        )
        .unwrap()
    };
    let snapshot = open(None, CacheChoice::None);
    assert!(snapshot.residency().is_none());
    assert!(
        snapshot
            .files()
            .unwrap()
            .mapped
            .iter()
            .all(|f| f.resident.is_none())
    );
    let file = &snapshot.dump.files[0];
    let cache = CacheChoice::Folder(directory.path().join("cache"));
    for (name, rss, path, shift, expected) in [
        ("matching", 1, file.path.as_str(), 0, Some(2048)),
        ("changed", 2, file.path.as_str(), 0, Some(4096)),
        ("missing-path", 1, "/another/file.so", 0, Some(0)),
        ("mismatched", 1, file.path.as_str(), 1 << 40, None),
    ] {
        let mut text = String::new();
        for mapping in &snapshot.native.mappings {
            let (start, end) = (mapping.start + shift, mapping.end + shift);
            if mapping.start == file.start {
                let middle = start + (end - start) / 2;
                for (from, to) in [(start, middle), (middle, end)] {
                    text +=
                        &format!("{from:x}-{to:x} r--p 00000000 00:00 0 {path}\nRss: {rss} kB\n");
                }
            } else {
                text += &format!(
                    "{start:x}-{end:x} rw-p 00000000 00:00 0\nRss: {} kB\n",
                    (end - start) / 1024
                );
            }
        }
        let smaps = directory.path().join(format!("{name}.smaps"));
        fs::write(&smaps, text).unwrap();
        for _ in 0..2 {
            let snapshot = open(Some(smaps.clone()), cache.clone());
            let residency = snapshot.residency().unwrap();
            assert!(std::ptr::eq(residency, snapshot.residency().unwrap()));
            assert_eq!(residency.agrees(), shift == 0);
            let files = snapshot.files().unwrap();
            let mapped = files.mapped.iter().find(|f| f.path == file.path).unwrap();
            assert_eq!(mapped.resident, expected, "{name}");
            assert_eq!(
                snapshot.files().unwrap().mapped[0].resident,
                mapped.resident
            );
        }
    }
}
