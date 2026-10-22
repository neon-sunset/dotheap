use crate::{
    Error, Result,
    bytes::ReadBytes,
    column::{create_directory, persist},
};
use memmap2::{Mmap, MmapMut, MmapOptions};
use rayon::prelude::*;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub const BLOCK_SIZE: usize = 1 << 18;
const LARGEST_FRAME: usize = 16 << 20;

pub(crate) struct CompressorPool {
    level: i32,
    contexts: Mutex<Vec<zstd::bulk::Compressor<'static>>>,
}

impl CompressorPool {
    pub fn new(level: i32) -> Self {
        Self {
            level,
            contexts: Mutex::new(Vec::new()),
        }
    }

    pub fn compress(&self, bytes: &[u8], output: &mut Vec<u8>) -> std::io::Result<()> {
        let context = self.contexts.lock().unwrap().pop();
        let mut context = match context {
            Some(context) => context,
            None => zstd::bulk::Compressor::new(self.level)?,
        };
        output.clear();
        output.reserve(zstd::zstd_safe::compress_bound(bytes.len()));
        let result = context.compress_to_buffer(bytes, output);
        self.contexts.lock().unwrap().push(context);
        result.map(|_| ())
    }
}

pub fn identity(path: &Path) -> Result<String> {
    let path = path.canonicalize()?;
    let m = fs::metadata(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "{}\n{} {} {} {} {} {} {}",
            path.display(),
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        let (volume, index) = file_id(&File::open(&path)?)?;
        Ok(format!(
            "{}\n{volume} {index} {} {:?} {:?}",
            path.display(),
            m.len(),
            m.modified()?,
            m.created()?
        ))
    }
}

#[cfg(unix)]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::cache("file has no parent folder"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    persist(file, path)?;
    Ok(())
}

fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path).map_err(|e| Error::open(path.display(), e))?;
    if file.metadata()?.len() == 0 {
        return Err(Error::open(path.display(), "the file is empty"));
    }
    // A dump or completed cache file must not be modified while it is open.
    unsafe { MmapOptions::new().map(&file) }.map_err(|e| Error::open(path.display(), e))
}

pub struct CompressedCore {
    frames: Mmap,
    offsets: Vec<usize>,
    starts: Vec<usize>,
    slots: Vec<usize>,
    decoders: Mutex<Vec<zstd::bulk::Decompressor<'static>>>,
    directory: PathBuf,
    marker: File,
}

