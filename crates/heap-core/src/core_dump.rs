use crate::{
    Error, Result,
    bytes::{ReadBytes, align_up},
    compressed::Source,
    native_memory::{HEAP_MAX, arena_heap_end},
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Platform {
    #[serde(rename = "Linux x64")]
    LinuxX64,
    #[serde(rename = "Linux arm64")]
    LinuxArm64,
    #[serde(rename = "macOS arm64")]
    MacOSArm64,
}
impl Platform {
    pub fn is_linux(self) -> bool {
        self != Self::MacOSArm64
    }
    pub fn is_arm64(self) -> bool {
        self != Self::LinuxX64
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::LinuxX64 => "Linux x64",
            Self::LinuxArm64 => "Linux arm64",
            Self::MacOSArm64 => "macOS arm64",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub address: u64,
    pub size: u64,
    pub file_offset: u64,
    pub writable: bool,
    pub executable: bool,
}
impl Segment {
    pub fn end(&self) -> u64 {
        self.address + self.size
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMapping {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub path: String,
    pub deleted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub tid: i32,
    pub registers: Vec<u64>,
    pub stack_pointer: u64,
    pub instruction_pointer: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_pointer: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    pub platform: Platform,
    pub segments: Vec<Segment>,
    pub files: Vec<FileMapping>,
    pub threads: Vec<Thread>,
    pub exited_thread_stacks: HashSet<usize>,
    pub shared_cache_ranges: Vec<(u64, u64)>,
    #[serde(default)]
    pub residency: Option<crate::smaps::CapturedResidency>,
}

pub struct CoreDump {
    pub path: PathBuf,
    pub source: Source,
    pub layout: Layout,
}
impl std::ops::Deref for CoreDump {
    type Target = Layout;
    fn deref(&self) -> &Layout {
        &self.layout
    }
}

impl CoreDump {
    pub fn open(path: &Path, directory: Option<&Path>, layout: Option<Layout>) -> Result<Self> {
        Self::open_with_progress(path, directory, layout, crate::progress::Progress::none())
    }

    pub(crate) fn open_with_progress(
        path: &Path,
        directory: Option<&Path>,
        layout: Option<Layout>,
        progress: crate::progress::Progress<'_>,
    ) -> Result<Self> {
        progress.report(0.0);
        let source = Source::open_with_progress(path, directory, progress.portion(0.0, 0.9))?;
        let progress = progress.named("Reading dump headers");
        progress.report(0.9);
        let saved = layout.is_some();
        let mut dump = Self {
            path: path.to_owned(),
            source,
            layout: layout.unwrap_or(Layout {
                platform: Platform::LinuxX64,
                segments: vec![],
                files: vec![],
                threads: vec![],
                exited_thread_stacks: HashSet::new(),
                shared_cache_ranges: vec![],
                residency: None,
            }),
        };
        if !saved {
            dump.parse()?;
        } else if dump.layout.segments.iter().any(|s| {
            s.address.checked_add(s.size).is_none()
                || s.file_offset
                    .checked_add(s.size)
                    .is_none_or(|end| end > dump.source.len() as u64)
        }) || dump
            .layout
            .segments
            .windows(2)
            .any(|s| s[0].address > s[1].address)
            || dump.layout.files.iter().any(|f| f.start > f.end)
            || dump
                .layout
                .shared_cache_ranges
                .iter()
                .any(|&(start, end)| start > end)
        {
            return Err(Error::cache("invalid cached core layout"));
        }
        dump.check()?;
        progress.report(1.0);
        Ok(dump)
    }

    pub fn check(&self) -> Result<()> {
        self.source.check()
    }
    fn error(&self, message: &str) -> Error {
        Error::open(self.path.display(), message)
    }
    fn file(&self, offset: u64, count: u64) -> Result<&[u8]> {
        let bytes = usize::try_from(offset)
            .ok()
            .zip(usize::try_from(count).ok())
            .and_then(|(o, n)| self.source.bytes(o, n));
        match bytes {
            Some(b) => Ok(b),
            None => {
                self.check()?;
                Err(self.error("a core header points outside the file"))
            }
        }
    }

    fn parse(&mut self) -> Result<()> {
        let header = self.file(0, self.source.len().min(64) as u64)?;
        if header.len() >= 64
            && header.u32(0) == Some(0x464c457f)
            && header.u8(4) == Some(2)
            && header.u8(5) == Some(1)
            && header.u16(16) == Some(4)
            && matches!(header.u16(18), Some(62 | 183))
        {
            self.layout.platform = if header.u16(18) == Some(62) {
                Platform::LinuxX64
            } else {
                Platform::LinuxArm64
            };
            self.parse_elf()?;
            self.split_at_glibc_structures();
        } else if header.len() > 32
            && header.u32(0) == Some(0xfeedfacf)
            && header.u32(4) == Some(0x100000c)
            && header.u32(12) == Some(4)
        {
            self.layout.platform = Platform::MacOSArm64;
            self.parse_mach_o()?;
            self.find_images();
        } else {
            return Err(Error(format!(
                "{} is not a Linux x64 or arm64 ELF core or a macOS arm64 Mach-O core",
                self.path.display()
            )));
        }
        Ok(())
    }

    fn parse_elf(&mut self) -> Result<()> {
        let header = self.file(0, 64)?;
        let size = header.u16(0x36).unwrap() as usize;
        let count = header.u16(0x38).unwrap() as usize;
        let offset = header.u64(0x20).unwrap();
        if size < 56 || count == 0 || count == 65535 {
            return Err(self.error("invalid or unsupported ELF program table"));
        }
        self.file(offset, (size * count) as u64)?;
        for i in 0..count {
            let p = self.file(offset + (i * size) as u64, 56)?;
            let kind = p.u32(0).unwrap();
            let flags = p.u32(4).unwrap();
            let file_offset = p.u64(8).unwrap();
            let address = p.u64(16).unwrap();
            let length = p.u64(32).unwrap();
            let memory_size = p.u64(40).unwrap();
            if kind == 1 && length > 0 {
                self.validate_file_range(file_offset, length)?;
                if length > memory_size || address.checked_add(length).is_none() {
                    return Err(self.error("invalid ELF memory range"));
                }
                self.layout.segments.push(Segment {
                    address,
                    size: length,
                    file_offset,
                    writable: flags & 2 != 0,
                    executable: flags & 1 != 0,
                });
            } else if kind == 4 {
                self.parse_notes(file_offset, length)?;
            }
        }
        self.layout.segments.sort_by_key(|s| s.address);
        Ok(())
    }

    fn validate_file_range(&self, offset: u64, length: u64) -> Result<()> {
        if offset > self.source.len() as u64 || length > self.source.len() as u64 - offset {
            Err(self.error("a core header points outside the file"))
        } else {
            Ok(())
        }
    }

    fn parse_notes(&mut self, offset: u64, length: u64) -> Result<()> {
        let mut p = 0u64;
        self.validate_file_range(offset, length)?;
        while length.saturating_sub(p) >= 12 {
            let header = self.file(offset + p, 12)?;
            let name_size = header.u32(0).unwrap() as u64;
            let desc_size = header.u32(4).unwrap() as u64;
            let kind = header.u32(8).unwrap();
            let desc = p + 12 + align_up(name_size, 4).unwrap();
            if desc > length || desc_size > length - desc {
                return Err(self.error("truncated ELF note"));
            }
            let data = self.file(offset + desc, desc_size)?;
            match kind {
                1 => {
                    let arm = self.platform == Platform::LinuxArm64;
                    let count = if arm { 34 } else { 27 };
                    if data.len() < 112 + count * 8 {
                        return Err(self.error("truncated thread registers"));
                    }
                    let tid = data.u32(32).unwrap() as i32;
                    let registers: Vec<_> =
                        (0..count).map(|i| data.u64(112 + i * 8).unwrap()).collect();
                    self.layout.threads.push(Thread {
                        tid,
                        stack_pointer: registers[if arm { 31 } else { 19 }],
                        instruction_pointer: registers[if arm { 32 } else { 16 }],
                        thread_pointer: if arm { None } else { Some(registers[21]) },
                        registers,
                    });
                }
                0x46494c45 => {
                    if data.len() < 16 || data.u64(0).unwrap() > ((data.len() - 16) / 24) as u64 {
                        return Err(self.error("invalid mapped-file table"));
                    }
                    let count = data.u64(0).unwrap() as usize;
                    let page_size = data.u64(8).unwrap();
                    if page_size == 0 {
                        return Err(self.error("invalid page size in mapped-file table"));
                    }
                    let mut names = 16 + count * 24;
                    let mut files = Vec::with_capacity(count);
                    for i in 0..count {
                        let end_name = data
                            .get(names..)
                            .and_then(|b| memchr::memchr(0, b))
                            .ok_or_else(|| self.error("unterminated mapped-file path"))?
                            + names;
                        let mut path = String::from_utf8_lossy(&data[names..end_name]).into_owned();
                        names = end_name + 1;
                        let deleted = path.ends_with(" (deleted)");
                        if deleted {
                            path.truncate(path.len() - " (deleted)".len());
                        }
                        let at = 16 + i * 24;
                        let start = data.u64(at).unwrap();
                        let end = data.u64(at + 8).unwrap();
                        let file_offset = data
                            .u64(at + 16)
                            .unwrap()
                            .checked_mul(page_size)
                            .ok_or_else(|| self.error("invalid mapped-file range"))?;
                        if start > end {
                            return Err(self.error("invalid mapped-file range"));
                        }
                        files.push(FileMapping {
                            start,
                            end,
                            file_offset,
                            path,
                            deleted,
                        });
                    }
                    self.layout.files.extend(files);
                }
                _ => {}
            }
            p = desc + align_up(desc_size, 4).unwrap();
        }
        Ok(())
    }

    fn parse_mach_o(&mut self) -> Result<()> {
        let h = self.file(0, 32)?;
        let count = h.u32(16).unwrap() as usize;
        let bytes = h.u32(20).unwrap() as u64;
        self.validate_file_range(32, bytes)?;
        if count == 0 || count as u64 > bytes / 8 {
            return Err(self.error("invalid Mach-O command table"));
        }
        let (mut p, end) = (32u64, 32 + bytes);
        for _ in 0..count {
            if end.saturating_sub(p) < 8 {
                return Err(self.error("truncated Mach-O command"));
            }
            let h = self.file(p, 8)?;
            let kind = h.u32(0).unwrap();
            let size = h.u32(4).unwrap() as u64;
            if size < 8 || size > end - p {
                return Err(self.error("invalid Mach-O command size"));
            }
            if kind == 0x19 {
                if size < 72 {
                    return Err(self.error("truncated Mach-O segment"));
                }
                let h = self.file(p, 72)?;
                let (address, memory_size, file_offset, length, prot) = (
                    h.u64(24).unwrap(),
                    h.u64(32).unwrap(),
                    h.u64(40).unwrap(),
                    h.u64(48).unwrap(),
                    h.u32(60).unwrap(),
                );
                if length > 0 {
                    self.validate_file_range(file_offset, length)?;
                    if length > memory_size || address.checked_add(length).is_none() {
                        return Err(self.error("invalid Mach-O memory range"));
                    }
                    self.layout.segments.push(Segment {
                        address,
                        size: length,
                        file_offset,
                        writable: prot & 2 != 0,
                        executable: prot & 4 != 0,
                    });
                }
            } else if kind == 0x31 {
                if size < 40 {
                    return Err(self.error("truncated Mach-O note"));
                }
                let h = self.file(p, 40)?;
                if &h[8..24] == crate::smaps::MACH_NOTE_OWNER {
                    let (offset, length) = (h.u64(24).unwrap(), h.u64(32).unwrap());
                    self.validate_file_range(offset, length)?;
                    if length > 64 << 20 {
                        return Err(self.error("residency note is too large"));
                    }
                    let note: crate::smaps::CapturedResidency =
                        serde_json::from_slice(self.file(offset, length)?)
                            .map_err(|e| self.error(&format!("invalid residency note: {e}")))?;
                    if note.mappings.iter().any(|m| {
                        m.end <= m.start
                            || m.rss > m.end - m.start
                            || m.swap > m.end - m.start
                            || m.private_bytes > m.rss
                    }) {
                        return Err(self.error("invalid residency mapping"));
                    }
                    if note.version == 1 {
                        self.layout.residency = Some(note);
                    }
                }
            } else if kind == 4 {
                let mut q = p + 8;
                while p + size - q >= 8 {
                    let h = self.file(q, 8)?;
                    let (flavor, words) = (h.u32(0).unwrap(), h.u32(4).unwrap() as u64);
                    if words > (p + size - q - 8) / 4 {
                        return Err(self.error("truncated Mach-O thread state"));
                    }
                    if flavor == 6 && words >= 66 {
                        let h = self.file(q + 8, 33 * 8)?;
                        let registers: Vec<_> = (0..33).map(|i| h.u64(i * 8).unwrap()).collect();
                        self.layout.threads.push(Thread {
                            tid: self.threads.len() as i32,
                            stack_pointer: registers[31],
                            instruction_pointer: registers[32],
                            registers,
                            thread_pointer: None,
                        });
                    }
                    q += 8 + words * 4;
                }
            }
            p += size;
        }
        self.layout.segments.sort_by_key(|s| s.address);
        Ok(())
    }

    fn find_images(&mut self) {
        for segment in self.segments.clone().into_iter().filter(|s| s.size >= 4096) {
            let address = segment.address;
            if self.u32(address) == Some(0xfeedfacf) {
                self.add_mach_o_image(address);
            } else if self.u64(address).unwrap_or(0) & 0x00ff_ffff_ffff_ffff
                == 0x0031_765f_646c_7964
            {
                let offset = self.u32(address + 16).unwrap_or(0) as u64;
                let count = self.u32(address + 20).unwrap_or(0).min(64);
                let mut added = false;
                if count > 0
                    && let Some(first) = self.u64(address.wrapping_add(offset))
                {
                    let slide = address.wrapping_sub(first);
                    if let (Some(start), Some(size)) =
                        (self.u64(address + 0xe0), self.u64(address + 0xe8))
                    {
                        let start = start.wrapping_add(slide);
                        if size > 0
                            && size < 1 << 36
                            && let Some(end) = start.checked_add(size)
                        {
                            self.layout.shared_cache_ranges.push((start, end));
                        }
                    }
                    for i in 0..count {
                        let entry = address.wrapping_add(offset).wrapping_add(i as u64 * 32);
                        if let (Some(start), Some(size)) =
                            (self.u64(entry), self.u64(entry.wrapping_add(8)))
                        {
                            let start = start.wrapping_add(slide);
                            if size > 0
                                && let Some(end) = start.checked_add(size)
                            {
                                self.layout.shared_cache_ranges.push((start, end));
                                added = true;
                            }
                        }
                    }
                }
                if !added {
                    self.layout
                        .shared_cache_ranges
                        .push((address, segment.end()));
                }
            }
        }
        self.layout.shared_cache_ranges.sort_unstable();
        let mut assemblies = Vec::new();
        let mut cursor = Cursor::new(self, true);
        for seg in self
            .segments
            .iter()
            .filter(|s| !self.in_shared_cache(s.address) && self.file_mapping(s.address).is_none())
        {
            let mut at = seg.address;
            while at.checked_add(0x200).is_some_and(|end| end <= seg.end()) {
                if cursor.u16(at) == Some(0x5a4d)
                    && let Some(pe) = cursor.u32(at + 0x3c).filter(|&n| n < 4096)
                    && cursor.u32(at + pe as u64) == Some(0x4550)
                {
                    assemblies.push(at);
                }
                let Some(next) = at.checked_add(4096) else {
                    break;
                };
                at = next;
            }
        }
        drop(cursor);
        for start in assemblies {
            let Some(pe) = self.u32(start + 0x3c).filter(|&n| n < 4096) else {
                continue;
            };
            let Some(size) = self.u32(start + pe as u64 + 24 + 56) else {
                continue;
            };
            let Some(limit) = start.checked_add(size as u64) else {
                continue;
            };
            let mut end = start;
            while end < limit {
                let Some(i) = self.segment_index(end) else {
                    break;
                };
                end = self.segments[i].end();
            }
            end = end.min(limit);
            if end <= start {
                continue;
            }
            if let Some(bytes) = self.bytes(start, (end - start) as usize)
                && let Some(metadata) =
                    crate::metadata::AssemblyMetadata::from_bytes(bytes.into_owned())
                && let Some(name) = metadata.assembly_name()
                && let Some(length) = metadata.image_length().filter(|&n| n > 0)
            {
                self.layout.files.push(FileMapping {
                    start,
                    end: end.min(start.saturating_add(length as u64)),
                    file_offset: 0,
                    path: format!("{name}.dll"),
                    deleted: false,
                });
            }
        }
        self.layout.files.sort_by_key(|f| f.start);
    }

    fn add_mach_o_image(&mut self, address: u64) {
        let (Some(kind), Some(count), Some(size)) = (
            self.u32(address + 12),
            self.u32(address + 16),
            self.u32(address + 20),
        ) else {
            return;
        };
        if ![2, 6, 8].contains(&kind) {
            return;
        }
        let Some(data) = self.bytes(address + 32, size.min(1 << 20) as usize) else {
            return;
        };
        let mut name = if kind == 2 {
            "executable".to_owned()
        } else {
            format!("dylib at 0x{address:x}")
        };
        let (mut ranges, mut slide, mut p) = (Vec::new(), None, 0usize);
        for _ in 0..count {
            let (Some(cmd), Some(size)) = (data.u32(p), data.u32(p + 4).map(|n| n as usize)) else {
                break;
            };
            if size < 8 || size > data.len() - p {
                break;
            }
            if cmd == 0x19 && size >= 72 {
                let segment_name = &data[p + 8..p + 24];
                let segment_name = &segment_name[..memchr::memchr(0, segment_name).unwrap_or(16)];
                let (start, size, offset) = (
                    data.u64(p + 24).unwrap(),
                    data.u64(p + 32).unwrap(),
                    data.u64(p + 40).unwrap(),
                );
                if segment_name == b"__TEXT" {
                    slide = Some(address.wrapping_sub(start));
                }
                if segment_name != b"__PAGEZERO" && size > 0 {
                    ranges.push((start, size, offset));
                }
            } else if cmd == 0xd && size >= 12 {
                let offset = data.u32(p + 8).unwrap() as usize;
                if offset < size {
                    let bytes = &data[p + offset..p + size];
                    name = String::from_utf8_lossy(
                        &bytes[..memchr::memchr(0, bytes).unwrap_or(bytes.len())],
                    )
                    .into_owned();
                }
            }
            p += size;
        }
        if let Some(slide) = slide {
            for (start, size, offset) in ranges {
                let start = start.wrapping_add(slide);
                if let Some(end) = start.checked_add(size) {
                    self.layout.files.push(FileMapping {
                        start,
                        end,
                        file_offset: offset,
                        path: name.clone(),
                        deleted: false,
                    });
                }
            }
        }
    }

    pub fn segment_index(&self, address: u64) -> Option<usize> {
        let i = self
            .segments
            .partition_point(|s| s.address <= address)
            .checked_sub(1)?;
        (address < self.segments[i].end()).then_some(i)
    }
    pub fn slice(&self, address: u64, count: usize) -> Option<&[u8]> {
        let s = &self.segments[self.segment_index(address)?];
        if address.checked_add(count as u64)? > s.end() {
            return None;
        }
        self.source
            .bytes((s.file_offset + address - s.address) as usize, count)
    }
    pub fn bytes(&self, address: u64, count: usize) -> Option<Cow<'_, [u8]>> {
        if let Some(bytes) = self.slice(address, count) {
            return Some(Cow::Borrowed(bytes));
        }
        address.checked_add(count as u64)?;
        let mut result = Vec::new();
        let mut at = address;
        while result.len() < count {
            let s = &self.segments[self.segment_index(at)?];
            let n = ((s.end() - at) as usize).min(count - result.len());
            result.extend_from_slice(
                self.source
                    .bytes((s.file_offset + at - s.address) as usize, n)?,
            );
            at += n as u64;
        }
        Some(Cow::Owned(result))
    }
    #[inline]
    pub fn u64(&self, at: u64) -> Option<u64> {
        self.slice(at, 8)?.u64(0)
    }
    #[inline]
    pub fn u32(&self, at: u64) -> Option<u32> {
        self.slice(at, 4)?.u32(0)
    }
    #[inline]
    pub fn u16(&self, at: u64) -> Option<u16> {
        self.slice(at, 2)?.u16(0)
    }
    #[inline]
    pub fn u8(&self, at: u64) -> Option<u8> {
        self.slice(at, 1)?.u8(0)
    }
    pub fn c_string(&self, at: u64, maximum: usize) -> Option<String> {
        Cursor::new(self, false).c_string(at, maximum)
    }
    pub fn file_mapping(&self, address: u64) -> Option<&FileMapping> {
        self.files
            .iter()
            .find(|f| f.start <= address && address < f.end)
    }
    pub fn in_shared_cache(&self, address: u64) -> bool {
        self.shared_cache_ranges
            .iter()
            .any(|&(a, b)| a <= address && address < b)
    }
    pub fn library(&self, suffix: &str) -> Vec<&FileMapping> {
        let mut result: Vec<_> = self
            .files
            .iter()
            .filter(|f| f.path.ends_with(suffix))
            .collect();
        result.sort_by_key(|f| f.start);
        result
    }
    pub fn scan(&self, start: u64, end: u64, mut body: impl FnMut(u64, &[u8])) {
        let first = self.segments.partition_point(|s| s.end() <= start);
        for s in &self.segments[first..] {
            if s.address >= end {
                break;
            }
            let at = s.address.max(start);
            let stop = s.end().min(end);
            self.source.scan(
                (s.file_offset + at - s.address) as usize,
                (stop - at) as usize,
                |offset, bytes| {
                    body(s.address + offset as u64 - s.file_offset, bytes);
                },
            );
        }
    }

    pub fn find_bytes(&self, start: u64, end: u64, needle: &[u8]) -> Option<u64> {
        if needle.is_empty() {
            return None;
        }
        let mut found = None;
        let mut tail = Vec::new();
        let mut previous_end = None;
        self.scan(start, end, |address, bytes| {
            if found.is_some() {
                return;
            }
            if previous_end != Some(address) {
                tail.clear();
            }
            if !tail.is_empty() {
                let tail_length = tail.len();
                tail.extend_from_slice(&bytes[..bytes.len().min(needle.len() - 1)]);
                if let Some(index) = memchr::memmem::find(&tail, needle) {
                    found = Some(address - tail_length as u64 + index as u64);
                    return;
                }
                tail.truncate(tail_length);
            }
            if let Some(index) = memchr::memmem::find(bytes, needle) {
                found = Some(address + index as u64);
                return;
            }
            let keep = needle.len() - 1;
            if bytes.len() >= keep {
                tail.clear();
                tail.extend_from_slice(&bytes[bytes.len() - keep..]);
            } else {
                tail.extend_from_slice(bytes);
                if tail.len() > keep {
                    tail.drain(..tail.len() - keep);
                }
            }
            previous_end = Some(address + bytes.len() as u64);
        });
        found
    }

    fn split_at_glibc_structures(&mut self) {
        let (mut block_offset, mut tid_offset) = (None, None);
        let mut c = Cursor::new(self, true);
        let valid_block = |b: u64, size: u64, anchor: u64| {
            b & 4095 == 0
                && size & 4095 == 0
                && (65536..=1 << 30).contains(&size)
                && b < anchor
                && b.checked_add(size).is_some_and(|end| anchor < end)
        };
        if self.platform == Platform::LinuxX64 {
            for t in &self.threads {
                let Some(tp) = t.thread_pointer.filter(|&v| v != 0) else {
                    continue;
                };
                if block_offset.is_none() {
                    for o in (0..0x1000).step_by(8) {
                        if let (Some(b), Some(size)) =
                            (c.u64(tp.wrapping_add(o)), c.u64(tp.wrapping_add(o + 8)))
                            && valid_block(b, size, tp)
                        {
                            block_offset = Some(o as i64);
                            break;
                        }
                    }
                }
                if tid_offset.is_none() {
                    for o in (0..0x1000).step_by(4) {
                        if c.u32(tp.wrapping_add(o)) == Some(t.tid as u32) {
                            tid_offset = Some(o as i64);
                            break;
                        }
                    }
                }
            }
        } else {
            let mut votes: HashMap<i64, usize> = HashMap::new();
            for t in &self.threads {
                let Some(i) = self.segment_index(t.stack_pointer) else {
                    continue;
                };
                let segment = &self.segments[i];
                let mut q = segment.address.max(t.stack_pointer) & !3;
                while q <= segment.end().saturating_sub(4) {
                    if c.u32(q) == Some(t.tid as u32) {
                        for o in (-0x800i64..0x800).step_by(8) {
                            let Some(at) = q.checked_add_signed(o).map(|n| n & !7) else {
                                continue;
                            };
                            if let (Some(b), Some(size)) = (c.u64(at), c.u64(at + 8))
                                && b <= t.stack_pointer
                                && valid_block(b, size, q)
                                && t.stack_pointer < b + size
                            {
                                *votes.entry(at as i64 - q as i64).or_default() += 1;
                            }
                        }
                    }
                    q += 4;
                }
            }
            if let Some((&offset, &count)) = votes.iter().max_by_key(|&(&o, &n)| (n, -o))
                && count >= 2
            {
                block_offset = Some(offset);
                tid_offset = Some(0);
            }
        }
        let (mut cuts, mut exited) = (BTreeSet::new(), Vec::new());
        if let (Some(block_offset), Some(tid_offset)) = (block_offset, tid_offset) {
            let x64 = self.platform == Platform::LinuxX64;
            for seg in self
                .segments
                .iter()
                .filter(|s| s.writable && s.size >= 65536 && self.file_mapping(s.address).is_none())
            {
                let mut off = if x64 {
                    0
                } else {
                    (-block_offset).max(0) as u64
                };
                while off + block_offset.max(tid_offset).max(0) as u64 + 16 <= seg.size {
                    let a = seg.address + off;
                    if (!x64 || (c.u64(a) == Some(a) && c.u64(a + 16) == Some(a)))
                        && let Some(at) = a.checked_add_signed(block_offset)
                        && let (Some(block), Some(size)) = (c.u64(at), c.u64(at + 8))
                        && valid_block(block, size, a)
                        && (x64 || block + size - a < 0x40000)
                    {
                        cuts.insert(block);
                        cuts.insert(block + size);
                        if a.checked_add_signed(tid_offset).and_then(|at| c.u32(at)) == Some(0) {
                            exited.push((block, block + size));
                        }
                    }
                    off += if x64 { 64 } else { 4 };
                }
            }
        }
        for seg in self
            .segments
            .iter()
            .filter(|s| s.writable && s.size > 1 << 20 && self.file_mapping(s.address).is_none())
        {
            let Some(mut a) = align_up(seg.address, HEAP_MAX) else {
                continue;
            };
            while a < seg.end() {
                if let Some(end) = arena_heap_end(&mut c, a) {
                    cuts.insert(a);
                    cuts.insert(end);
                }
                let Some(next) = a.checked_add(HEAP_MAX) else {
                    break;
                };
                a = next;
            }
        }
        drop(c);
        if !cuts.is_empty() {
            let mut split = Vec::new();
            for seg in &self.segments {
                let mut start = seg.address;
                for &cut in cuts.range((
                    std::ops::Bound::Excluded(seg.address),
                    std::ops::Bound::Excluded(seg.end()),
                )) {
                    split.push(Segment {
                        address: start,
                        size: cut - start,
                        file_offset: seg.file_offset + start - seg.address,
                        ..seg.clone()
                    });
                    start = cut;
                }
                split.push(Segment {
                    address: start,
                    size: seg.end() - start,
                    file_offset: seg.file_offset + start - seg.address,
                    ..seg.clone()
                });
            }
            self.layout.segments = split;
        }
        let with_thread: HashSet<_> = self
            .threads
            .iter()
            .filter_map(|t| self.segment_index(t.stack_pointer))
            .collect();
        let mut stacks = HashSet::new();
        for (start, end) in exited {
            for (i, seg) in self.segments.iter().enumerate() {
                if seg.address >= start && seg.end() <= end && !with_thread.contains(&i) {
                    stacks.insert(i);
                }
            }
        }
        self.layout.exited_thread_stacks = stacks;
    }
}

pub struct Cursor<'a> {
    dump: &'a CoreDump,
    scratch: bool,
    data: Cow<'a, [u8]>,
    start: u64,
    end: u64,
    data_offset: usize,
}
impl<'a> Cursor<'a> {
    pub fn new(dump: &'a CoreDump, scratch: bool) -> Self {
        Self {
            dump,
            scratch,
            data: Cow::Borrowed(&[]),
            start: 0,
            end: 0,
            data_offset: 0,
        }
    }
    pub fn slice(&mut self, address: u64, count: usize) -> Option<&[u8]> {
        let end = address.checked_add(count as u64)?;
        if address < self.start || end > self.end || self.data.is_empty() {
            let segment = &self.dump.segments[self.dump.segment_index(address)?];
            if end > segment.end() {
                return None;
            }
            let offset = segment.file_offset + address - segment.address;
            let scratch = if self.scratch {
                Some(match std::mem::take(&mut self.data) {
                    Cow::Owned(bytes) => bytes,
                    Cow::Borrowed(_) => Vec::new(),
                })
            } else {
                None
            };
            let (start, data) = self.dump.source.window(offset as usize, count, scratch)?;
            let from = (start as u64).max(segment.file_offset);
            let to = (start as u64 + data.len() as u64).min(segment.file_offset + segment.size);
            self.start = segment.address + from - segment.file_offset;
            self.end = segment.address + to - segment.file_offset;
            self.data_offset = from as usize - start;
            self.data = data;
        }
        let offset = self.data_offset + (address - self.start) as usize;
        self.data.get(offset..offset + count)
    }
    #[inline]
    pub fn u64(&mut self, at: u64) -> Option<u64> {
        self.slice(at, 8)?.u64(0)
    }
    #[inline]
    pub fn u32(&mut self, at: u64) -> Option<u32> {
        self.slice(at, 4)?.u32(0)
    }
    #[inline]
    pub fn u16(&mut self, at: u64) -> Option<u16> {
        self.slice(at, 2)?.u16(0)
    }
    #[inline]
    pub fn u8(&mut self, at: u64) -> Option<u8> {
        self.slice(at, 1)?.u8(0)
    }
    pub fn holds(&self, at: u64, count: usize) -> bool {
        self.dump.segment_index(at).is_some_and(|i| {
            at.checked_add(count as u64)
                .is_some_and(|end| end <= self.dump.segments[i].end())
        })
    }
    pub fn c_string(&mut self, at: u64, maximum: usize) -> Option<String> {
        let mut bytes = Vec::new();
        for i in 0..maximum {
            let byte = self.u8(at.checked_add(i as u64)?)?;
            if byte == 0 {
                return String::from_utf8(bytes).ok();
            }
            if !(0x20..=0x7e).contains(&byte) {
                return None;
            }
            bytes.push(byte);
        }
        None
    }
}
