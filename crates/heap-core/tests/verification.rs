use heap_core::{
    analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions, verification::verify_file,
};
use serde::Deserialize;
use std::{path::Path, process::Command};

#[derive(Deserialize)]
struct Case {
    name: String,
    expected: Option<(String, String, u64)>,
}

#[test]
fn corrupted_heaps_and_valid_controls() {
    let temporary = tempfile::tempdir().unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/generate-corruption-fixtures.py");
    assert!(
        Command::new(if cfg!(windows) { "python" } else { "python3" })
            .arg(script)
            .arg(temporary.path())
            .status()
            .unwrap()
            .success()
    );
    let cases: Vec<Case> =
        serde_json::from_slice(&std::fs::read(temporary.path().join("expected.json")).unwrap())
            .unwrap();
    for case in cases {
        let path = temporary.path().join(format!("{}.dmp", case.name));
        let result = verify_file(&path, |_, _| {}).unwrap();
        if let Some((kind, structure, address)) = case.expected {
            if kind == "MissingMemory" {
                assert_eq!(result.invalid, 0, "{}: {result:?}", case.name);
            }
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|d| format!("{:?}", d.kind) == kind
                        && d.structure == structure
                        && d.address == address),
                "{}: {result:?}",
                case.name
            );
        } else {
            assert_eq!(
                result.invalid + result.missing_memory + result.unverified,
                0,
                "{}: {result:?}",
                case.name
            );
            assert!(result.objects >= 3, "{}: {result:?}", case.name);
        }
        if case.name.ends_with("many-bad-refs") {
            assert_eq!(result.invalid, 1300);
            assert_eq!(result.diagnostics.len(), 1000);
            assert_eq!(result.report(None).sections[0].omitted, 300);
        }
        if case.name.ends_with("bad-gcdesc") {
            assert!(
                HeapSnapshot::open(
                    &path,
                    vec![],
                    None,
                    AnalysisOptions::default(),
                    CacheChoice::None,
                    |_, _| {}
                )
                .is_err()
            );
        }
        if case.name.ends_with("outside-ref") {
            let cache = temporary.path().join(format!("{}-cache", case.name));
            for cached in [false, true] {
                let s = HeapSnapshot::open(
                    &path,
                    vec![],
                    None,
                    AnalysisOptions::default(),
                    CacheChoice::Folder(cache.clone()),
                    |_, _| {},
                )
                .unwrap();
                assert_eq!(s.from_cache, cached);
                let report = s.verification_check().unwrap();
                assert!(!report.findings.is_empty());
                let row = &report.sections[0].rows[0];
                assert_eq!(s.heap.addresses[row.samples[0]], 0x4020_9018);
                assert!(
                    s.all_checks()
                        .unwrap()
                        .iter()
                        .any(|r| r.name == "verify" && !r.findings.is_empty())
                );
            }
        }
    }
}
