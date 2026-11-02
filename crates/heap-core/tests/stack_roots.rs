use heap_core::{
    column::ColumnStore,
    core_dump::CoreDump,
    graph::{AnalysisOptions, HeapGraph, RootKind},
    heap::ManagedHeap,
    runtime::RuntimeInfo,
};
use std::{path::Path, process::Command, sync::Arc};

#[test]
fn stack_roots_outside_heap_regions_are_scanned() {
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
    for architecture in [62, 183] {
        for position in ["below", "between", "above"] {
            for native in [false, true] {
                let path = directory
                    .path()
                    .join(format!("stack-roots-{architecture}-{position}.dmp"));
                let mut dump = CoreDump::open(&path, None, None).unwrap();
                if native {
                    dump.layout.threads.clear();
                }
                let dump = Arc::new(dump);
                let runtime = RuntimeInfo::find(&dump);
                let mut heap =
                    ManagedHeap::new(dump, runtime.as_ref(), ColumnStore::default()).unwrap();
                let graph = HeapGraph::new(
                    &mut heap,
                    runtime.as_ref(),
                    AnalysisOptions {
                        native_memory_roots: native,
                    },
                )
                .unwrap();
                assert_eq!(graph.live_count, 3, "{architecture} {position} {native}");
                assert_eq!(graph.pending_count, 1);
                let first = heap.object_at(0x40209018).unwrap();
                let child = heap.object_at(0x40209088).unwrap();
                assert_eq!(graph.state(first), "live");
                assert_eq!(graph.state(child), "live");
                assert!(graph.sources.iter().any(|s| {
                    matches!(s.kind, RootKind::Mapping { .. }) && native
                        || matches!(s.kind, RootKind::Stack { tid: 1 }) && !native
                }));
            }
        }
    }
}
