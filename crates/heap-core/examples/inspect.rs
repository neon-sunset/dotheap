use heap_core::{analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions};
use std::{path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or("usage: inspect <dump> <cache>")?);
    let cache = PathBuf::from(args.next().ok_or("usage: inspect <dump> <cache>")?);
    let mode = args.next();
    let started = Instant::now();
    let snapshot = HeapSnapshot::open(
        &path,
        vec![],
        None,
        AnalysisOptions::default(),
        CacheChoice::Folder(cache),
        |phase, _| eprintln!("{:.3}s {phase}", started.elapsed().as_secs_f64()),
    )?;
    let mut output = serde_json::json!({
        "heap": snapshot.heap.saved,
        "graph": snapshot.graph.saved,
        "native": snapshot.native,
        "typeStats": snapshot.type_stats,
        "fromCache": snapshot.from_cache,
    });
    if mode.as_deref() == Some(std::ffi::OsStr::new("checks")) {
        output["checks"] = serde_json::to_value(snapshot.all_checks()?)?;
        output["findings"] = serde_json::to_value(snapshot.summary_findings()?)?;
    } else if mode.as_deref() == Some(std::ffi::OsStr::new("tasks")) {
        output["async"] = serde_json::to_value(snapshot.async_analysis()?)?;
        output["queued"] = serde_json::to_value(snapshot.queued_work_items()?)?;
    } else if mode.as_deref() == Some(std::ffi::OsStr::new("resources")) {
        output["network"] = serde_json::to_value(snapshot.network()?)?;
        output["connections"] = serde_json::to_value(snapshot.connections_check()?)?;
        output["timers"] = serde_json::to_value(snapshot.timers()?)?;
        output["files"] = serde_json::to_value(snapshot.files()?)?;
        output["fragmentation"] = serde_json::to_value(snapshot.fragmentation()?)?;
    } else if mode.is_some() {
        eprintln!("{:.3}s Native graph", started.elapsed().as_secs_f64());
        let native = snapshot.native_graph()?;
        output["nativeGraph"] = serde_json::to_value(&*native.data)?;
        output["nativeGroups"] = serde_json::to_value(native.groups(&snapshot)?)?;
        eprintln!("{:.3}s Ownership graph", started.elapsed().as_secs_f64());
        output["ownershipGroups"] = serde_json::to_value(snapshot.ownership_groups(0)?)?;
        output["threads"] = serde_json::to_value(&snapshot.thread_stacks()?.threads)?;
        output["versions"] = serde_json::to_value(snapshot.versions())?;
        output["handles"] = serde_json::to_value(snapshot.handle_groups())?;
        output["nativeTotals"] = serde_json::to_value(native.totals())?;
        let garbage = snapshot.garbage()?;
        output["garbage"] = serde_json::json!({
            "deadCount": garbage.dead_count,
            "deadBytes": garbage.dead_bytes,
            "unreferenced": garbage.unreferenced,
        });
        eprintln!("{:.3}s Queries done", started.elapsed().as_secs_f64());
    }
    serde_json::to_writer(std::io::stdout().lock(), &output)?;
    Ok(())
}
