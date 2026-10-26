use heap_core::{
    analysis::HeapSnapshot, cache::CacheChoice, compressed::atomic_write, graph::AnalysisOptions,
};
use std::{fs, path::Path, process::Command, sync::Mutex};

#[test]
fn invalid_heap_and_graph_caches_are_rebuilt_from_the_dump() {
    let directory = tempfile::tempdir().unwrap();
    let fixtures = directory.path().join("fixtures");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/generate-compatibility-fixtures.py");
    assert!(
        Command::new(if cfg!(windows) { "python" } else { "python3" })
            .arg(script)
            .arg(&fixtures)
            .status()
            .unwrap()
            .success()
    );
    let path = fixtures.join("reference-rooted.dmp");
    let cache = directory.path().join("cache");
    let open = || {
        let progress = Mutex::new(Vec::new());
        let snapshot = HeapSnapshot::open(
            &path,
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::Folder(cache.clone()),
            |phase, fraction| progress.lock().unwrap().push((phase.to_owned(), fraction)),
        )
        .unwrap();
        let progress = progress.into_inner().unwrap();
        assert_eq!(progress.first().unwrap().1, 0.0);
        assert_eq!(progress.last().unwrap(), &("Done".into(), 1.0));
        assert!(
            progress
                .iter()
                .all(|(_, fraction)| fraction.is_finite() && (0.0..=1.0).contains(fraction))
        );
        assert!(
            progress.windows(2).all(|pair| pair[0].1 <= pair[1].1),
            "{progress:?}"
        );
        let expected = if snapshot.from_cache {
            &[
                "Opening cached objects",
                "Opening cached references",
                "Resolving type names",
            ][..]
        } else {
            &[
                "Walking the heap",
                "References and roots",
                "Native memory",
                "Types",
            ][..]
        };
        for phase in expected {
            assert!(
                progress
                    .iter()
                    .any(|(name, fraction)| name == phase && *fraction < 1.0),
                "{progress:?}"
            );
        }
        snapshot
    };
    let original = open();
    assert!(!original.from_cache);
    assert!(open().from_cache);
    for case in [
        "missing",
        "truncated",
        "reference",
        "parent-cycle",
        "dominator-cycle",
        "address-index",
        "large-size",
        "region",
        "layout",
        "type-count",
    ] {
        match case {
            "missing" => fs::remove_file(cache.join("addresses.col")).unwrap(),
            "truncated" => atomic_write(&cache.join("references.items.col"), &[]).unwrap(),
            "reference" => {
                atomic_write(&cache.join("references.items.col"), &i32::MAX.to_le_bytes()).unwrap()
            }
            "parent-cycle" | "dominator-cycle" | "address-index" | "large-size" => {
                let (name, value) = match case {
                    "parent-cycle" => ("parent.col", 0u32),
                    "dominator-cycle" => ("idom.col", 0u32),
                    "address-index" => ("granuleFirst.col", u32::MAX),
                    _ => ("sizes.col", u32::MAX),
                };
                let file = cache.join(name);
                let mut bytes = fs::read(&file).unwrap();
                bytes[..4].copy_from_slice(&value.to_le_bytes());
                atomic_write(&file, &bytes).unwrap();
            }
            _ => {
                let file = cache.join("manifest.json");
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
                match case {
                    "region" => manifest["heap"]["regions"][0]["objectStart"] = 0u64.into(),
                    "layout" => manifest["core"]["segments"][0]["address"] = u64::MAX.into(),
                    "type-count" => manifest["typeStats"][0]["count"] = 0u64.into(),
                    _ => unreachable!(),
                }
                atomic_write(&file, &serde_json::to_vec(&manifest).unwrap()).unwrap();
            }
        }
        let rebuilt = open();
        assert!(!rebuilt.from_cache, "{case}");
        assert_eq!(
            &rebuilt.heap.addresses[..],
            &original.heap.addresses[..],
            "{case}"
        );
        assert_eq!(&rebuilt.graph.live[..], &[1, 1, 0], "{case}");
        assert_eq!(rebuilt.graph.references(0), [1], "{case}");
        assert!(open().from_cache, "{case}");
    }

    let graph = open().native_graph().unwrap();
    assert!(graph.chunks.is_empty());
    let graph_directory = fs::read_dir(cache.join("rust-results"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join("native-graph"))
        .find(|path| path.is_dir())
        .unwrap();
    for (file, invalid) in [
        ("reach.col", vec![0]),
        ("edgeStart.col", 1u64.to_le_bytes().to_vec()),
        ("edgeTargets.col", i32::MAX.to_le_bytes().to_vec()),
        ("edgeExact.col", vec![2]),
        ("ownedChunks.start.col", 1u64.to_le_bytes().to_vec()),
        ("chunkDominator.col", 0i32.to_le_bytes().to_vec()),
        ("chunks.col", vec![0; 23]),
        ("manifest.json", br#"{"format":99}"#.to_vec()),
    ] {
        let path = graph_directory.join(file);
        let original = fs::read(&path).unwrap();
        atomic_write(&path, &invalid).unwrap();
        let rebuilt = open().native_graph().unwrap();
        assert!(rebuilt.chunks.is_empty(), "{file}");
        assert_eq!(fs::read(&path).unwrap(), original, "{file}");
    }
}
