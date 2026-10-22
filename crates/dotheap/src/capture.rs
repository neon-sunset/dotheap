use crate::CliError;
use dotheap::{arguments::Arguments, output::size};
use heap_core::{
    Error,
    live_dump::{self, CaptureOptions, Status},
    live_process,
};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub fn capture(pid: &str, arguments: &Arguments) -> Result<PathBuf, CliError> {
    let pid: i32 = pid
        .parse()
        .ok()
        .filter(|&pid| pid > 0)
        .ok_or_else(|| CliError {
            message: format!("{pid} is not a process id"),
            status: 2,
        })?;
    let progress = ProgressLine::new();
    if arguments.flag("--offline") {
        let process = live_process::list()
            .into_iter()
            .find(|p| p.pid == pid)
            .ok_or_else(|| {
                Error(format!(
                    "process {pid} has no diagnostics socket in the temporary folder \
                     (not a .NET process, diagnostics disabled with DOTNET_EnableDiagnostics=0, \
                     or another TMPDIR); see dotheap ps"
                ))
            })?;
        let path = output_path(arguments, &live_process::file_name(&process))?
            .map(Ok)
            .unwrap_or_else(|| live_process::default_dump_path(&process))?;
        let compression = if path.extension().is_some_and(|e| e == "zst") {
            " Compression starts after it resumes."
        } else {
            ""
        };
        progress.line(&format!(
            "requesting a runtime dump of process {pid} ({}) to {}\n  \
             The process stays paused until the raw dump is written.{compression}",
            process.name,
            path.display()
        ));
        let started = Instant::now();
        live_process::write_dump(&process, &path, |done, total| {
            progress.show(Status {
                phase: "compressing dump".into(),
                done,
                total,
                done_per_second: 0.,
                written_per_second: 0.,
                seconds: started.elapsed().as_secs_f64(),
            })
        })?;
        progress.finish();
        if let Err(error) = live_dump::save_smaps(pid, &path) {
            eprintln!("dotheap: could not save smaps: {error}");
        }
        Ok(path)
    } else {
        if !live_dump::SUPPORTED {
            return Err(Error(
                "live capture requires Linux; pass --offline to request a .NET runtime dump.\n\
                 Runtime capture pauses the process until the raw dump is written."
                    .into(),
            )
            .into());
        }
        let default = live_dump::default_path(pid)?;
        let name = default.file_name().unwrap_or_default().to_string_lossy();
        let path = output_path(arguments, &name)?.unwrap_or(default);
        let _signals = CaptureSignals::install()?;
        progress.line(&format!("capturing process {pid} to {}", path.display()));
        let line = |text: &str| progress.line(text);
        let status = |status| progress.show(status);
        let cancelled = || CANCELLED.load(Ordering::Relaxed);
        let options = CaptureOptions {
            progress: Some(&line),
            status: Some(&status),
            cancelled: Some(&cancelled),
        };
        let result = live_dump::write(pid, &path, &options);
        progress.finish();
        match result {
            Ok(result) => Ok(result.path),
            Err(error) => {
                let retry = if arguments.command == "dump" {
                    format!("run dotheap dump {pid} --offline")
                } else {
                    "add --offline".into()
                };
                let message = if error.runtime_can_write {
                    format!(
                        "{error}\nTo use .NET runtime capture, {retry}\n\
                         The process stays paused until the raw dump is written."
                    )
                } else {
                    error.to_string()
                };
                Err(CliError { message, status: 1 })
            }
        }
    }
}

fn output_path(arguments: &Arguments, name: &str) -> io::Result<Option<PathBuf>> {
    arguments
        .value("--out")
        .map(|out| {
            let out = Path::new(out);
            std::path::absolute(if out.is_dir() {
                out.join(name)
            } else {
                out.into()
            })
        })
        .transpose()
}

struct ProgressLine {
    terminal: bool,
    state: Mutex<ProgressState>,
}
struct ProgressState {
    printed: Option<Instant>,
    shown: bool,
    finished: bool,
}
impl ProgressLine {
    fn new() -> Self {
        Self {
            terminal: io::stderr().is_terminal(),
            state: Mutex::new(ProgressState {
                printed: None,
                shown: false,
                finished: false,
            }),
        }
    }
    fn show(&self, status: Status) {
        let mut state = self.state.lock().unwrap();
        if state.finished
            || !self.terminal
                && state
                    .printed
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        state.printed = Some(Instant::now());
        let mut text = status.phase;
        if status.total > 0 {
            let average = if status.seconds > 0. {
                status.done as f64 / status.seconds
            } else {
                0.
            };
            text += &format!(
                ": {} of {} ({}%), {}/s",
                size(status.done),
                size(status.total),
                (100. * status.done as f64 / status.total as f64) as u64,
                size(status.done_per_second as u64)
            );
            let left = status.total.saturating_sub(status.done) as f64 / average;
            if average > 0. && left >= 1. {
                text += &if left >= 90. {
                    format!(", about {:.0} min left", left / 60.)
                } else {
                    format!(", about {left:.0} s left")
                };
            }
        } else {
            text += &format!(" ({:.0} s)", status.seconds);
        }
        text += &format!(", writing {}/s", size(status.written_per_second as u64));
        let mut stderr = io::stderr().lock();
        if self.terminal {
            let _ = write!(stderr, "\r\x1b[K{text}");
        } else {
            let _ = writeln!(stderr, "dotheap: {text}");
        }
        state.shown = self.terminal;
    }
    fn line(&self, text: &str) {
        let mut state = self.state.lock().unwrap();
        let _ = writeln!(
            io::stderr(),
            "{}dotheap: {text}",
            if state.shown { "\r\x1b[K" } else { "" }
        );
        state.shown = false;
    }
    fn finish(&self) {
        let mut state = self.state.lock().unwrap();
        if state.shown {
            let _ = write!(io::stderr(), "\r\x1b[K");
        }
        state.shown = false;
        state.finished = true;
    }
}
impl Drop for ProgressLine {
    fn drop(&mut self) {
        self.finish();
    }
}

static CANCELLED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}
#[cfg(unix)]
struct CaptureSignals(Vec<(i32, libc::sigaction)>);
#[cfg(unix)]
impl CaptureSignals {
    fn install() -> io::Result<Self> {
        CANCELLED.store(false, Ordering::Relaxed);
        let mut guard = Self(Vec::new());
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = cancel as *const () as usize;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            guard.0.push((signal, previous));
        }
        Ok(guard)
    }
}
#[cfg(unix)]
impl Drop for CaptureSignals {
    fn drop(&mut self) {
        for (signal, action) in &self.0 {
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(windows)]
struct CaptureSignals;
#[cfg(windows)]
impl CaptureSignals {
    fn install() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "live capture requires Linux",
        ))
    }
}
