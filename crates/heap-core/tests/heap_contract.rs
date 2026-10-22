use heap_core::{
    analysis::HeapSnapshot,
    cache::CacheChoice,
    column::ColumnStore,
    core_dump::CoreDump,
    graph::{AnalysisOptions, HeapGraph},
    heap::ManagedHeap,
    runtime::RuntimeInfo,
};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path, process::Command, sync::Arc};

#[derive(Deserialize)]
struct Case {
    name: String,
    layout: String,
    error: Option<String>,
    objects: usize,
    references: Vec<i32>,
    rooted: bool,
    finalizable: bool,
    storage: Option<String>,
}

#[test]
fn satori_allocation_contexts_identify_threads_without_stack_or_tls_registers() {
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
    for architecture in [62, 183] {
        for mode in [
            "regular",
            "large",
            "shared",
            "bad-backpointer",
            "missing-descriptor",
            "unknown-thread",
            "null-locals",
            "overflow-locals",
            "ambiguous",
        ] {
            let name = format!("satori-thread-{architecture}-{mode}");
            let path = temporary.path().join(format!("{name}.dmp"));
            let cache = temporary.path().join(format!("{name}-cache"));
            let open = || {
                HeapSnapshot::open(
                    &path,
                    Vec::new(),
                    None,
                    AnalysisOptions::default(),
                    CacheChoice::Folder(cache.clone()),
                    |_, _| {},
                )
                .unwrap()
            };
            let snapshot = open();
            let expected = matches!(mode, "regular" | "large").then_some(4001);
            assert!(!snapshot.from_cache);
            assert_eq!(
                snapshot.heap.regions[0].allocating_thread, expected,
                "{name}"
            );
            assert_eq!(snapshot.heap.regions[0].owner_thread, None, "{name}");
            assert!(snapshot.dump.threads.iter().all(|t| t.stack_pointer == 0));
            drop(snapshot);
            let manifest = cache.join("manifest.json");
            let mut saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            saved["heap"]["regions"][0]["allocatingThread"] = serde_json::Value::Null;
            std::fs::write(manifest, serde_json::to_vec(&saved).unwrap()).unwrap();
            let cached = open();
            assert!(cached.from_cache, "{name}");
            assert_eq!(
                cached.heap.regions[0].allocating_thread, expected,
                "cached {name}"
            );
        }
    }
}

#[test]
fn synthetic_layouts_references_and_finalization() {
    let temporary = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/generate-compatibility-fixtures.py");
    let status = Command::new(if cfg!(windows) { "python" } else { "python3" })
        .arg(script)
        .arg(temporary.path())
        .status()
        .unwrap();
    assert!(status.success());
    let cases: Vec<Case> =
        serde_json::from_slice(&std::fs::read(temporary.path().join("expected.json")).unwrap())
            .unwrap();
    for case in cases {
        let result = (|| {
            let dump = Arc::new(CoreDump::open(
                &temporary.path().join(format!("{}.dmp", case.name)),
                None,
                None,
            )?);
            let runtime = RuntimeInfo::find(&dump);
            let mut heap = ManagedHeap::new(dump, runtime.as_ref(), ColumnStore::default())?;
            let graph = HeapGraph::new(&mut heap, runtime.as_ref(), AnalysisOptions::default())?;
            Ok::<_, heap_core::Error>((heap, graph))
        })();
        if let Some(error) = case.error {
            let actual = result
                .err()
                .unwrap_or_else(|| panic!("{} succeeded", case.name));
            assert!(
                actual.to_string().contains(&error),
                "{}: {actual}",
                case.name
            );
            continue;
        }
        let (heap, graph) = result.unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(heap.kind.layout, case.layout, "{}", case.name);
        assert_eq!(heap.count(), case.objects, "{}", case.name);
        assert_eq!(
            graph.pending_count,
            usize::from(case.finalizable),
            "{}",
            case.name
        );
        if let Some(storage) = &case.storage {
            assert_eq!(
                heap.regions.iter().map(|r| r.unparsed_bytes).sum::<u64>(),
                if storage == "free" { 0 } else { 160 },
                "{}",
                case.name
            );
            assert_eq!(
                heap.regions.iter().map(|r| r.free_bytes).sum::<u64>(),
                if storage == "free" { 160 } else { 0 },
                "{}",
                case.name
            );
        }
        if case.objects == 0 {
            continue;
        }
        assert_eq!(
            graph.reference_rows.row(0),
            &case.references,
            "{}",
            case.name
        );
        assert!(
            graph.reference_rows.row(1).is_empty() && graph.reference_rows.row(2).is_empty(),
            "{}",
            case.name
        );
        if !case.finalizable {
            let live = [
                u8::from(case.rooted),
                u8::from(case.rooted && case.references.contains(&1)),
                u8::from(case.rooted && case.references.contains(&2)),
            ];
            assert_eq!(&graph.live[..], live, "{}", case.name);
            if case.rooted {
                assert_eq!(
                    graph.retained[0],
                    heap.size(0)
                        + 24 * case.references.iter().collect::<BTreeSet<_>>().len() as u64,
                    "{}",
                    case.name
                );
            }
        }
    }
}
