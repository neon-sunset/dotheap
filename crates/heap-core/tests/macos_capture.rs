#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use heap_core::{
    core_dump::CoreDump,
    live_dump::{self, CaptureOptions},
    live_process,
};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

struct Target {
    child: Child,
    lines: Receiver<String>,
}
impl Target {
    fn start(folder: &Path) -> Self {
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Tests/LiveDump/macos-target.c");
        let executable = folder.join("target");
        assert!(
            Command::new("clang")
                .args(["-O2", "-pthread"])
                .arg(source)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        let entitlements = folder.join("entitlements.plist");
        std::fs::write(&entitlements, "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>com.apple.security.get-task-allow</key><true/></dict></plist>").unwrap();
        assert!(
            Command::new("codesign")
                .args(["--force", "--sign", "-", "--entitlements"])
                .arg(entitlements)
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        let mut child = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }
    fn line(&self) -> String {
        self.lines
            .recv_timeout(Duration::from_secs(10))
            .expect("the target must respond")
    }
    fn wait_for_progress(&mut self, before: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.round() <= before {
            assert!(Instant::now() < deadline, "the writer must resume");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn round(&mut self) -> u64 {
        writeln!(self.child.stdin.as_mut().unwrap(), "ping").unwrap();
        self.line().strip_prefix("alive ").unwrap().parse().unwrap()
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn macos_captures_one_moment_and_resumes_after_cancellation() {
    let folder = tempfile::tempdir().unwrap();
    let mut target = Target::start(folder.path());
    let line = target.line();
    let (address, count) = line.split_once(' ').unwrap();
    let address = u64::from_str_radix(address.trim_start_matches("0x"), 16).unwrap();
    let count: usize = count.parse().unwrap();
    let pid = target.child.id() as i32;
    let process = live_process::list()
        .into_iter()
        .find(|p| p.pid == pid)
        .unwrap();
    assert!(process.socket.is_none());
    assert!(
        process
            .memory_bytes
            .is_some_and(|bytes| bytes >= (count * 8) as u64)
    );
    for pause in [false, true] {
        let output = tempfile::tempdir_in(folder.path()).unwrap();
        let path = output.path().join("capture.dmp.zst");
        let before = target.round();
        let result = live_dump::write(
            pid,
            &path,
            &CaptureOptions {
                pause,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.threads, 2);
        target.wait_for_progress(before);
        let cache = output.path().join("cache");
        let dump = CoreDump::open(&path, Some(&cache), None).unwrap();
        let residency = dump.residency.as_ref().expect("capture includes residency");
        assert_eq!(residency.version, 1);
        assert!(
            residency
                .physical_footprint
                .is_some_and(|bytes| bytes >= (count * 8) as u64)
        );
        let mapping = residency
            .mappings
            .iter()
            .find(|m| m.start <= address && m.end > address)
            .unwrap();
        assert!(mapping.rss >= (count * 8) as u64);
        assert!(mapping.readable);
        assert_eq!(dump.threads.len(), 2);
        assert!(
            dump.threads
                .iter()
                .all(|thread| thread.stack_pointer != 0 && thread.instruction_pointer != 0)
        );
        let bytes = dump.bytes(address, count * 8).unwrap();
        let values: Vec<_> = bytes
            .chunks_exact(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert!(values[count - 1] >= before);
        assert!(values[0] - values[count - 1] <= 1);
        assert!(
            values.windows(2).all(|pair| pair[0] >= pair[1]),
            "the sequential writer's values must come from one moment"
        );
    }
    for pause in [false, true] {
        let output = tempfile::tempdir_in(folder.path()).unwrap();
        let path = output.path().join("cancelled.dmp.zst");
        let checks = AtomicUsize::new(0);
        let cancelled = || checks.fetch_add(1, Ordering::Relaxed) > 0;
        let before = target.round();
        let error = live_dump::write(
            pid,
            &path,
            &CaptureOptions {
                pause,
                cancelled: Some(&cancelled),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.description, "capture cancelled");
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(output.path()).unwrap().count(), 0);
        target.wait_for_progress(before);
    }
}
