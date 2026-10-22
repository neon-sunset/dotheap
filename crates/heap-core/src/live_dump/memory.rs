use super::{CaptureError, Result};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) struct MemoryReader {
    pub pid: i32,
    pub page_size: usize,
    directory: File,
    memory: File,
    pagemap: File,
    pub unreadable: AtomicU64,
}

impl MemoryReader {
    pub fn open(pid: i32) -> Result<Self> {
        let directory = File::open(format!("/proc/{pid}"))?;
        let memory = open_at(&directory, "mem", libc::O_RDONLY).map_err(|e| {
            CaptureError::new(format!(
                "cannot open the memory of process {pid}: {e}; run dotheap as root, or as its user \
                with /proc/sys/kernel/yama/ptrace_scope set to 0"
            ))
        })?;
        let pagemap = open_at(&directory, "pagemap", libc::O_RDONLY)?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 || !(page_size as usize).is_power_of_two() {
            return Err(CaptureError::terminal(
                "cannot determine the system page size",
            ));
        }
        Ok(Self {
            pid,
            page_size: page_size as usize,
            directory,
            memory,
            pagemap,
            unreadable: AtomicU64::new(0),
        })
    }

    pub fn proc_file(&self, name: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        open_at(&self.directory, name, libc::O_RDONLY)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn clear_soft_dirty(&self) -> Result<()> {
        open_at(&self.directory, "clear_refs", libc::O_WRONLY)?.write_all(b"4")?;
        Ok(())
    }

    pub fn thread_ids(&self) -> Result<Vec<i32>> {
        let directory = open_at(&self.directory, "task", libc::O_RDONLY | libc::O_DIRECTORY)?;
        let entries = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
        let mut tids: Vec<i32> = entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        tids.sort_unstable();
        Ok(tids)
    }

    pub fn read(&self, address: u64, buffer: &mut [u8], required: bool) -> Result<()> {
        let mut done = 0;
        while done < buffer.len() {
            let at = address.checked_add(done as u64).ok_or_else(|| {
                CaptureError::terminal("a memory range overflows the address space")
            })?;
            let local = libc::iovec {
                iov_base: buffer[done..].as_mut_ptr().cast(),
                iov_len: buffer.len() - done,
            };
            let remote = libc::iovec {
                iov_base: at as usize as *mut libc::c_void,
                iov_len: local.iov_len,
            };
            let count = unsafe { libc::process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
            if count > 0 {
                done += count as usize;
                continue;
            }
            if count < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let page =
                (self.page_size - (at as usize & (self.page_size - 1))).min(buffer.len() - done);
            match self.memory.read_at(&mut buffer[done..done + page], at) {
                Ok(count) if count > 0 => {
                    done += count;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                _ if required => {
                    return Err(CaptureError::terminal(format!(
                        "cannot read memory at 0x{at:x} while process {} is stopped",
                        self.pid
                    )));
                }
                _ => {
                    buffer[done..done + page].fill(0);
                    self.unreadable.fetch_add(page as u64, Ordering::Relaxed);
                    done += page;
                }
            }
        }
        Ok(())
    }

    pub fn pagemap(&self, address: u64, entries: &mut [u64]) -> Result<()> {
        let offset = (address / self.page_size as u64)
            .checked_mul(8)
            .ok_or_else(|| CaptureError::terminal("a pagemap offset overflows"))?;
        let bytes = bytemuck::cast_slice_mut(entries);
        self.pagemap.read_exact_at(bytes, offset).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                CaptureError::terminal(format!(
                    "process {} exited or replaced its address space during capture",
                    self.pid
                ))
            } else {
                CaptureError::new(format!("cannot read /proc/{}/pagemap: {error}", self.pid))
            }
        })?;
        Ok(())
    }
}

fn open_at(directory: &File, name: &str, flags: i32) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid proc file name"))?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub(super) fn in_memory(entry: u64) -> bool {
    entry & (3 << 62) != 0
}
pub(super) fn soft_dirty(entry: u64) -> bool {
    entry & (1 << 55) != 0
}

pub(super) fn soft_dirty_works(page_size: usize) -> bool {
    let check = || -> io::Result<bool> {
        let mut page = memmap2::MmapMut::map_anon(page_size)?;
        fs::write("/proc/self/clear_refs", b"4")?;
        page[0] = 1;
        let mut entry = [0; 8];
        File::open("/proc/self/pagemap")?
            .read_exact_at(&mut entry, (page.as_ptr() as u64 / page_size as u64) * 8)?;
        Ok(soft_dirty(u64::from_le_bytes(entry)))
    };
    check().unwrap_or(false)
}

pub(super) fn hash_page(bytes: &[u8]) -> u64 {
    let mut a = 0x243f_6a88_85a3_08d3u64;
    let mut b = 0x1319_8a2e_0370_7344u64;
    for pair in bytes.chunks_exact(16) {
        a = (a ^ u64::from_le_bytes(pair[..8].try_into().unwrap()))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
        a ^= a >> 29;
        b = (b ^ u64::from_le_bytes(pair[8..].try_into().unwrap()))
            .wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
        b ^= b >> 31;
    }
    a ^ b.wrapping_mul(0x1656_67b1_9e37_79f9)
}

pub(super) struct OomPreference(Option<Vec<u8>>);

impl OomPreference {
    pub fn enter() -> Self {
        let previous = fs::read("/proc/self/oom_score_adj").ok();
        if previous.is_some() {
            let _ = fs::write("/proc/self/oom_score_adj", b"1000");
        }
        Self(previous)
    }
}

impl Drop for OomPreference {
    fn drop(&mut self) {
        if let Some(previous) = &self.0 {
            let _ = fs::write("/proc/self/oom_score_adj", previous);
        }
    }
}
