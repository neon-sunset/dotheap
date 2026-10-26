#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
mod core_file;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(target_os = "linux")]
mod mappings;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
mod meter;
#[cfg(target_os = "linux")]
mod notes;
#[cfg(target_os = "linux")]
mod page_store;
#[cfg(target_os = "linux")]
mod thread_stop;

use serde::{Deserialize, Serialize};
use std::{
    fmt, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub copied_again: u64,
    pub pause: f64,
    pub threads: usize,
    pub soft_dirty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub done_per_second: f64,
    pub written_per_second: f64,
    pub seconds: f64,
}

#[derive(Clone, Debug)]
pub struct CaptureError {
    pub description: String,
    pub runtime_can_write: bool,
    pub pause_can_capture: bool,
}

impl CaptureError {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            runtime_can_write: true,
            pause_can_capture: false,
        }
    }

    pub fn terminal(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            runtime_can_write: false,
            pause_can_capture: false,
        }
    }

    #[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
    fn system(description: impl fmt::Display) -> Self {
        Self::new(format!("{description}: {}", io::Error::last_os_error()))
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.description.fmt(f)
    }
}
impl std::error::Error for CaptureError {}
impl From<io::Error> for CaptureError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}
impl From<crate::Error> for CaptureError {
    fn from(error: crate::Error) -> Self {
        Self::new(error.0)
    }
}

pub type Result<T> = std::result::Result<T, CaptureError>;
pub const SUPPORTED: bool = cfg!(any(
    target_os = "linux",
    all(target_os = "macos", target_arch = "aarch64")
));

#[derive(Default)]
pub struct CaptureOptions<'a> {
    pub pause: bool,
    pub progress: Option<&'a (dyn Fn(&str) + Sync)>,
    pub status: Option<&'a (dyn Fn(Status) + Sync)>,
    pub cancelled: Option<&'a (dyn Fn() -> bool + Sync)>,
}

#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
impl CaptureOptions<'_> {
    fn report(&self, text: impl AsRef<str>) {
        if let Some(progress) = self.progress {
            progress(text.as_ref());
        }
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.is_some_and(|cancelled| cancelled()) {
            Err(CaptureError::terminal("capture cancelled"))
        } else {
            Ok(())
        }
    }
}

pub fn default_path(pid: i32) -> crate::Result<PathBuf> {
    let folder = crate::cache::root().join("live");
    crate::column::create_directory(&folder)?;
    Ok(folder.join(crate::live_process::file_name_for(
        pid,
        &crate::live_process::command_line(pid),
    )))
}

pub fn write(pid: i32, path: &Path, options: &CaptureOptions<'_>) -> Result<CaptureResult> {
    if options.pause && !cfg!(target_os = "macos") {
        return Err(CaptureError::terminal(
            "paused capture is available only on macOS",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        // Kernel ptrace ownership ends with this thread, including tracees that cannot enter a stop.
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("dotheap-capture".into())
                .spawn_scoped(scope, || linux::write(pid, path, options))?
                .join()
                .map_err(|_| CaptureError::terminal("the capture worker failed"))?
        })
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        macos::write(pid, path, options)
    }
    #[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
    {
        let _ = (pid, path, options);
        Err(CaptureError::new(
            "direct capture requires Linux or macOS on Apple silicon",
        ))
    }
}

pub fn save_smaps(pid: i32, dump: &Path) -> crate::Result<()> {
    #[cfg(target_os = "linux")]
    {
        save_smaps_bytes(&std::fs::read(format!("/proc/{pid}/smaps"))?, dump)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, dump);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn save_smaps_bytes(bytes: &[u8], dump: &Path) -> crate::Result<()> {
    use std::io::Write;
    let path = crate::smaps::path_for_dump(dump);
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    output.write_all(&zstd::bulk::compress(bytes, 1)?)?;
    output.as_file().sync_all()?;
    output
        .persist_noclobber(&path)
        .map_err(|e| crate::Error::open(path.display(), e.error))?;
    Ok(())
}
