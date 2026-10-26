use crate::{
    Error, Result,
    analysis::HeapSnapshot,
    cache::CacheGuard,
    compressed::atomic_write,
    runtime::elf_build_id,
    symbols::{self, ElfFile},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const DEFAULT_SERVER: &str = "https://msdl.microsoft.com/download/symbols";
const NOT_ON_SERVER: &str = "not on the symbol server";
const PREFIXES: [&str; 11] = [
    "libcoreclr",
    "libclrjit",
    "libclrgc",
    "libSystem.",
    "libhostfxr",
    "libhostpolicy",
    "libmscordaccore",
    "libmscordbi",
    "libcoreclrtraceptprovider",
    "libjitinterface",
    "createdump",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDownload {
    pub library: String,
    pub build_id: String,
    pub result: String,
    pub path: Option<PathBuf>,
}

impl HeapSnapshot {
    pub fn download_runtime_symbols(
        &self,
        server: &str,
        automatic: bool,
        cancelled: impl Fn() -> bool,
        progress: impl Fn(&str, f64),
    ) -> Vec<SymbolDownload> {
        if !self.dump.platform.is_linux() {
            return Vec::new();
        }
        let index = symbols::index(&self.dll_folders);
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        let server = server.trim_end_matches('/');
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(20)))
            .build()
            .new_agent();
        for file in self.dump.files.iter().filter(|f| f.file_offset == 0) {
            let name = symbols::file_name(Path::new(&file.path));
            if !PREFIXES.iter().any(|p| name.starts_with(p)) || !seen.insert(file.path.clone()) {
                continue;
            }
            let Some(id) = elf_build_id(&self.dump, file.start) else {
                continue;
            };
            let mut result = SymbolDownload {
                library: name.clone(),
                build_id: id.clone(),
                result: String::new(),
                path: None,
            };
            if let Some(path) = symbols::debug_paths(&name, &id, &file.path, &index)
                .into_iter()
                .find(|path| matches_build(path, &id))
            {
                result.result = "already there".into();
                result.path = Some(path);
                results.push(result);
                continue;
            }
            if cancelled() {
                break;
            }
            let folder = symbols::symbol_folder().join(&id);
            let target = folder.join(format!("{name}.dbg"));
            let missing = folder.join(format!("{name}.dbg.not-on-server"));
            let guard = CacheGuard::lock(&folder);
            let _guard = match guard {
                Ok(guard) => guard,
                Err(error) => {
                    result.result = error.to_string();
                    results.push(result);
                    continue;
                }
            };
            if matches_build(&target, &id) {
                result.result = "already there".into();
                result.path = Some(target);
                results.push(result);
                continue;
            }
            if automatic && fs::read_to_string(&missing).is_ok_and(|saved| saved == server) {
                result.result = NOT_ON_SERVER.into();
                results.push(result);
                continue;
            }
            if cancelled() {
                break;
            }
            let phase = format!("Downloading the debug file of {name}");
            progress(&phase, 0.0);
            let url = format!("{server}/_.debug/elf-buildid-sym-{id}/_.debug");
            let response = match agent.get(&url).call() {
                Ok(response) if response.status() == 200 => response,
                Ok(response) => {
                    result.result = format!("HTTP status {}", response.status().as_u16());
                    results.push(result);
                    continue;
                }
                Err(ureq::Error::StatusCode(404)) => {
                    let _ = atomic_write(&missing, server.as_bytes());
                    result.result = NOT_ON_SERVER.into();
                    results.push(result);
                    continue;
                }
                Err(ureq::Error::StatusCode(status)) => {
                    result.result = format!("HTTP status {status}");
                    results.push(result);
                    continue;
                }
                Err(error) => {
                    result.result = if cancelled() {
                        "skipped".into()
                    } else {
                        error.to_string()
                    };
                    results.push(result);
                    break;
                }
            };
            match save_response(response, &target, &id, &cancelled, |fraction| {
                progress(&phase, fraction)
            }) {
                Ok(()) => {
                    let _ = fs::remove_file(&missing);
                    result.result = "downloaded".into();
                    result.path = Some(target);
                }
                Err(error) => result.result = error.to_string(),
            }
            results.push(result);
            if cancelled() {
                break;
            }
        }
        results
    }
}

fn matches_build(path: &Path, id: &str) -> bool {
    ElfFile::open(path)
        .and_then(|file| file.build_id())
        .as_deref()
        == Some(id)
}

fn save_response(
    mut response: ureq::http::Response<ureq::Body>,
    target: &Path,
    id: &str,
    cancelled: impl Fn() -> bool,
    progress: impl Fn(f64),
) -> Result<()> {
    let total = response.body().content_length().unwrap_or(0);
    let mut input = response.body_mut().as_reader();
    let parent = target
        .parent()
        .ok_or_else(|| Error("the symbol path has no parent folder".into()))?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    let mut buffer = vec![0; 256 << 10];
    let mut done = 0u64;
    loop {
        if cancelled() {
            return Err(Error("skipped".into()));
        }
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        done += count as u64;
        progress(if total == 0 {
            0.0
        } else {
            (done as f64 / total as f64).min(1.0)
        });
    }
    if !matches_build(output.path(), id) {
        return Err(Error(
            "the downloaded debug file does not match the library's ELF build ID".into(),
        ));
    }
    if cancelled() {
        return Err(Error("skipped".into()));
    }
    output.as_file().sync_all()?;
    output
        .persist(target)
        .map_err(|e| Error::open(target.display(), e.error))?;
    progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufRead,
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn debug_elf() -> Vec<u8> {
        let mut bytes = vec![0; 140];
        bytes[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&4u32.to_le_bytes());
        bytes[72..80].copy_from_slice(&120u64.to_le_bytes());
        bytes[80..88].copy_from_slice(&120u64.to_le_bytes());
        bytes[96..104].copy_from_slice(&20u64.to_le_bytes());
        bytes[120..124].copy_from_slice(&4u32.to_le_bytes());
        bytes[124..128].copy_from_slice(&4u32.to_le_bytes());
        bytes[128..132].copy_from_slice(&3u32.to_le_bytes());
        bytes[132..136].copy_from_slice(b"GNU\0");
        bytes[136..140].copy_from_slice(&[1, 2, 3, 4]);
        bytes
    }

    #[test]
    fn downloads_require_matching_build_ids_and_preserve_existing_files_on_failure() {
        for (id, cancel, truncated) in [
            ("01020304", false, false),
            ("deadbeef", false, false),
            ("01020304", true, false),
            ("01020304", false, true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let target = directory.path().join("library.dbg");
            fs::write(&target, b"previous symbols").unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/_.debug", listener.local_addr().unwrap());
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut input = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert_eq!(line, "GET /_.debug HTTP/1.1\r\n");
                loop {
                    line.clear();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                let bytes = debug_elf();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len() + usize::from(truncated)
                )
                .unwrap();
                socket.write_all(&bytes).unwrap();
            });
            let response = ureq::get(&url).call().unwrap();
            let calls = AtomicUsize::new(0);
            let result = save_response(
                response,
                &target,
                id,
                || cancel && calls.fetch_add(1, Ordering::Relaxed) > 0,
                |_| {},
            );
            worker.join().unwrap();
            if id == "01020304" && !cancel && !truncated {
                result.unwrap();
                assert_eq!(fs::read(&target).unwrap(), debug_elf());
                assert!(matches_build(&target, id));
            } else {
                assert!(result.is_err());
                assert_eq!(fs::read(&target).unwrap(), b"previous symbols");
            }
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }
}
