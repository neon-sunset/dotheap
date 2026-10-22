use super::{Process, file_name};
use crate::{Error, Result, bytes::ReadBytes, cache, compressed};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

const MAGIC: &[u8; 14] = b"DOTNET_IPC_V1\0";
const HEADER_SIZE: usize = 20;
pub const DUMP_DEADLINE: Duration = Duration::from_secs(30 * 60);

pub fn list() -> Vec<Process> {
    let mut folders = vec![PathBuf::from("/tmp")];
    if let Some(folder) = std::env::var_os("TMPDIR").filter(|s| !s.is_empty()) {
        folders.push(folder.into());
    }
    folders.sort();
    folders.dedup();
    let mut processes = BTreeMap::new();
    for folder in folders {
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(pid) = socket_pid(name) else {
                continue;
            };
            if unsafe { libc::kill(pid, 0) } != 0
                && io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
            {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !metadata.file_type().is_socket() {
                continue;
            }
            let time = metadata.modified().unwrap_or(UNIX_EPOCH);
            let old = processes.entry(pid).or_insert_with(|| {
                (
                    time,
                    Process {
                        pid,
                        name: command_line(pid),
                        socket: entry.path(),
                        uid: metadata.uid(),
                        gid: metadata.gid(),
                    },
                )
            });
            if time > old.0 {
                old.0 = time;
                old.1.socket = entry.path();
                old.1.uid = metadata.uid();
                old.1.gid = metadata.gid();
            }
        }
    }
    processes
        .into_values()
        .map(|(_, process)| process)
        .collect()
}

fn socket_pid(name: &str) -> Option<i32> {
    let body = name
        .strip_prefix("dotnet-diagnostic-")?
        .strip_suffix("-socket")?;
    let (pid, key) = body.split_once('-')?;
    let pid = pid.parse().ok()?;
    (pid > 0 && !key.is_empty()).then_some(pid)
}

pub fn default_dump_path(process: &Process) -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let folder = if process.uid == uid {
        let folder = cache::root().join("live");
        crate::column::create_directory(&folder)?;
        folder
    } else if uid == 0 {
        let folder = tempfile::Builder::new()
            .prefix("dotheap-")
            .tempdir_in("/tmp")?;
        set_owner(&File::open(folder.path())?, process)?;
        folder.keep()
    } else {
        return Err(Error(format!(
            "process {} runs as user {}; run dotheap as that user or as root",
            process.pid, process.uid
        )));
    };
    Ok(folder.join(file_name(process)))
}

