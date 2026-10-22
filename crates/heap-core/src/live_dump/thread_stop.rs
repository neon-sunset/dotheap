use super::{CaptureError, CaptureOptions, Result, memory::MemoryReader};
use std::{
    io,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

#[cfg(target_arch = "x86_64")]
pub(super) const MACHINE: u16 = 62;
#[cfg(target_arch = "aarch64")]
pub(super) const MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const GENERAL_BYTES: usize = 27 * 8;
#[cfg(target_arch = "aarch64")]
const GENERAL_BYTES: usize = 34 * 8;
#[cfg(target_arch = "x86_64")]
const FLOAT_BYTES: usize = 512;
#[cfg(target_arch = "aarch64")]
const FLOAT_BYTES: usize = 528;

struct Thread {
    tid: i32,
    signal: i32,
    stopped: bool,
}

pub(super) struct Registers {
    pub tid: i32,
    pub general: Vec<u8>,
    pub float: Option<Vec<u8>>,
}

pub(super) struct ThreadStop {
    threads: Vec<Thread>,
    // Linux assigns each tracee to the OS thread that called PTRACE_SEIZE.
    same_thread: PhantomData<Rc<()>>,
}

impl ThreadStop {
    pub fn stop(reader: &MemoryReader, options: &CaptureOptions<'_>) -> Result<Self> {
        let mut guard = Self {
            threads: Vec::new(),
            same_thread: PhantomData,
        };
        let mut seen = std::collections::HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            options.check_cancelled()?;
            let tids: Vec<_> = reader
                .thread_ids()?
                .into_iter()
                .filter(|tid| seen.insert(*tid))
                .collect();
            if tids.is_empty() {
                break;
            }
            for tid in tids {
                if let Err(error) = ptrace(libc::PTRACE_SEIZE, tid, 0, 0) {
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        continue;
                    }
                    if error.raw_os_error() == Some(libc::EPERM) {
                        return Err(CaptureError::new(format!(
                            "no permission to stop the threads of process {}: run dotheap as \
                                root, or as its user with /proc/sys/kernel/yama/ptrace_scope set to \
                                0",
                            reader.pid
                        )));
                    }
                    return Err(CaptureError::new(format!(
                        "cannot stop thread {tid}: {error}"
                    )));
                }
                guard.threads.push(Thread {
                    tid,
                    signal: 0,
                    stopped: false,
                });
                if let Err(error) = ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0) {
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        guard.threads.pop();
                        continue;
                    }
                    return Err(CaptureError::new(format!(
                        "cannot interrupt thread {tid}: {error}"
                    )));
                }
                match wait_stopped(tid, deadline, || options.check_cancelled())? {
                    Some(signal) => {
                        let thread = guard.threads.last_mut().unwrap();
                        thread.stopped = true;
                        thread.signal = signal;
                    }
                    None => {
                        guard.threads.pop();
                    }
                }
            }
        }
        if guard.threads.is_empty() {
            return Err(CaptureError::terminal(format!(
                "process {} exited during capture",
                reader.pid
            )));
        }
        guard.threads.sort_by_key(|t| (t.tid != reader.pid, t.tid));
        Ok(guard)
    }

    pub fn registers(&self) -> Result<Vec<Registers>> {
        self.threads
            .iter()
            .map(|thread| {
                let general = register_set(thread.tid, 1, GENERAL_BYTES).map_err(|e| {
                    CaptureError::new(format!(
                        "cannot read registers of thread {}: {e}",
                        thread.tid
                    ))
                })?;
                let float = register_set(thread.tid, 2, FLOAT_BYTES).ok();
                Ok(Registers {
                    tid: thread.tid,
                    general,
                    float,
                })
            })
            .collect()
    }
}

impl Drop for ThreadStop {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(1);
        for thread in &mut self.threads {
            if !thread.stopped {
                if ptrace(libc::PTRACE_INTERRUPT, thread.tid, 0, 0).is_err() {
                    continue;
                }
                match wait_stopped(thread.tid, deadline, || Ok(())) {
                    Ok(Some(signal)) => {
                        thread.signal = signal;
                        thread.stopped = true;
                    }
                    _ => continue,
                }
            }
            let _ = ptrace(libc::PTRACE_DETACH, thread.tid, 0, thread.signal as usize);
        }
    }
}

#[cfg(target_env = "musl")]
type PtraceRequest = libc::c_int;
#[cfg(not(target_env = "musl"))]
type PtraceRequest = libc::c_uint;

fn ptrace(request: PtraceRequest, tid: i32, address: usize, data: usize) -> io::Result<()> {
    if unsafe {
        libc::ptrace(
            request,
            tid,
            address as *mut libc::c_void,
            data as *mut libc::c_void,
        )
    } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wait_stopped(
    tid: i32,
    deadline: Instant,
    cancelled: impl Fn() -> Result<()>,
) -> Result<Option<i32>> {
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(tid, &mut status, libc::__WALL | libc::WNOHANG) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if result == 0 {
            cancelled()?;
            if Instant::now() >= deadline {
                return Err(CaptureError::terminal(format!(
                    "thread {tid} did not stop before the capture deadline"
                )));
            }
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        if !libc::WIFSTOPPED(status) {
            return Ok(None);
        }
        let event = status >> 16;
        return Ok(Some(if event == 0 {
            libc::WSTOPSIG(status)
        } else {
            0
        }));
    }
}

fn register_set(tid: i32, kind: usize, size: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; size];
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: size,
    };
    ptrace(
        libc::PTRACE_GETREGSET,
        tid,
        kind,
        (&mut vector as *mut libc::iovec) as usize,
    )?;
    if vector.iov_len != size {
        return Err(io::Error::other(format!(
            "expected {size} register bytes, received {}",
            vector.iov_len
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn waiting_for_a_thread_stop_has_a_deadline() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let started = Instant::now();
        let result = wait_stopped(
            child.id() as i32,
            started + Duration::from_millis(25),
            || Ok(()),
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(result.unwrap_err().description.contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn tracer_thread_exit_releases_attached_threads() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let tid = child.id() as i32;
        let result = std::thread::spawn(move || -> Result<()> {
            ptrace(libc::PTRACE_SEIZE, tid, 0, 0)?;
            ptrace(libc::PTRACE_INTERRUPT, tid, 0, 0)?;
            assert!(
                wait_stopped(tid, Instant::now() + Duration::from_secs(2), || Ok(()))?.is_some()
            );
            Ok(())
        })
        .join()
        .unwrap();
        // Joining can finish before the kernel detaches this thread's tracees.
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            let status = std::fs::read_to_string(format!("/proc/{tid}/status")).unwrap();
            if status.lines().any(|line| line == "TracerPid:\t0")
                && !status.lines().any(|line| line.starts_with("State:\tt"))
                || Instant::now() >= deadline
            {
                break status;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
        assert!(
            status.lines().any(|line| line == "TracerPid:\t0"),
            "{status}"
        );
        assert!(
            !status.lines().any(|line| line.starts_with("State:\tt")),
            "{status}"
        );
    }
}
