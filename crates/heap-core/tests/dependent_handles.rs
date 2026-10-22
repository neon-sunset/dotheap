use heap_core::{analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions};
use std::{path::Path, process::Command};

#[test]
fn dependent_handles_connect_dead_objects_without_making_them_live() {
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
    let mut snapshot = HeapSnapshot::open(
        &directory.path().join("scalar-dead.dmp"),
        Vec::new(),
        None,
        AnalysisOptions::default(),
        CacheChoice::None,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(&snapshot.graph.live[..], &[0, 0, 0]);
    snapshot.graph.saved.dependents.insert(0, vec![1]);
    let garbage = snapshot.garbage().unwrap();
    assert_eq!(garbage.references.row(0), &[1]);
    assert_eq!(garbage.referrers.row(1), &[0]);
    assert_eq!(garbage.unreferenced, [0, 2]);
    assert_eq!(garbage.idom[1], 0);
    assert_eq!(
        garbage.retained[0],
        snapshot.heap.size(0) + snapshot.heap.size(1)
    );
}
