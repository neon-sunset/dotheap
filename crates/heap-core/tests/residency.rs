use heap_core::{analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions};
use std::{fs, path::Path, process::Command};

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
