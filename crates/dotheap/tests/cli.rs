use dotheap::{
    arguments::{self, HELP, Parsed},
    output::{self, Value},
};
use serde_json::json;
use std::{
    path::Path,
    process::{Command, Output},
};

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dotheap"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn every_command_has_help_without_opening_a_dump() {
    for name in HELP
        .commands
        .iter()
        .map(|c| c.name.as_str())
        .chain(["report"])
    {
        let output = run(&[name, "--help"]);
        assert!(output.status.success(), "{name}");
        assert!(output.stderr.is_empty(), "{name}");
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(
            help.starts_with(&format!("usage: dotheap {name}")),
            "{name}"
        );
        assert!(help.contains("options:"), "{name}");
    }
}

#[test]
fn argument_errors_are_distinct_from_analysis_errors() {
    for args in [
        vec!["types"],
        vec!["types", "missing.dmp", "--top", "3"],
        vec!["types", "missing.dmp", "--limit", "-1"],
        vec!["types", "missing.dmp", "--limit", "many"],
        vec!["report", "missing.dmp", "--top", "-1"],
        vec!["report", "missing.dmp", "--json"],
        vec!["summary", "missing.dmp", "extra"],
        vec!["dump", "not-a-pid"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(output.stderr.starts_with(b"dotheap: "), "{args:?}");
    }
    let output = run(&["summary", "missing.dmp", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn options_preserve_paths_repeated_folders_and_unlimited_rows() {
    let raw = [
        "types",
        "a file.dmp",
        "--dlls",
        "app",
        "--dlls",
        "runtime",
        "--limit",
        "0",
    ];
    let Parsed::Run(args) = arguments::parse(&raw.map(str::to_owned)).unwrap() else {
        panic!("expected command")
    };
    assert_eq!(args.core, "a file.dmp");
    assert_eq!(args.dlls, ["app", "runtime"]);
    assert_eq!(args.limit(), usize::MAX);
}

#[test]
fn json_keeps_integer_ranges_addresses_and_omitted_counts() {
    let value = output::record([
        ("bytes", output::bytes(u64::MAX)),
        ("count", output::int(u64::MAX)),
        ("signed", output::int(i64::MIN)),
        ("address", output::address(0x1234)),
        ("items", output::limited(0..4, 2, output::int)),
    ]);
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({
            "bytes": u64::MAX, "count": u64::MAX, "signed": i64::MIN, "address": "0x1234", "items": [0, 1], "itemsOmitted": 2,
        })
    );
    assert_eq!(output::int(u64::MAX).scalar(), "18,446,744,073,709,551,615");
    assert_eq!(Value::Text("a".repeat(200)).scalar().len(), 200);
}

#[test]
fn synthetic_heap_outputs_match_independent_expectations() {
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
    for command in [
        "summary", "types", "object", "retained", "findings", "files", "network", "timers",
    ] {
        let mut args = vec![command, dump.to_str().unwrap()];
        if command == "object" {
            args.push("0x40209018");
        }
        args.extend(["--no-cache", "--json"]);
        let output = run(&args);
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value.is_object(), "{command}");
        match command {
            "summary" => assert!(value["next"].as_array().unwrap().iter().any(|next| {
                next.as_str()
                    .is_some_and(|next| next.starts_with("dotheap findings "))
            })),
            "findings" => {
                assert!(value["findings"].is_array());
                let reports = value["reports"].as_array().unwrap();
                assert!(!reports.is_empty());
                for report in reports {
                    let name = report["command"].as_str().unwrap();
                    assert!(HELP.commands.iter().any(|c| c.name == name), "{name}");
                    assert!(report["rows"].is_u64(), "{name}");
                    assert!(!report["title"].as_str().unwrap().is_empty(), "{name}");
                }
            }
            "timers" => {
                assert_eq!(value["title"], "Timers");
                assert!(value["explanation"].is_string());
            }
            _ => {}
        }
    }
}

#[test]
fn verification_and_findings_survive_a_broken_gc_descriptor() {
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
    let path = temporary.path().join("satori-62-bad-gcdesc.dmp");
    let path = path.to_str().unwrap();
    for command in ["verify", "findings"] {
        let output = run(&[command, path, "--json", "--no-cache"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["invalid"], 1);
        assert_eq!(value["diagnostics"][0]["structure"], "GC descriptor");
        assert_eq!(value["diagnostics"][0]["address"], "0x40001ff8");
        if command == "findings" {
            assert!(
                value["analysisError"]
                    .as_str()
                    .unwrap()
                    .contains("GC descriptor")
            );
            assert!(!value["findings"].as_array().unwrap().is_empty());
        }
    }
    let output = run(&["summary", path, "--json", "--no-cache"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("dotheap verify"));
}
