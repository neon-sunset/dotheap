#[cfg(target_os = "linux")]
mod core_file;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod mappings;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
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
}

impl CaptureError {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            runtime_can_write: true,
        }
    }

    pub fn terminal(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            runtime_can_write: false,
        }
    }

    #[cfg(target_os = "linux")]
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
pub const SUPPORTED: bool = cfg!(target_os = "linux");

#[derive(Default)]
pub struct CaptureOptions<'a> {
    pub progress: Option<&'a (dyn Fn(&str) + Sync)>,
    pub status: Option<&'a (dyn Fn(Status) + Sync)>,
    pub cancelled: Option<&'a (dyn Fn() -> bool + Sync)>,
}

#[cfg(target_os = "linux")]
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, path, options);
        Err(CaptureError::new("live capture requires Linux"))
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
