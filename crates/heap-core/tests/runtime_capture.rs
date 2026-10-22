#![cfg(unix)]

use heap_core::{
    bytes::ReadBytes,
    live_process::{self, Process},
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::mpsc,
    time::Duration,
};

#[test]
fn runtime_capture_publishes_private_files_and_preserves_recovery_data() {
    for (compressed, keep_open) in [(false, false), (false, true), (true, false), (true, true)] {
        for existing in [false, true] {
            for race in [false, true] {
                if existing && race {
                    continue;
                }
                let folder = tempfile::tempdir().unwrap();
                let output = folder.path().join(if compressed {
                    "capture.dmp.zst"
                } else {
                    "capture.dmp"
                });
                let socket = folder.path().join("diagnostic-socket");
                let server = UnixListener::bind(&socket).unwrap();
                let process = Process {
                    pid: std::process::id() as i32,
                    name: "test".into(),
                    socket,
                    uid: unsafe { libc::geteuid() },
                    gid: unsafe { libc::getegid() },
                };
                if existing {
                    fs::write(&output, b"existing output").unwrap();
                    assert!(live_process::write_dump(&process, &output, |_, _| {}).is_err());
                    assert_eq!(fs::read(&output).unwrap(), b"existing output");
                    continue;
                }
                let (written, receiver) = mpsc::channel();
                let (finished, finish) = mpsc::channel();
                let out = output.clone();
                let worker = std::thread::spawn(move || {
                    let (mut socket, _) = server.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut header = [0; 20];
                    socket.read_exact(&mut header).unwrap();
                    assert_eq!(&header[..14], b"DOTNET_IPC_V1\0");
                    assert_eq!(&header[16..20], &[1, 3, 0, 0]);
                    let mut payload = vec![0; header.u16(14).unwrap() as usize - 20];
                    socket.read_exact(&mut payload).unwrap();
                    let length = payload.u32(0).unwrap() as usize;
                    let units: Vec<_> = payload[4..4 + (length - 1) * 2]
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    let raw = std::path::PathBuf::from(String::from_utf16(&units).unwrap());
                    assert_ne!(raw, out);
                    fs::write(&raw, b"captured memory").unwrap();
                    if race {
                        fs::write(out, b"concurrent output").unwrap();
                    }
                    written.send(raw).unwrap();
                    let mut response = b"DOTNET_IPC_V1\0".to_vec();
                    response.extend_from_slice(&[24, 0, 255, 0, 0, 0, 0, 0, 0, 0]);
                    if keep_open {
                        for byte in response {
                            socket.write_all(&[byte]).unwrap();
                        }
                        finish
                            .recv_timeout(Duration::from_secs(5))
                            .expect("client waited for EOF instead of the complete response");
                    } else {
                        socket.write_all(&response).unwrap();
                    }
                });
                let result = live_process::write_dump(&process, &output, |_, _| {});
                if keep_open {
                    finished.send(()).unwrap();
                }
                worker.join().unwrap();
                let raw = receiver.recv().unwrap();
                if race {
                    let error = result.unwrap_err();
                    assert!(error.0.contains(&raw.to_string_lossy().to_string()));
                    assert_eq!(fs::read(&raw).unwrap(), b"captured memory");
                    assert_eq!(fs::read(&output).unwrap(), b"concurrent output");
                } else {
                    result.unwrap();
                    let bytes = fs::read(&output).unwrap();
                    let bytes = if compressed {
                        zstd::stream::decode_all(bytes.as_slice()).unwrap()
                    } else {
                        bytes
                    };
                    assert_eq!(bytes, b"captured memory");
                    assert_eq!(
                        output.metadata().unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                    assert!(!raw.exists());
                }
            }
        }
    }
}