impl CompressedCore {
    pub fn open(path: &Path, directory: &Path) -> Result<Self> {
        create_directory(directory)?;
        let before = identity(path)?;
        if let Ok(core) = Self::cached(path, directory, &before) {
            return Ok(core);
        }
        let input = map_file(path)?;
        let (offsets, starts) = if let Some(index) = index_frames(&input) {
            let _ = fs::remove_file(directory.join("core.blocks"));
            index
        } else {
            let mut decoder = zstd::stream::read::Decoder::new(&input[..])
                .map_err(|e| Error::cache(format!("cannot decompress {}: {e}", path.display())))?;
            let mut blocks = tempfile::NamedTempFile::new_in(directory)?;
            let (mut offsets, mut starts) = (vec![0], vec![0]);
            const BATCH: usize = 64;
            let mut buffer = vec![0; BLOCK_SIZE * BATCH];
            let compressors = CompressorPool::new(3);
            let mut packed = vec![Vec::new(); BATCH];
            loop {
                let mut n = 0;
                while n < buffer.len() {
                    match decoder.read(&mut buffer[n..]) {
                        Ok(0) => break,
                        Ok(k) => n += k,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            return Err(Error::cache(format!(
                                "cannot decompress {}: {e}",
                                path.display()
                            )));
                        }
                    }
                }
                if n == 0 {
                    break;
                }
                let count = n.div_ceil(BLOCK_SIZE);
                packed[..count]
                    .par_iter_mut()
                    .zip(buffer[..n].par_chunks(BLOCK_SIZE))
                    .try_for_each(|(packed, bytes)| -> std::io::Result<()> {
                        packed.clear();
                        if bytes.iter().any(|&b| b != 0) {
                            compressors.compress(bytes, packed)?;
                        }
                        Ok(())
                    })?;
                for (bytes, packed) in buffer[..n].chunks(BLOCK_SIZE).zip(&packed) {
                    blocks.write_all(packed)?;
                    offsets.push(offsets.last().unwrap() + packed.len());
                    starts.push(starts.last().unwrap() + bytes.len());
                }
            }
            if starts.len() < 2 {
                return Err(Error::cache("the compressed dump is empty"));
            }
            blocks.as_file().sync_all()?;
            persist(blocks, &directory.join("core.blocks"))?;
            (offsets, starts)
        };
        if identity(path)? != before {
            return Err(Error::open(
                path.display(),
                "the dump changed while it was being read",
            ));
        }
        let _ = fs::remove_file(directory.join("core.dump"));
        for (name, values) in [("core.index.col", offsets), ("core.starts.col", starts)] {
            let data: Vec<u8> = values
                .iter()
                .flat_map(|&n| (n as u64).to_le_bytes())
                .collect();
            atomic_write(&directory.join(name), &data)?;
        }
        atomic_write(&directory.join("core.dump"), before.as_bytes())?;
        Self::cached(path, directory, &before)
    }

    fn cached(path: &Path, directory: &Path, expected: &str) -> Result<Self> {
        let mut marker = File::open(directory.join("core.dump"))?;
        let mut identity = String::new();
        marker.read_to_string(&mut identity)?;
        if identity != expected {
            return Err(Error::cache("the compressed cache belongs to another dump"));
        }
        let read_column = |name: &str| -> Result<Vec<usize>> {
            let data = fs::read(directory.join(name))?;
            if data.len() < 16 || data.len() % 8 != 0 {
                return Err(Error::cache("invalid compressed core index"));
            }
            data.chunks_exact(8)
                .map(|b| {
                    usize::try_from(u64::from_le_bytes(b.try_into().unwrap()))
                        .map_err(|_| Error::cache("core index is too large"))
                })
                .collect()
        };
        let offsets = read_column("core.index.col")?;
        let starts = read_column("core.starts.col")?;
        let blocks = directory.join("core.blocks");
        // All-zero cores have no compressed bytes, but still need an addressable mapping.
        let frames = if blocks.exists() && fs::metadata(&blocks)?.len() == 0 {
            MmapOptions::new().len(1).map_anon()?.make_read_only()?
        } else {
            map_file(if blocks.exists() { &blocks } else { path })?
        };
        if starts.len() != offsets.len()
            || starts[0] != 0
            || offsets.windows(2).any(|v| v[0] > v[1])
            || starts
                .windows(2)
                .any(|v| v[0] >= v[1] || v[1] - v[0] > LARGEST_FRAME)
            || *offsets.last().unwrap() > frames.len()
            || (blocks.exists() && *offsets.last().unwrap() as u64 != fs::metadata(&blocks)?.len())
        {
            return Err(Error::cache("invalid compressed core index"));
        }
        let length = *starts.last().unwrap();
        let mut block = 0;
        let slots = (0..=length / BLOCK_SIZE + 1)
            .map(|slot| {
                let offset = slot.saturating_mul(BLOCK_SIZE).min(length - 1);
                while block + 1 < starts.len() - 1 && starts[block + 1] <= offset {
                    block += 1;
                }
                block
            })
            .collect();
        Ok(Self {
            frames,
            offsets,
            starts,
            slots,
            decoders: Mutex::new(Vec::new()),
            directory: directory.to_owned(),
            marker,
        })
    }

    pub fn len(&self) -> usize {
        *self.starts.last().unwrap()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn block(&self, offset: usize) -> usize {
        let slot = offset / BLOCK_SIZE;
        let start = self.slots[slot];
        let end = self.slots[slot + 1];
        start + self.starts[start..=end].partition_point(|&s| s <= offset) - 1
    }
    pub fn range(&self, block: usize) -> std::ops::Range<usize> {
        self.starts[block]..self.starts[block + 1]
    }

    fn decode(&self, block: usize, buffer: &mut Vec<u8>) -> Result<()> {
        let range = self.range(block);
        let (a, b) = (self.offsets[block], self.offsets[block + 1]);
        buffer.clear();
        if a == b {
            buffer.resize(range.len(), 0);
            return Ok(());
        }
        buffer.reserve(range.len());
        let decoder = self.decoders.lock().unwrap().pop();
        let mut decoder = match decoder {
            Some(decoder) => decoder,
            None => zstd::bulk::Decompressor::new()?,
        };
        let result = decoder.decompress_to_buffer(&self.frames[a..b], buffer);
        self.decoders.lock().unwrap().push(decoder);
        match result {
            Ok(length) if length == range.len() => Ok(()),
            _ => Err(Error::cache(format!(
                "block {block} of the core cannot be decompressed"
            ))),
        }
    }

    fn invalidate(&self) {
        let Ok(Some(_guard)) = crate::cache::CacheGuard::try_lock(&self.directory) else {
            return;
        };
        let current = File::open(self.directory.join("core.dump")).and_then(|file| file_id(&file));
        let original = file_id(&self.marker);
        if current.ok().zip(original.ok()).is_none_or(|(a, b)| a != b) {
            return;
        }
        for name in ["manifest.json", "core.dump"] {
            let _ = fs::remove_file(self.directory.join(name));
        }
    }
}

