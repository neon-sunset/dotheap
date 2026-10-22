use heap_core::{analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions, heap::Gc};
use std::{path::Path, process::Command};

#[test]
fn coreclr_regions_segments_server_heaps_and_finalization() {
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
        for mode in [
            "workstation-regions",
            "workstation-segments",
            "server-regions",
        ] {
            let snapshot = HeapSnapshot::open(
                &directory
                    .path()
                    .join(format!("coreclr-{architecture}-{mode}.dmp")),
                Vec::new(),
                None,
                AnalysisOptions::default(),
                CacheChoice::None,
                |_, _| {},
            )
            .unwrap();
            assert_eq!(snapshot.heap.kind.gc, Gc::CoreClr, "{architecture} {mode}");
            let server = mode.starts_with("server");
            assert_eq!(snapshot.heap.count(), if server { 5 } else { 4 }, "{mode}");
            assert_eq!(snapshot.graph.live_count, 3, "{mode}");
            assert_eq!(snapshot.graph.pending_count, 1, "{mode}");
            let first = snapshot.heap.object_at(0x40209018).unwrap();
            let second = snapshot.heap.object_at(0x40209088).unwrap();
            let pending = snapshot.heap.object_at(0x402090a0).unwrap();
            assert_eq!(snapshot.graph.references(first), [second as i32]);
            assert_eq!(snapshot.graph.retained[first], 56);
            assert_eq!(snapshot.graph.pending[pending], 1);
            assert_eq!(
                snapshot.heap.regions.iter().filter(|r| r.immortal).count(),
                1
            );
            assert_eq!(
                snapshot
                    .heap
                    .regions
                    .iter()
                    .map(|r| r.unparsed_bytes)
                    .sum::<u64>(),
                80
            );
            if mode.ends_with("segments") {
                let names: Vec<_> = [first, second, pending]
                    .map(|i| {
                        snapshot.heap.regions[snapshot.heap.region_of(i)]
                            .generation_name
                            .as_str()
                    })
                    .into();
                assert_eq!(names, ["gen2", "gen1", "gen0"]);
            }
            if server {
                assert_eq!(
                    snapshot
                        .heap
                        .kind
                        .stock
                        .as_ref()
                        .unwrap()
                        .dynamic_adaptation,
                    Some(true)
                );
                assert!(snapshot.heap.regions.iter().any(|r| r.gc_heap == 1));
            }
        }
    }
}
