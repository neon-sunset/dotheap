use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::ensure_absent;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use unix::memory_bytes;
#[cfg(unix)]
pub use unix::{DUMP_DEADLINE, command_line, default_dump_path, list, write_dump};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{command_line, default_dump_path, list, write_dump};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Process {
    pub pid: i32,
    pub name: String,
    pub socket: Option<PathBuf>,
    pub uid: u32,
    pub gid: u32,
    pub memory_bytes: Option<u64>,
}

pub fn file_name(process: &Process) -> String {
    file_name_for(process.pid, &process.name)
}

pub(crate) fn file_name_for(pid: i32, command: &str) -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}-{pid}-{seconds}.dmp.zst", program_name(command))
}

pub fn program_name(command: &str) -> String {
    let mut words = command.split_whitespace();
    let first = words.next().unwrap_or("process");
    let path = words.find(|word| word.ends_with(".dll"));
    let path = Path::new(path.unwrap_or(first));
    let name = if path.extension().is_some_and(|e| e == "dll") {
        path.file_stem()
    } else {
        path.file_name()
    };
    name.unwrap_or_default().to_string_lossy().into_owned()
}