fn index_frames(bytes: &[u8]) -> Option<(Vec<usize>, Vec<usize>)> {
    let (mut offsets, mut starts, mut at, mut position) = (Vec::new(), Vec::new(), 0usize, 0usize);
    while at < bytes.len() {
        let rest = &bytes[at..];
        let (length, size) = if rest.u32(0)? & 0xffff_fff0 == 0x184d_2a50 {
            (8usize.checked_add(rest.u32(4)? as usize)?, 0)
        } else {
            let size =
                usize::try_from(zstd::zstd_safe::get_frame_content_size(rest).ok()??).ok()?;
            if size > LARGEST_FRAME {
                return None;
            }
            (
                zstd::zstd_safe::find_frame_compressed_size(rest).ok()?,
                size,
            )
        };
        if length == 0 || length > rest.len() {
            return None;
        }
        if size > 0 {
            offsets.push(at);
            starts.push(position);
        }
        at += length;
        position = position.checked_add(size)?;
    }
    if starts.is_empty() {
        return None;
    }
    offsets.push(at);
    starts.push(position);
    Some((offsets, starts))
}

enum Backing {
    Raw(Mmap),
    Compressed(Box<LazyMemory>),
}

pub struct Source {
    backing: Backing,
}

impl Source {
    pub fn open(path: &Path, directory: Option<&Path>) -> Result<Self> {
        let mut prefix = [0; 4];
        let n = File::open(path)
            .map_err(|e| Error::open(path.display(), e))?
            .read(&mut prefix)?;
        let compressed = path.extension().is_some_and(|s| s == "zst")
            || (n == 4 && prefix == [0x28, 0xb5, 0x2f, 0xfd]);
        Ok(Self {
            backing: if compressed {
                let directory = directory.ok_or_else(|| {
                    Error::cache("a .zst dump needs a cache folder for its compressed blocks")
                })?;
                Backing::Compressed(Box::new(LazyMemory::new(
                    CompressedCore::open(path, directory)?,
                    directory,
                )?))
            } else {
                Backing::Raw(map_file(path)?)
            },
        })
    }