pub fn command_line(pid: i32) -> String {
    let args = process_arguments(pid);
    let Some(first) = args.first() else {
        return format!("pid {pid}");
    };
    let program = Path::new(first)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if program == "dotnet"
        && let Some(argument) = args.get(1)
    {
        return format!(
            "dotnet {}",
            Path::new(argument)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
    }
    program.into_owned()
}

#[cfg(target_os = "linux")]
fn process_arguments(pid: i32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

#[cfg(target_os = "macos")]
fn process_arguments(pid: i32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || !(4..=16 << 20).contains(&size)
    {
        return Vec::new();
    }
    let mut data = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            data.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < 4
    {
        return Vec::new();
    }
    let count = data.u32(0).unwrap_or(0) as usize;
    data[4..size]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .skip(1)
        .take(count)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

pub fn write_dump(process: &Process, path: &Path, progress: impl FnMut(u64, u64)) -> Result<()> {
    let path = std::path::absolute(path)?;
    ensure_absent(&path)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error("the dump path has no parent folder".into()))?;
    let directory = tempfile::Builder::new()
        .prefix(".dotheap-")
        .tempdir_in(parent)?;
    set_owner(&File::open(directory.path())?, process)?;
    let raw = directory.path().join("capture.dmp");
    let mut in_progress = false;
    let mut capture = || -> Result<File> {
        request_dump(process, &raw, DUMP_DEADLINE, &mut in_progress)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&raw)
            .map_err(|e| Error::open(raw.display(), e))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != process.uid {
            return Err(Error(
                "the runtime did not create a regular dump file owned by its user".into(),
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    };
    let publish = |mut file: File, progress| -> Result<()> {
        if path.extension().is_some_and(|e| e == "zst") {
            compressed::compress_file(file, &path, progress)
        } else {
            let mut output = tempfile::NamedTempFile::new_in(parent)?;
            io::copy(&mut file, &mut output)?;
            output.as_file().sync_all()?;
            output
                .persist_noclobber(&path)
                .map_err(|e| Error::open(path.display(), e.error))?;
            Ok(())
        }
    };
    if let Err(error) = capture().and_then(|file| publish(file, progress)) {
        if raw.symlink_metadata().is_ok() {
            let _ = directory.keep();
            return Err(Error(format!(
                "{error}; the uncompressed dump is {}",
                raw.display()
            )));
        }
        if in_progress {
            let _ = directory.keep();
            return Err(Error(format!(
                "{error}; the runtime may still be writing {}; its capture folder was kept",
                raw.display()
            )));
        }
        return Err(error);
    }
    Ok(())
}

fn set_owner(file: &File, process: &Process) -> Result<()> {
    let metadata = file.metadata()?;
    if metadata.uid() == process.uid && metadata.gid() == process.gid {
        return Ok(());
    }
    if unsafe { libc::fchown(file.as_raw_fd(), process.uid, process.gid) } != 0 {
        return Err(Error(format!(
            "cannot give the capture folder to user {}: {}",
            process.uid,
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub(crate) fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(Error(format!(
            "output already exists or cannot be checked: {}",
            path.display()
        ))),
    }
}

fn request_dump(
    process: &Process,
    path: &Path,
    timeout: Duration,
    in_progress: &mut bool,
) -> Result<()> {
    let message = dump_request(path)?;
    let mut socket = UnixStream::connect(&process.socket).map_err(|error| {
        if error.kind() == io::ErrorKind::PermissionDenied {
            Error(format!(
                "no access to the diagnostics socket of process {} ({error}); run dotheap as the \
                user that runs it, or as root",
                process.pid
            ))
        } else {
            Error(format!(
                "cannot connect to process {} at {}: {error}",
                process.pid,
                process.socket.display()
            ))
        }
    })?;
    socket.set_write_timeout(Some(timeout))?;
    // Darwin rejects setsockopt after the peer closes, even with unread response bytes.
    socket.set_read_timeout(Some(timeout.min(Duration::from_secs(1))))?;
    let deadline = Instant::now() + timeout;
    socket.write_all(&message)?;
    *in_progress = true;
    let mut read = |bytes: &mut [u8]| -> Result<()> {
        let mut done = 0;
        while done < bytes.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error(format!(
                    "the runtime did not answer within {} minutes; createdump may still be writing {}",
                    timeout.as_secs() / 60,
                    path.display()
                )));
            }
            match socket.read(&mut bytes[done..]) {
                Ok(0) => {
                    return Err(Error(
                        "the runtime closed its diagnostics socket before completing the response"
                            .into(),
                    ));
                }
                Ok(count) => done += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(Error(format!(
                        "cannot read the runtime's response: {error}"
                    )));
                }
            }
        }
        Ok(())
    };
    let mut header = [0; HEADER_SIZE];
    read(&mut header)?;
    if &header[..14] != MAGIC || header[16] != 0xff || header[18..20] != [0, 0] {
        return Err(Error(
            "the runtime answered with an invalid diagnostics IPC header".into(),
        ));
    }
    let size = header.u16(14).unwrap() as usize;
    if size < HEADER_SIZE + 4 {
        return Err(Error(
            "the runtime's dump response has no result code".into(),
        ));
    }
    let mut payload = vec![0; size - HEADER_SIZE];
    read(&mut payload)?;
    *in_progress = false;
    decode_response(header[17], &payload)
}

fn dump_request(path: &Path) -> Result<Vec<u8>> {
    let path = path
        .to_str()
        .ok_or_else(|| Error("the runtime requires a UTF-8 dump path".into()))?;
    if path.contains('\0') {
        return Err(Error("the dump path contains a NUL byte".into()));
    }
    let units: Vec<_> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let size = HEADER_SIZE + 12 + units.len() * 2;
    let size = u16::try_from(size).map_err(|_| Error("the dump path is too long".into()))?;
    let mut message = Vec::with_capacity(size as usize);
    message.extend_from_slice(MAGIC);
    message.extend_from_slice(&size.to_le_bytes());
    message.extend_from_slice(&[1, 3, 0, 0]);
    message.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        message.extend_from_slice(&unit.to_le_bytes());
    }
    message.extend_from_slice(&4u32.to_le_bytes());
    message.extend_from_slice(&0u32.to_le_bytes());
    Ok(message)
}

fn decode_response(command: u8, payload: &[u8]) -> Result<()> {
    let code = payload
        .u32(0)
        .ok_or_else(|| Error("the runtime's dump response has no result code".into()))?;
    if command == 0 && code == 0 && payload.len() == 4 {
        return Ok(());
    }
    if command > 1 || command == 0 && code == 0 {
        return Err(Error(
            "the runtime answered with an invalid dump response".into(),
        ));
    }
    let mut text = String::new();
    if payload.len() > 4 {
        let length = payload
            .u32(4)
            .ok_or_else(|| Error("the runtime's error message is truncated".into()))?
            as usize;
        let bytes = length
            .checked_mul(2)
            .and_then(|n| payload.get(8..8 + n))
            .ok_or_else(|| Error("the runtime's error message is truncated".into()))?;
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if !units.is_empty() && units.last() != Some(&0) {
            return Err(Error(
                "the runtime's error message has no terminator".into(),
            ));
        }
        text = String::from_utf16_lossy(&units[..units.len().saturating_sub(1)])
            .trim()
            .into();
    }
    let mut hint = String::new();
    if let Some((_, rest)) = text.split_once("execve(")
        && let Some((program, _)) = rest.split_once(')')
        && let Ok(metadata) = fs::metadata(program)
        && metadata.is_file()
        && metadata.permissions().mode() & 0o111 == 0
    {
        hint = format!(
            ". {program} is not executable; give the process's user execute permission so the runtime can start it"
        );
    }
    Err(Error(format!(
        "the runtime could not write the dump (0x{code:x}){}{hint}",
        if text.is_empty() {
            String::new()
        } else {
            format!(": {text}")
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_request_uses_utf16_full_dump_and_generate_core_dump_3() {
        let request = dump_request(Path::new("/tmp/λ🦀.dmp")).unwrap();
        assert_eq!(&request[..14], MAGIC);
        assert_eq!(request.u16(14).unwrap() as usize, request.len());
        assert_eq!(&request[16..20], &[1, 3, 0, 0]);
        let count = request.u32(20).unwrap() as usize;
        let units: Vec<_> = request[24..24 + count * 2]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "/tmp/λ🦀.dmp\0");
        assert_eq!(&request[24 + count * 2..], &[4, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn runtime_success_requires_a_complete_success_response() {
        assert!(decode_response(0, &[0, 0, 0, 0]).is_ok());
        for (command, data) in [
            (0, vec![]),
            (0, vec![1, 0, 0, 0]),
            (1, vec![0; 4]),
            (2, vec![0; 4]),
            (0, vec![0; 8]),
            (1, vec![1, 0, 0, 0, 255, 255, 255, 255]),
        ] {
            assert!(decode_response(command, &data).is_err());
        }
    }
}
