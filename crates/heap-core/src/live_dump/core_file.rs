use super::{CaptureError, Result, meter::Meter};
use crate::compressed::{BLOCK_SIZE, CompressorPool, seek_table};
use rayon::prelude::*;
#[cfg(any(test, target_os = "linux"))]
use std::collections::VecDeque;
#[cfg(target_os = "linux")]
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
};
use std::{
    fs::{self, File},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

const BATCH: usize = 64;
const WRITE_INTERVAL: u64 = 64 << 20;

#[derive(Clone, Copy)]
pub(super) struct Frame {
    #[cfg(any(test, target_os = "linux"))]
    pub position: u64,
    pub size: usize,
    #[cfg(any(test, target_os = "linux"))]
    pub offset: u64,
    pub length: usize,
}

pub(super) struct CoreFile<'a> {
    pub path: PathBuf,
    file: File,
    temporary_path: Option<tempfile::TempPath>,
    header_size: usize,
    room: usize,
    pub position: u64,
    file_offset: u64,
    flushed: u64,
    #[cfg(target_os = "linux")]
    punched: u64,
    checked_at: u64,
    dropped: u64,
    writing: u64,
    blocks: Vec<Vec<u8>>,
    spare_blocks: Vec<Vec<u8>>,
    compressors: CompressorPool,
    packed: Vec<Vec<u8>>,
    block_open: bool,
    pub frames: Vec<Frame>,
    #[cfg(any(test, target_os = "linux"))]
    reads: VecDeque<(usize, Vec<u8>)>,
    #[cfg(any(test, target_os = "linux"))]
    read_buffer: Vec<u8>,
    #[cfg(any(test, target_os = "linux"))]
    decoder: zstd::bulk::Decompressor<'static>,
    pool: &'a rayon::ThreadPool,
    meter: &'a Meter,
    counts_done: bool,
}

impl<'a> CoreFile<'a> {
    pub fn new(
        path: &Path,
        header_size: usize,
        temporary: bool,
        pool: &'a rayon::ThreadPool,
        meter: &'a Meter,
    ) -> Result<Self> {
        let parent = parent(path);
        let (file, temporary_path) = temporary_file(parent, temporary)?;
        let room = if header_size == 0 {
            0
        } else {
            zstd::zstd_safe::compress_bound(header_size) + 8
        };
        Ok(Self {
            path: path.into(),
            file,
            temporary_path,
            header_size,
            room,
            position: header_size as u64,
            flushed: header_size as u64,
            file_offset: room as u64,
            #[cfg(target_os = "linux")]
            punched: 0,
            checked_at: 0,
            dropped: room as u64,
            writing: room as u64,
            blocks: Vec::new(),
            spare_blocks: Vec::new(),
            compressors: CompressorPool::new(1),
            packed: Vec::new(),
            block_open: false,
            frames: Vec::new(),
            #[cfg(any(test, target_os = "linux"))]
            reads: VecDeque::new(),
            #[cfg(any(test, target_os = "linux"))]
            read_buffer: Vec::new(),
            #[cfg(any(test, target_os = "linux"))]
            decoder: zstd::bulk::Decompressor::new()?,
            pool,
            meter,
            counts_done: !temporary,
        })
    }

    #[cfg(any(test, target_os = "linux"))]
    pub fn frame(&self, position: u64) -> Result<usize> {
        let index = self
            .frames
            .partition_point(|f| f.position <= position)
            .checked_sub(1);
        index
            .filter(|&i| position - self.frames[i].position < self.frames[i].size as u64)
            .ok_or_else(|| {
                CaptureError::terminal(format!("the capture has no bytes at {position}"))
            })
    }

    #[cfg(any(test, target_os = "linux"))]
    fn decompressed(&mut self, index: usize) -> Result<&[u8]> {
        if let Some(slot) = self.reads.iter().position(|(i, _)| *i == index) {
            let entry = self.reads.remove(slot).unwrap();
            self.reads.push_front(entry);
        } else {
            let frame = self.frames[index];
            self.read_buffer.resize(frame.length, 0);
            self.file
                .read_exact_at(&mut self.read_buffer, frame.offset)?;
            let mut bytes = if self.reads.len() == 8 {
                self.reads.pop_back().unwrap().1
            } else {
                Vec::new()
            };
            bytes.clear();
            bytes.reserve(frame.size);
            if self
                .decoder
                .decompress_to_buffer(&self.read_buffer, &mut bytes)?
                != frame.size
            {
                return Err(CaptureError::terminal(
                    "cannot read back a block of the capture",
                ));
            }
            self.reads.push_front((index, bytes));
        }
        Ok(&self.reads.front().unwrap().1)
    }