    pub fn len(&self) -> usize {
        match &self.backing {
            Backing::Raw(m) => m.len(),
            Backing::Compressed(m) => m.core.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn window(
        &self,
        offset: usize,
        minimum: usize,
        scratch: Option<Vec<u8>>,
    ) -> Option<(usize, std::borrow::Cow<'_, [u8]>)> {
        if offset.checked_add(minimum)? > self.len() {
            return None;
        }
        if minimum == 0 {
            return Some((offset, std::borrow::Cow::Borrowed(&[])));
        }
        match &self.backing {
            Backing::Raw(m) => Some((0, std::borrow::Cow::Borrowed(m))),
            Backing::Compressed(m) => {
                let block = m.core.block(offset);
                let range = m.core.range(block);
                if offset + minimum > range.end {
                    return self
                        .bytes(offset, minimum)
                        .map(|b| (offset, std::borrow::Cow::Borrowed(b)));
                }
                if let Some(mut buffer) = scratch
                    && !m.ready[block].load(Ordering::Acquire)
                {
                    match m.core.decode(block, &mut buffer) {
                        Ok(()) => Some((range.start, std::borrow::Cow::Owned(buffer))),
                        Err(e) => {
                            m.fail(e);
                            None
                        }
                    }
                } else {
                    m.bytes(range.start, range.len())
                        .map(|b| (range.start, std::borrow::Cow::Borrowed(b)))
                }
            }
        }
    }
    pub fn bytes(&self, offset: usize, count: usize) -> Option<&[u8]> {
        let end = offset.checked_add(count)?;
        if end > self.len() {
            return None;
        }
        match &self.backing {
            Backing::Raw(m) => Some(&m[offset..end]),
            Backing::Compressed(m) => m.bytes(offset, count),
        }
    }

    pub fn check(&self) -> Result<()> {
        if let Backing::Compressed(m) = &self.backing
            && let Some(e) = m.error.lock().unwrap().as_ref()
        {
            return Err(e.clone());
        }
        Ok(())
    }

    pub fn scan(&self, start: usize, length: usize, mut body: impl FnMut(usize, &[u8])) {
        let Some(end) = start.checked_add(length).filter(|&end| end <= self.len()) else {
            return;
        };
        match &self.backing {
            Backing::Raw(m) => {
                for (i, bytes) in m[start..end].chunks(BLOCK_SIZE).enumerate() {
                    body(start + i * BLOCK_SIZE, bytes);
                }
            }
            Backing::Compressed(m) => {
                let mut at = start;
                let mut buffer = Vec::new();
                while at < end {
                    let block = m.core.block(at);
                    let range = m.core.range(block);
                    let stop = range.end.min(end);
                    if m.ready[block].load(Ordering::Acquire) {
                        body(at, m.bytes(at, stop - at).unwrap());
                    } else {
                        match m.core.decode(block, &mut buffer) {
                            Ok(()) => body(at, &buffer[at - range.start..stop - range.start]),
                            Err(e) => {
                                m.fail(e);
                                break;
                            }
                        }
                    }
                    at = stop;
                }
            }
        }
    }
}

struct LazyMemory {
    core: CompressedCore,
    _map: MmapMut,
    pointer: *mut u8,
    ready: Vec<AtomicBool>,
    locks: [Mutex<()>; 64],
    error: Mutex<Option<Error>>,
}

// Each block is written once under its lock. Readers get references only after
// publication with Release/Acquire, and no code can change published bytes.
unsafe impl Send for LazyMemory {}
unsafe impl Sync for LazyMemory {}

impl LazyMemory {
    fn new(core: CompressedCore, directory: &Path) -> Result<Self> {
        let file = tempfile::tempfile_in(directory)?;
        file.set_len(core.len() as u64)?;
        // The unlinked file and its mapping are owned exclusively by this object.
        let mut map = unsafe { MmapOptions::new().map_mut(&file)? };
        let pointer = map.as_mut_ptr();
        let ready = (1..core.starts.len())
            .map(|_| AtomicBool::new(false))
            .collect();
        Ok(Self {
            core,
            _map: map,
            pointer,
            ready,
            locks: std::array::from_fn(|_| Mutex::new(())),
            error: Mutex::new(None),
        })
    }

    fn fail(&self, e: Error) {
        let mut error = self.error.lock().unwrap();
        if error.is_none() {
            *error = Some(e);
            self.core.invalidate();
        }
    }

    fn bytes(&self, offset: usize, count: usize) -> Option<&[u8]> {
        if count > 0 {
            let last = self.core.block(offset + count - 1);
            let mut buffer = Vec::new();
            for block in self.core.block(offset)..=last {
                if self.ready[block].load(Ordering::Acquire) {
                    continue;
                }
                let _lock = self.locks[block % self.locks.len()].lock().unwrap();
                if self.ready[block].load(Ordering::Acquire) {
                    continue;
                }
                if let Err(e) = self.core.decode(block, &mut buffer) {
                    self.fail(e);
                    return None;
                }
                let start = self.core.starts[block];
                for (i, page) in buffer.chunks(4096).enumerate() {
                    if page.iter().any(|&b| b != 0) {
                        // This block has no readers until the ready flag is published.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                page.as_ptr(),
                                self.pointer.add(start + i * 4096),
                                page.len(),
                            );
                        }
                    }
                }
                self.ready[block].store(true, Ordering::Release);
            }
        }
        // Every byte in this range is initialized and immutable for self's lifetime.
        Some(unsafe { std::slice::from_raw_parts(self.pointer.add(offset), count) })
    }
}

