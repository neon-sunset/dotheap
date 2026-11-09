use heap_core::{
    analysis::HeapSnapshot,
    cache::{CacheChoice, disk_bytes},
    graph::AnalysisOptions,
};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};

#[derive(Serialize)]
struct Measurement {
    cached: bool,
    objects: usize,
    columns_seconds: f64,
    native_seconds: f64,
    ownership_seconds: f64,
    total_seconds: f64,
    cache_bytes: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: cache_benchmark DUMP CACHE_DIRECTORY".into());
    }
    let dump = PathBuf::from(&args[0]);
    let cache = PathBuf::from(&args[1]);
    let started = Instant::now();
    let snapshot = HeapSnapshot::open(
        &dump,
        vec![],
        None,
        AnalysisOptions::default(),
        CacheChoice::Folder(cache.clone()),
        |_, _| {},
    )?;
    let columns = started.elapsed().as_secs_f64();
    snapshot.native_graph()?;
    let native = started.elapsed().as_secs_f64();
    snapshot.ownership()?;
    let total = started.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::to_string(&Measurement {
            cached: snapshot.from_cache,
            objects: snapshot.heap.count(),
            columns_seconds: columns,
            native_seconds: native - columns,
            ownership_seconds: total - native,
            total_seconds: total,
            cache_bytes: disk_bytes(&cache),
        })?
    );
    Ok(())
}