    #[cfg(any(test, target_os = "linux"))]
    pub fn append_from(
        &mut self,
        source: &mut CoreFile<'_>,
        mut at: u64,
        mut count: usize,
    ) -> Result<()> {
        while count > 0 {
            let index = source.frame(at)?;
            let frame = source.frames[index];
            if at == frame.position && count >= frame.size {
                source.read_buffer.resize(frame.length, 0);
                source
                    .file
                    .read_exact_at(&mut source.read_buffer, frame.offset)?;
                self.flush()?;
                self.write(&source.read_buffer, self.file_offset)?;
                self.frames.push(Frame {
                    #[cfg(any(test, target_os = "linux"))]
                    position: self.flushed,
                    #[cfg(any(test, target_os = "linux"))]
                    offset: self.file_offset,
                    ..frame
                });
                self.file_offset += frame.length as u64;
                self.flushed += frame.size as u64;
                self.position += frame.size as u64;
                if self.counts_done {
                    self.meter.done(frame.size as u64);
                }
                self.write_back();
                at += frame.size as u64;
                count -= frame.size;
            } else {
                let from = (at - frame.position) as usize;
                let size = count.min(frame.size - from);
                self.append(&source.decompressed(index)?[from..from + size])?;
                at += size as u64;
                count -= size;
            }
        }
        Ok(())
    }