pub fn seek_table(frames: &[(u32, u32)]) -> Result<Vec<u8>> {
    let payload = frames
        .len()
        .checked_mul(8)
        .and_then(|n| n.checked_add(9))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| Error::cache("too many compressed frames"))?;
    let mut table = Vec::with_capacity(payload as usize + 8);
    for value in [0x184d_2a5e, payload] {
        table.extend_from_slice(&value.to_le_bytes());
    }
    for &(length, size) in frames {
        table.extend_from_slice(&length.to_le_bytes());
        table.extend_from_slice(&size.to_le_bytes());
    }
    table.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    table.push(0);
    table.extend_from_slice(&0x8f92_eab1u32.to_le_bytes());
    Ok(table)
}

pub fn compress(source: &Path, target: &Path, progress: impl FnMut(u64, u64)) -> Result<()> {
    compress_file(File::open(source)?, target, progress)
}

pub fn compress_file(
    mut input: File,
    target: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    let workers = (std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        / 2)
    .clamp(1, 8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .map_err(|e| Error::cache(e.to_string()))?;
    const BATCH: usize = 64;
    let mut buffer = vec![0; BATCH * BLOCK_SIZE];
    let compressors = CompressorPool::new(1);
    let mut packed = vec![Vec::new(); BATCH];
    let (mut frames, mut done, mut written) = (Vec::new(), 0, 0);
    loop {
        let mut count = 0;
        while count < buffer.len() {
            match input.read(&mut buffer[count..]) {
                Ok(0) => break,
                Ok(n) => count += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if count == 0 {
            break;
        }
        let blocks = count.div_ceil(BLOCK_SIZE);
        pool.install(|| {
            packed[..blocks]
                .par_iter_mut()
                .zip(buffer[..count].par_chunks(BLOCK_SIZE))
                .try_for_each(|(packed, bytes)| compressors.compress(bytes, packed))
        })?;
        for (data, bytes) in packed.iter().zip(buffer[..count].chunks(BLOCK_SIZE)) {
            output.write_all(data)?;
            frames.push((data.len() as u32, bytes.len() as u32));
            written += data.len() as u64;
        }
        done += count as u64;
        progress(done, written);
    }
    output.write_all(&seek_table(&frames)?)?;
    output.as_file().sync_all()?;
    output
        .persist_noclobber(target)
        .map_err(|e| Error::open(target.display(), e.error))?;
    Ok(())
}
