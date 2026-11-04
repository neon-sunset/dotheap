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
            assert_eq!(
                snapshot.heap.kind.stock.as_ref().unwrap().uses_regions,
                mode.ends_with("regions"),
                "{architecture} {mode}"
            );
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
            check_map(&snapshot);
        }
        for name in ["2mb-region", "2mb-segment-range", "cross-page-segment"] {
            let snapshot = HeapSnapshot::open(
                &directory
                    .path()
                    .join(format!("coreclr-{architecture}-{name}.dmp")),
                Vec::new(),
                None,
                AnalysisOptions::default(),
                CacheChoice::None,
                |_, _| {},
            )
            .unwrap();
            check_map(&snapshot);
            let occupancy: usize = snapshot.occupancy().iter().flat_map(|h| h.counts).sum();
            assert_eq!(occupancy, usize::from(name == "2mb-region"), "{name}");
            if name == "cross-page-segment" {
                assert_eq!(snapshot.heap.pages.len(), 3);
            }
        }
    }
}

fn check_map(snapshot: &HeapSnapshot) {
    let map = snapshot.granule_map();
    for (page, cells) in snapshot.heap.pages.iter().zip(&map) {
        let mut address = page.start;
        for cell in cells {
            assert_eq!(cell.address, address);
            assert!(cell.size > 0 && cell.size <= 2 * 1024 * 1024);
            address += u64::from(cell.size);
            if cell.region >= 0 {
                let region = &snapshot.heap.regions[cell.region as usize];
                assert!(cell.address >= region.start && address <= region.end);
            }
        }
        assert_eq!(address, page.end);
    }
    for region in &snapshot.heap.regions {
        let cells: Vec<_> = map
            .iter()
            .flatten()
            .filter(|c| c.region == region.index as i32)
            .collect();
        assert_eq!(
            cells.iter().map(|c| u64::from(c.size)).sum::<u64>(),
            region.size()
        );
        assert_eq!(
            cells.iter().map(|c| u64::from(c.used)).sum::<u64>(),
            region.used_bytes()
        );
        assert_eq!(
            cells.iter().map(|c| u64::from(c.committed)).sum::<u64>(),
            region.committed_bytes()
        );
        assert_eq!(
            cells.iter().map(|c| u64::from(c.live)).sum::<u64>(),
            region.live_bytes
        );
        assert_eq!(
            cells.iter().map(|c| u64::from(c.pending)).sum::<u64>(),
            region.pending_bytes
        );
        assert_eq!(
            cells.iter().map(|c| u64::from(c.dead)).sum::<u64>(),
            region.dead_bytes()
        );
        assert_eq!(cells.iter().filter(|c| c.is_region_start).count(), 1);
    }
}
