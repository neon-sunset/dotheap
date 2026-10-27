mod capture;
mod report;

use dotheap::{
    arguments::{self, Arguments, Parsed},
    commands::{self, Context},
    output::*,
};
use heap_core::{
    analysis::HeapSnapshot,
    cache::{self, CacheChoice},
    graph::AnalysisOptions,
    live_process,
};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

struct CliError {
    message: String,
    status: u8,
}

impl From<heap_core::Error> for CliError {
    fn from(error: heap_core::Error) -> Self {
        Self {
            message: error.to_string(),
            status: 1,
        }
    }
}
impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self {
            status: if error.kind() == io::ErrorKind::BrokenPipe {
                0
            } else {
                1
            },
            message: error.to_string(),
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            if error.status != 0 {
                let _ = writeln!(io::stderr(), "dotheap: {}", error.message);
            }
            ExitCode::from(error.status)
        }
    }
}

fn run() -> Result<u8, CliError> {
    let raw: Vec<_> = std::env::args().skip(1).collect();
    let mut arguments =
        match arguments::parse(&raw).map_err(|message| CliError { message, status: 2 })? {
            Parsed::Print(text, status) => {
                writeln!(io::stdout().lock(), "{text}")?;
                return Ok(status);
            }
            Parsed::Run(arguments) => arguments,
        };
    match arguments.command.as_str() {
        "cache" => {
            cache_command(&arguments)?.print(arguments.flag("--json"))?;
            return Ok(0);
        }
        "ps" => {
            record([(
                "processes",
                list(live_process::list().iter().map(|p| {
                    record([
                        ("pid", Value::Id(p.pid as i64)),
                        ("user", Value::Id(p.uid as i64)),
                        ("program", text(&p.name)),
                        (
                            "runtime",
                            text(if p.socket.is_some() { ".NET" } else { "other" }),
                        ),
                        (
                            "socket",
                            text(
                                p.socket
                                    .as_ref()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                            ),
                        ),
                    ])
                })),
            )])
            .print(arguments.flag("--json"))?;
            return Ok(0);
        }
        "dump" => {
            let path = capture::capture(&arguments.core, &arguments)?;
            if arguments.flag("--json") {
                record([("dump", text(path.to_string_lossy()))]).print(true)?;
            } else {
                writeln!(io::stdout().lock(), "{}", path.display())?;
            }
            return Ok(0);
        }
        _ => {}
    }
    if let Some(pid) = arguments.value("--pid") {
        let path = capture::capture(pid, &arguments)?;
        if !arguments.core.is_empty() {
            arguments
                .positional
                .insert(0, std::mem::take(&mut arguments.core));
        }
        arguments.core = path.to_string_lossy().into_owned();
    }
    if !Path::new(&arguments.core).exists()
        && let Some(index) = arguments
            .positional
            .iter()
            .position(|s| Path::new(s).exists())
    {
        let core = arguments.positional.remove(index);
        arguments
            .positional
            .insert(0, std::mem::replace(&mut arguments.core, core));
    }
    if !Path::new(&arguments.core).exists() {
        return Err(
            heap_core::Error(format!("cannot open {}: no such file", arguments.core)).into(),
        );
    }
    if arguments.command == "verify" {
        let last_phase = std::cell::RefCell::new(String::new());
        let result =
            heap_core::verification::verify_file(Path::new(&arguments.core), |name, _| {
                if arguments.flag("--progress") && *last_phase.borrow() != name {
                    eprintln!("{name}");
                    *last_phase.borrow_mut() = name.into();
                }
            })?;
        verification_output(&result, arguments.limit()).print(arguments.flag("--json"))?;
        return Ok(0);
    }
    let cache = if arguments.flag("--no-cache") {
        CacheChoice::None
    } else if arguments.flag("--rebuild") {
        CacheChoice::Rebuild
    } else if let Some(path) = arguments.value("--cache") {
        CacheChoice::Folder(path.into())
    } else {
        CacheChoice::Automatic
    };
    let started = Instant::now();
    let status = std::sync::Mutex::new((String::new(), started, false));
    let opened = HeapSnapshot::open(
        Path::new(&arguments.core),
        arguments.dlls.iter().map(PathBuf::from).collect(),
        arguments.value("--smaps").map(PathBuf::from),
        AnalysisOptions {
            native_memory_roots: arguments.flag("--native-roots"),
        },
        cache.clone(),
        |name, _| {
            let mut status = status.lock().unwrap();
            let (phase, phase_start, announced) = &mut *status;
            if name == *phase {
                return;
            }
            if arguments.flag("--progress") {
                if !phase.is_empty() {
                    eprintln!("{phase}: {:.1} s", phase_start.elapsed().as_secs_f64());
                }
            } else if !*announced && ["Walking the heap", "Reading the .zst dump"].contains(&name) {
                *announced = true;
                eprintln!(
                    "dotheap: processing the dump; {}",
                    if cache == CacheChoice::None {
                        "--no-cache: the result is not kept"
                    } else {
                        "later commands open the cached result"
                    }
                );
            }
            *phase = name.into();
            *phase_start = Instant::now();
        },
    );
    let snapshot = match opened {
        Ok(snapshot) => snapshot,
        Err(error) if arguments.command == "findings" => {
            let result =
                heap_core::verification::verify_file(Path::new(&arguments.core), |_, _| {})?;
            let report = result.report(None);
            let mut value = verification_output(&result, arguments.limit());
            value.add("analysisError", text(error.to_string()));
            value.add(
                "findings",
                list(report.findings.iter().map(|f| text(&f.text))),
            );
            value.add(
                "note",
                text("Other findings are unavailable because normal analysis failed."),
            );
            value.print(arguments.flag("--json"))?;
            return Ok(0);
        }
        Err(error) => return Err(error.into()),
    };
    if arguments.flag("--progress") || status.lock().unwrap().2 {
        eprintln!(
            "dotheap: opened in {:.1} s{}",
            started.elapsed().as_secs_f64(),
            if snapshot.from_cache {
                format!(
                    " from the cache {}",
                    snapshot
                        .cache_directory
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            } else {
                String::new()
            }
        );
    }
    for warning in snapshot.versions().warnings {
        eprintln!("dotheap: warning: {warning}");
    }
    if arguments.command == "report" {
        let text = report::report(&snapshot, &arguments);
        snapshot.dump.check()?;
        writeln!(io::stdout().lock(), "{}", text?)?;
    } else {
        let value = commands::run(&Context {
            snapshot: &snapshot,
            arguments: &arguments,
        });
        snapshot.dump.check()?;
        value?.print(arguments.flag("--json"))?;
    }
    Ok(0)
}

fn verification_output(result: &heap_core::verification::HeapVerification, limit: usize) -> Value {
    record([
        ("objects", int(result.objects)),
        ("methodTables", int(result.method_tables)),
        ("referenceSlots", int(result.reference_slots)),
        ("unparsedBytes", bytes(result.unparsed_bytes)),
        ("invalid", int(result.invalid)),
        ("missingMemory", int(result.missing_memory)),
        ("unverified", int(result.unverified)),
        (
            "omitted",
            int(result.invalid + result.missing_memory + result.unverified
                - result.diagnostics.len().min(limit)),
        ),
        (
            "diagnostics",
            limited(&result.diagnostics, limit, |d| {
                record([
                    ("kind", text(format!("{:?}", d.kind))),
                    ("structure", text(&d.structure)),
                    ("address", address(d.address)),
                    ("object", optional_address(d.object)),
                    ("value", optional_address(d.value)),
                    ("message", text(&d.message)),
                ])
            }),
        ),
        ("limitations", list(result.limitations.iter().map(text))),
    ])
}

fn cache_command(arguments: &Arguments) -> Result<Value, CliError> {
    let mut entries = cache::analysis_entries(true);
    if let Some(what) = arguments.value("--delete") {
        let chosen: Vec<_> = entries
            .iter()
            .filter(|e| {
                what == "all"
                    || what == "missing" && e.dump_exists == Some(false)
                    || e.directories[0].file_name().is_some_and(|n| n == what)
            })
            .collect();
        if !["all", "missing"].contains(&what) && chosen.is_empty() {
            return Err(CliError {
                message: format!(
                    "no folder {what} in {}; see dotheap cache",
                    cache::root().display()
                ),
                status: 2,
            });
        }
        for entry in &chosen {
            cache::remove(entry)?;
        }
        eprintln!(
            "dotheap: deleted {} folders, {}",
            chosen.len(),
            size(chosen.iter().map(|e| e.bytes).sum())
        );
        entries = cache::analysis_entries(true);
    }
    let other = [
        ("live", "dumps captured from running processes"),
        ("symbols", "downloaded native debug symbols"),
    ];
    Ok(record([
        ("root", text(cache::root().to_string_lossy())),
        ("bytes", bytes(entries.iter().map(|e| e.bytes).sum())),
        (
            "dumps",
            limited(entries, arguments.limit(), |e| {
                record([
                    ("bytes", bytes(e.bytes)),
                    ("lastOpened", text(timestamp(e.last_opened))),
                    (
                        "dump",
                        text(
                            e.dump_path
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or(e.name),
                        ),
                    ),
                    (
                        "dumpExists",
                        e.dump_exists
                            .map(Value::Bool)
                            .unwrap_or_else(|| text("not recorded")),
                    ),
                    ("processed", Value::Bool(e.processed)),
                    (
                        "folder",
                        text(
                            e.directories[0]
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                        ),
                    ),
                ])
            }),
        ),
        (
            "otherFolders",
            list(other.into_iter().map(|(name, contents)| {
                record([
                    ("folder", text(name)),
                    ("holds", text(contents)),
                    ("bytes", bytes(cache::disk_bytes(&cache::root().join(name)))),
                ])
            })),
        ),
    ]))
}

fn timestamp(seconds: f64) -> String {
    let seconds = seconds as _;
    let mut time: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    let valid = !unsafe { libc::gmtime_r(&seconds, &mut time) }.is_null();
    #[cfg(windows)]
    let valid = unsafe { libc::gmtime_s(&mut time, &seconds) } == 0;
    if !valid {
        return "unknown".into();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        time.tm_year + 1900,
        time.tm_mon + 1,
        time.tm_mday,
        time.tm_hour,
        time.tm_min,
        time.tm_sec
    )
}
