use heap_core::{
    analysis::HeapSnapshot,
    cache::CacheChoice,
    graph::AnalysisOptions,
    symbols::{demangle, function_name},
    type_names::TypeNames,
};
use std::{path::Path, process::Command};

#[test]
fn zero_arrays_count_individual_bytes_and_nested_types_keep_their_names() {
    let temporary = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/generate-compatibility-fixtures.py");
    assert!(
        Command::new(if cfg!(windows) { "python" } else { "python3" })
            .arg(script)
            .arg(temporary.path())
            .status()
            .unwrap()
            .success()
    );
    let snapshot = HeapSnapshot::open(
        &temporary.path().join("query-zero-array.dmp"),
        vec![],
        None,
        AnalysisOptions::default(),
        CacheChoice::None,
        |_, _| {},
    )
    .unwrap();
    let arrays = snapshot.mostly_zero_arrays();
    assert_eq!(arrays.len(), 1);
    assert_eq!(arrays[0].zero_bytes, 960);
    assert_eq!(arrays[0].samples, [0]);
    let base = snapshot.heap.pages[0].start;
    let names = TypeNames::new(snapshot.dump.clone(), base + 0x2100, &[], []);
    assert_eq!(names.name(base + 0x6000), format!("Free{}", "[]".repeat(8)));
    assert_eq!(names.name(base + 0x7000), "?[]");
    assert_eq!(names.name(base + 0x6000 + 7 * 64), "Free[]");
    assert_eq!(
        names.type_argument_handles(base + 0x8000),
        (0..32).map(|i| base + 0x9000 + i * 64).collect::<Vec<_>>()
    );
    let g = snapshot.garbage().unwrap();
    assert_eq!(g.dead_count, 2);
    assert_eq!(g.dead_bytes, 48);
    assert_eq!(g.unreferenced, [1, 2]);
    assert_eq!(g.path_from_unreferenced(2), [2]);
    assert_eq!(g.retained[2], 24);
}

#[test]
fn native_names_preserve_class_and_function_identity() {
    assert_eq!(
        demangle("_ZTVN7rocksdb13LRUCacheShardE").as_deref(),
        Some("vtable for rocksdb::LRUCacheShard")
    );
    assert_eq!(
        function_name("(anonymous namespace)::Worker::Run(int) const"),
        "(anonymous namespace)::Worker::Run"
    );
    assert_eq!(
        demangle("_ZN5space3fooEii").as_deref(),
        Some("space::foo(int, int)")
    );
}
