use super::Process;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub fn list() -> Vec<Process> {
    Vec::new()
}

pub fn command_line(pid: i32) -> String {
    format!("pid {pid}")
}

pub fn default_dump_path(_: &Process) -> Result<PathBuf> {
    Err(unsupported())
}

pub fn write_dump(_: &Process, _: &Path, _: impl FnMut(u64, u64)) -> Result<()> {
    Err(unsupported())
}

fn unsupported() -> Error {
    Error("dumps of running processes are not supported on Windows".into())
}