    pub fn append(&mut self, mut bytes: &[u8]) -> Result<()> {
        let count = bytes.len();
        while !bytes.is_empty() {
            if !self.block_open || self.blocks.last().is_some_and(|b| b.len() == BLOCK_SIZE) {
                if self.blocks.len() == BATCH {
                    self.flush()?;
                }
                self.blocks.push(
                    self.spare_blocks
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(BLOCK_SIZE)),
                );
                self.block_open = true;
            }
            let block = self.blocks.last_mut().unwrap();
            let size = bytes.len().min(BLOCK_SIZE - block.len());
            block.extend_from_slice(&bytes[..size]);
            bytes = &bytes[size..];
        }
        self.position += count as u64;
        if self.counts_done {
            self.meter.done(count as u64);
        }
        Ok(())
    }

    #[cfg(any(test, target_os = "linux"))]
    pub fn append_zeros(&mut self, mut count: u64) -> Result<()> {
        let zero = vec![0; BLOCK_SIZE];
        while count > 0 {
            let size = count.min(BLOCK_SIZE as u64) as usize;
            self.append(&zero[..size])?;
            count -= size as u64;
        }
        Ok(())
    }

    #[cfg(any(test, target_os = "linux"))]
    pub fn end_frame(&mut self) {
        self.block_open = false;
    }

    #[cfg(target_os = "macos")]
    pub fn append_parallel(
        &mut self,
        mut ranges: impl Iterator<Item = (u64, usize)>,
        read: impl Fn(u64, &mut [u8]) -> Result<()> + Sync,
    ) -> Result<()> {
        self.flush()?;
        loop {
            let batch: Vec<_> = ranges.by_ref().take(BATCH).collect();
            if batch.is_empty() {
                return Ok(());
            }
            for &(_, size) in &batch {
                let mut block = self.spare_blocks.pop().unwrap_or_default();
                block.resize(size, 0);
                self.blocks.push(block);
            }
            self.packed.resize_with(batch.len(), Vec::new);
            self.pool.install(|| {
                batch
                    .par_iter()
                    .zip(&mut self.blocks)
                    .zip(&mut self.packed)
                    .try_for_each(|((&(address, _), block), packed)| {
                        read(address, block)?;
                        self.compressors.compress(block, packed)?;
                        Ok::<_, CaptureError>(())
                    })
            })?;
            self.write_blocks()?;
            let size = batch.iter().map(|&(_, size)| size as u64).sum::<u64>();
            self.position += size;
            if self.counts_done {
                self.meter.done(size);
            }
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        while self.packed.len() < self.blocks.len() {
            self.packed.push(Vec::new());
        }
        self.pool.install(|| {
            self.blocks
                .par_iter()
                .zip(&mut self.packed)
                .try_for_each(|(bytes, packed)| self.compressors.compress(bytes, packed))
        })?;
        self.write_blocks()
    }

    fn write_blocks(&mut self) -> Result<()> {
        let packed = std::mem::take(&mut self.packed);
        for (i, bytes) in packed.iter().take(self.blocks.len()).enumerate() {
            let size = self.blocks[i].len();
            self.write(bytes, self.file_offset)?;
            self.frames.push(Frame {
                #[cfg(any(test, target_os = "linux"))]
                position: self.flushed,
                size,
                #[cfg(any(test, target_os = "linux"))]
                offset: self.file_offset,
                length: bytes.len(),
            });
            self.flushed += size as u64;
            self.file_offset += bytes.len() as u64;
        }
        self.packed = packed;
        for mut block in self.blocks.drain(..) {
            block.clear();
            self.spare_blocks.push(block);
        }
        self.block_open = false;
        self.write_back();
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn disk_bytes(&self) -> u64 {
        self.file_offset - self.punched.saturating_sub(self.room as u64)
    }

    #[cfg(target_os = "linux")]
    pub fn punch(&mut self, offset: u64) {
        #[cfg(target_os = "linux")]
        {
            let end = offset & !4095;
            let start = self.punched.max(self.room as u64);
            if end <= start {
                return;
            }
            if unsafe {
                libc::fallocate(
                    self.file.as_raw_fd(),
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    start as i64,
                    (end - start) as i64,
                )
            } == 0
            {
                self.punched = end;
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = offset;
    }

    fn write_back(&mut self) {
        if self.file_offset - self.writing < WRITE_INTERVAL {
            return;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            libc::sync_file_range(
                self.file.as_raw_fd(),
                self.writing as i64,
                (self.file_offset - self.writing) as i64,
                libc::SYNC_FILE_RANGE_WRITE,
            );
            if self.writing > self.dropped {
                libc::sync_file_range(
                    self.file.as_raw_fd(),
                    self.dropped as i64,
                    (self.writing - self.dropped) as i64,
                    libc::SYNC_FILE_RANGE_WAIT_BEFORE
                        | libc::SYNC_FILE_RANGE_WRITE
                        | libc::SYNC_FILE_RANGE_WAIT_AFTER,
                );
                libc::posix_fadvise(
                    self.file.as_raw_fd(),
                    self.dropped as i64,
                    (self.writing - self.dropped) as i64,
                    libc::POSIX_FADV_DONTNEED,
                );
            }
        }
        self.dropped = self.writing;
        self.writing = self.file_offset;
    }

    pub fn finish(mut self, header: &[u8]) -> Result<u64> {
        self.flush()?;
        if header.len() != self.header_size {
            return Err(CaptureError::terminal("the capture's header size changed"));
        }
        let mut bytes = zstd::bulk::compress(header, 1)?;
        let header_length = bytes.len();
        let skip = self
            .room
            .checked_sub(header_length + 8)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| CaptureError::terminal("the capture's headers are too large"))?;
        bytes.extend_from_slice(&0x184d2a50u32.to_le_bytes());
        bytes.extend_from_slice(&skip.to_le_bytes());
        bytes.resize(self.room, 0);
        self.write(&bytes, 0)?;
        let mut frames = vec![
            (header_length as u32, header.len() as u32),
            ((self.room - header_length) as u32, 0),
        ];
        frames.extend(self.frames.iter().map(|f| (f.length as u32, f.size as u32)));
        let table = seek_table(&frames)?;
        self.write(&table, self.file_offset)?;
        self.file_offset += table.len() as u64;
        self.file.sync_all()?;
        if let Some(temporary) = &self.temporary_path {
            fs::hard_link(temporary, &self.path)?;
        } else {
            #[cfg(target_os = "linux")]
            {
                let source =
                    CString::new(format!("/proc/self/fd/{}", self.file.as_raw_fd())).unwrap();
                let target = CString::new(self.path.as_os_str().as_bytes())
                    .map_err(|_| CaptureError::terminal("the dump path contains a NUL byte"))?;
                if unsafe {
                    libc::linkat(
                        libc::AT_FDCWD,
                        source.as_ptr(),
                        libc::AT_FDCWD,
                        target.as_ptr(),
                        libc::AT_SYMLINK_FOLLOW,
                    )
                } != 0
                {
                    return Err(CaptureError::system(format!(
                        "cannot name the core {}",
                        self.path.display()
                    )));
                }
            }
            #[cfg(not(target_os = "linux"))]
            return Err(CaptureError::terminal("the capture has no temporary name"));
        }
        Ok(self.file_offset)
    }

    fn write(&mut self, bytes: &[u8], offset: u64) -> Result<()> {
        if offset + bytes.len() as u64 >= self.checked_at + WRITE_INTERVAL {
            check_free_space(&self.path)?;
            self.checked_at = offset + bytes.len() as u64;
        }
        self.file.write_all_at(bytes, offset)?;
        self.meter.written(bytes.len() as u64);
        Ok(())
    }
}

pub(super) fn temporary_file(
    folder: &Path,
    temporary: bool,
) -> Result<(File, Option<tempfile::TempPath>)> {
    #[cfg(target_os = "linux")]
    {
        let path = CString::new(folder.as_os_str().as_bytes())
            .map_err(|_| CaptureError::terminal("the capture folder contains a NUL byte"))?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            return Ok((unsafe { File::from_raw_fd(fd) }, None));
        }
    }
    let (file, path) = tempfile::Builder::new()
        .prefix(".dotheap-")
        .tempfile_in(folder)?
        .into_parts();
    if temporary {
        path.close()?;
        Ok((file, None))
    } else {
        Ok((file, Some(path)))
    }
}

pub(super) fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub(super) fn check_free_space(path: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let folder = parent(path);
    let name = CString::new(folder.as_os_str().as_bytes())
        .map_err(|_| CaptureError::terminal("the capture folder contains a NUL byte"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(CaptureError::system(format!(
            "cannot check free space in {}",
            folder.display()
        )));
    }
    let stat = unsafe { stat.assume_init() };
    let free = u128::from(stat.f_bavail) * u128::from(stat.f_frsize);
    if free < 2 << 30 {
        return Err(CaptureError::terminal(format!(
            "the file system of {} has {} free; dotheap stops at 2 GB so that the dump does not \
                fill the disk. Write the dump to another file system with --out",
            folder.display(),
            crate::checks::format_size(free as u64)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_copies_frames_and_partial_frames_without_replacing_an_output() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("capture.zst");
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let meter = Meter::new();
        let original: Vec<_> = (0..BLOCK_SIZE * (BATCH * 2 + 3) + 97)
            .map(|i| ((i * 37) % 251) as u8)
            .collect();
        let mut source = CoreFile::new(&path, 0, true, &pool, &meter).unwrap();
        source.append(&original).unwrap();
        source.flush().unwrap();
        let header = vec![42; 4096];
        let mut output = CoreFile::new(&path, header.len(), false, &pool, &meter).unwrap();
        output.append_from(&mut source, 31, 4096).unwrap();
        output.append_zeros(4096).unwrap();
        output
            .append_from(&mut source, BLOCK_SIZE as u64, BLOCK_SIZE * 2)
            .unwrap();
        for block in 0..12 {
            output
                .append_from(&mut source, (block * BLOCK_SIZE + 31) as u64, 97)
                .unwrap();
            output.end_frame();
        }
        output.append(&original).unwrap();
        output.finish(&header).unwrap();
        let data = zstd::stream::decode_all(File::open(&path).unwrap()).unwrap();
        assert_eq!(&data[..4096], &header);
        assert_eq!(&data[4096..8192], &original[31..4127]);
        assert_eq!(&data[8192..12288], &[0; 4096]);
        let mut at = 12288;
        assert_eq!(
            &data[at..at + BLOCK_SIZE * 2],
            &original[BLOCK_SIZE..BLOCK_SIZE * 3]
        );
        at += BLOCK_SIZE * 2;
        for block in 0..12 {
            let start = block * BLOCK_SIZE + 31;
            assert_eq!(&data[at..at + 97], &original[start..start + 97]);
            at += 97;
        }
        assert_eq!(&data[at..], &original);
        let another = CoreFile::new(&path, header.len(), false, &pool, &meter).unwrap();
        assert!(another.finish(&header).is_err());
        assert_eq!(
            data,
            zstd::stream::decode_all(File::open(&path).unwrap()).unwrap()
        );
    }
}
