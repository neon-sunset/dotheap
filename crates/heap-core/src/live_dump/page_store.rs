use super::{
    CaptureError, Result,
    core_file::{CoreFile, check_free_space, parent, temporary_file},
};
use memmap2::{MmapMut, MmapOptions};
use std::{
    fs::{self, File},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Mutex,
};

const BLOCK_SIZE: usize = 64 << 20;

pub(super) struct StopMemory {
    path: PathBuf,
    state: Mutex<MemoryState>,
}

struct MemoryState {
    remaining: u64,
    file: Option<File>,
    file_bytes: u64,
}

impl StopMemory {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.into(),
            state: Mutex::new(MemoryState {
                remaining: available_memory() / 4,
                file: None,
                file_bytes: 0,
            }),
        }
    }

    pub fn file_bytes(&self) -> u64 {
        self.state.lock().unwrap().file_bytes
    }

    fn block(&self) -> Result<MmapMut> {
        let mut state = self.state.lock().unwrap();
        if state.remaining >= BLOCK_SIZE as u64 {
            let memory = MmapMut::map_anon(BLOCK_SIZE)?;
            state.remaining -= BLOCK_SIZE as u64;
            return Ok(memory);
        }
        if state.file.is_none() {
            state.file = Some(temporary_file(parent(&self.path), true)?.0);
        }
        check_free_space(&self.path)?;
        let file = state.file.as_ref().unwrap();
        let error = unsafe {
            libc::posix_fallocate(file.as_raw_fd(), state.file_bytes as i64, BLOCK_SIZE as i64)
        };
        if error != 0 {
            return Err(CaptureError::terminal(format!(
                "cannot allocate temporary disk space for the final copy: {}",
                std::io::Error::from_raw_os_error(error)
            )));
        }
        // The private temporary file has allocated disk blocks and is never truncated.
        let memory = unsafe {
            MmapOptions::new()
                .offset(state.file_bytes)
                .len(BLOCK_SIZE)
                .map_mut(file)?
        };
        state.file_bytes += BLOCK_SIZE as u64;
        Ok(memory)
    }
}

pub(super) struct PageStore<'a> {
    memory: &'a StopMemory,
    blocks: Vec<MmapMut>,
    end: usize,
    pub bytes: u64,
}

impl<'a> PageStore<'a> {
    pub fn new(memory: &'a StopMemory) -> Self {
        Self {
            memory,
            blocks: Vec::new(),
            end: 0,
            bytes: 0,
        }
    }

    pub fn add(&mut self, bytes: &[u8]) -> Result<usize> {
        if bytes.len() > BLOCK_SIZE {
            return Err(CaptureError::terminal("a captured page is too large"));
        }
        if self.end + bytes.len() > self.blocks.len() * BLOCK_SIZE {
            self.blocks.push(self.memory.block()?);
            self.end = (self.blocks.len() - 1) * BLOCK_SIZE;
        }
        let at = self.end;
        let start = at % BLOCK_SIZE;
        self.blocks[at / BLOCK_SIZE][start..start + bytes.len()].copy_from_slice(bytes);
        self.end += bytes.len();
        self.bytes += bytes.len() as u64;
        Ok(at)
    }

    pub fn write(
        &self,
        mut offset: usize,
        mut count: usize,
        core: &mut CoreFile<'_>,
    ) -> Result<()> {
        while count > 0 {
            let size = count.min(BLOCK_SIZE - offset % BLOCK_SIZE);
            let start = offset % BLOCK_SIZE;
            core.append(&self.blocks[offset / BLOCK_SIZE][start..start + size])?;
            offset += size;
            count -= size;
        }
        Ok(())
    }
}

fn available_memory() -> u64 {
    let info = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut available = info
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0)
        .saturating_mul(1024);
    let root = Path::new("/sys/fs/cgroup");
    let mut current = root.to_owned();
    if let Ok(groups) = fs::read_to_string("/proc/self/cgroup") {
        if let Some(path) = groups.lines().find_map(|line| line.strip_prefix("0::")) {
            if !path.split('/').any(|part| part == "..") {
                current = root.join(path.trim_start_matches('/'));
            }
        }
    }
    available = cgroup_available(root, &current, available);
    available
}

fn cgroup_available(root: &Path, current: &Path, mut available: u64) -> u64 {
    for folder in current
        .ancestors()
        .take_while(|folder| folder.starts_with(root))
    {
        let number = |name| {
            fs::read_to_string(folder.join(name))
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        };
        if let (Some(limit), Some(current)) = (number("memory.max"), number("memory.current")) {
            available = available.min(limit.saturating_sub(current));
        }
    }
    available
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intermediate_cgroup_limits_constrain_the_memory_budget() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let group = root.join("parent/child");
        fs::create_dir_all(&group).unwrap();
        fs::write(root.join("memory.max"), "max").unwrap();
        fs::write(root.join("parent/memory.max"), "1000").unwrap();
        fs::write(root.join("parent/memory.current"), "800").unwrap();
        fs::write(group.join("memory.max"), "800").unwrap();
        fs::write(group.join("memory.current"), "100").unwrap();
        assert_eq!(cgroup_available(root, &group, 900), 200);
        fs::write(root.join("parent/memory.current"), "1100").unwrap();
        assert_eq!(cgroup_available(root, &group, 900), 0);
    }
}
