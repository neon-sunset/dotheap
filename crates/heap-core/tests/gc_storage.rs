use heap_core::{analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions};
use std::{path::Path, process::Command};

#[test]
fn gc_storage_is_separate_from_free_space_and_unknown_data() {
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
    for layout in ["legacy", "previous", "current"] {
        for (kind, storage) in [
            ("work-chunks", Some("mark work chunks")),
            ("finalization-storage", Some("finalization queue")),
            ("unrooted-work-chunks", None),
        ] {
            let name = format!("{layout}-{kind}");
            for cached in [false, true] {
                let snapshot = HeapSnapshot::open(
                    &directory.path().join(format!("{name}.dmp")),
                    vec![],
                    None,
                    AnalysisOptions::default(),
                    CacheChoice::Folder(directory.path().join(format!("{name}-cache"))),
                    |_, _| {},
                )
                .unwrap();
                assert_eq!(snapshot.from_cache, cached, "{name}");
                assert_eq!(snapshot.heap.count(), 0, "{name}");
                let region = &snapshot.heap.regions[0];
                assert_eq!(region.gc_storage.as_deref(), storage, "{name}");
                let totals = snapshot.totals();
                assert_eq!(
                    totals.gc_storage,
                    if storage.is_some() { 2_060_264 } else { 0 }
                );
                assert_eq!(
                    totals.unparsed,
                    if storage.is_some() { 0 } else { 502 * 4096 }
                );
                assert_eq!(totals.free, if storage.is_some() { 0 } else { 4072 });
                assert_eq!(totals.committed_unused, 28_656);
                assert_eq!(
                    totals.gc_storage
                        + totals.unparsed
                        + totals.free
                        + totals.committed_unused
                        + totals.headers,
                    totals.committed,
                    "{name}"
                );
                let report = snapshot.fragmentation().unwrap();
                assert_eq!(report.total_waste, 28_656 + totals.free, "{name}");
                assert!(
                    report
                        .reasons
                        .iter()
                        .any(|r| r.name == "Free regions in allocator queues" && r.regions == 1)
                );
                let kinds = snapshot.satori_region_kinds().unwrap();
                assert_eq!(kinds[0], storage.unwrap_or("unassigned"));
                assert_eq!(kinds[1], "free");
                let map = snapshot.granule_map();
                assert_eq!(map[0][1].gc_storage as u64, totals.gc_storage);
                if storage.is_some() {
                    assert_eq!(map[0][1].gaps(), 0);
                    assert!(
                        report
                            .reasons
                            .iter()
                            .all(|r| !r.region_indices.contains(&0))
                    );
                }
            }
        }
    }
    let snapshot = HeapSnapshot::open(
        &directory.path().join("current.dmp"),
        vec![],
        None,
        AnalysisOptions::default(),
        CacheChoice::None,
        |_, _| {},
    )
    .unwrap();
    let totals = snapshot.totals();
    assert!(totals.pending > 0);
    assert_eq!(
        snapshot.fragmentation().unwrap().total_waste,
        totals.committed - totals.headers - totals.live - totals.pending - totals.unparsed,
    );
}
