use crate::{
    bytes::align_up,
    core_dump::{CoreDump, Cursor, FileMapping, Platform},
    heap::ManagedHeap,
    progress::Progress,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const HEAP_MAX: u64 = 64 << 20;
pub const STATE_TOP: u64 = 96;
pub const STATE_ATTACHED_THREADS: u64 = 2176;
pub const STATE_SIZE: u64 = 2200;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    pub writable: bool,
    pub category: String,
    pub detail: String,
    pub non_zero_bytes: u64,
}
impl Mapping {
    pub fn size(&self) -> u64 {
        self.end - self.start
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[repr(C)]
pub struct MallocHeap {
    pub start: u64,
    pub end: u64,
    pub main_arena: bool,
    pub in_use_bytes: u64,
    pub in_use_chunks: usize,
    pub free_bytes: u64,
    pub free_chunks: usize,
    pub largest_free: u64,
    pub top_bytes: u64,
    pub walk_complete: bool,
    pub arena: u64,
    pub header_bytes: u64,
    pub free_resident: u64,
    pub top_resident: u64,
    pub trim_releasable: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeMemory {
    pub mappings: Vec<Mapping>,
    pub malloc_heaps: Vec<MallocHeap>,
    pub glibc_version: Option<String>,
    pub mmapped_chunks: usize,
    pub mmapped_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Category {
    pub name: String,
    pub mappings: usize,
    pub bytes: u64,
    pub non_zero_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MallocChunk {
    pub start: u64,
    pub size: u64,
    pub state: String,
}

impl NativeMemory {
    pub fn new(dump: &CoreDump, heap: &ManagedHeap, handle_segments: &[u64]) -> Self {
        Self::new_with_progress(dump, heap, handle_segments, Progress::none())
    }

    pub(crate) fn new_with_progress(
        dump: &CoreDump,
        heap: &ManagedHeap,
        handle_segments: &[u64],
        progress: Progress<'_>,
    ) -> Self {
        progress.report(0.0);
        let handles: HashSet<_> = handle_segments.iter().copied().collect();
        let stacks: HashMap<_, _> = dump
            .threads
            .iter()
            .filter_map(|t| dump.segment_index(t.stack_pointer).map(|i| (i, t.tid)))
            .collect();
        let exe_path = dump
            .files
            .iter()
            .find(|f| {
                !f.path.ends_with(".so")
                    && !f.path.contains(".so.")
                    && !f.path.ends_with(".dll")
                    && !f.path.starts_with("/memfd:")
            })
            .map(|f| &f.path);
        let executable = dump.files.iter().rev().find(|f| Some(&f.path) == exe_path);
        let mut cursor = Cursor::new(dump, true);
        let mut memory = Self {
            mappings: vec![],
            malloc_heaps: vec![],
            glibc_version: glibc_version(dump),
            mmapped_chunks: 0,
            mmapped_bytes: 0,
        };
        for (i, seg) in dump.segments.iter().enumerate() {
            progress.portion(0.0, 0.3).items(i, dump.segments.len());
            let (category, detail) =
                classify(dump, &mut cursor, heap, i, &handles, &stacks, executable);
            if category == "malloc heap"
                && let Some(h) = walk_malloc_heap(
                    dump,
                    &mut cursor,
                    seg.address,
                    seg.end(),
                    detail.starts_with("main"),
                )
            {
                memory.malloc_heaps.push(h);
            }
            if category == "malloc large allocation" {
                memory.mmapped_chunks += 1;
                memory.mmapped_bytes += seg.size;
            }
            memory.mappings.push(Mapping {
                start: seg.address,
                end: seg.end(),
                writable: seg.writable,
                category,
                detail,
                non_zero_bytes: 0,
            });
        }
        let scanned = progress
            .portion(0.3, 1.0)
            .counter(memory.mappings.iter().map(|m| m.size()).sum());
        memory.mappings.par_iter_mut().for_each(|m| {
            m.non_zero_bytes = non_zero_bytes_with_progress(dump, m.start, m.end, &scanned);
        });
        progress.report(1.0);
        memory
    }
    pub fn categories(&self) -> Vec<Category> {
        let mut categories: BTreeMap<&str, Category> = BTreeMap::new();
        for m in &self.mappings {
            let c = categories.entry(&m.category).or_insert_with(|| Category {
                name: m.category.clone(),
                ..Category::default()
            });
            c.mappings += 1;
            c.bytes += m.size();
            c.non_zero_bytes += m.non_zero_bytes;
        }
        let mut values: Vec<_> = categories.into_values().collect();
        values.sort_by_key(|c| std::cmp::Reverse(c.bytes));
        values
    }
    pub fn malloc_chunk(&self, address: u64, dump: &CoreDump) -> Option<MallocChunk> {
        if let Some(m) = self.mappings.iter().find(|m| {
            address >= m.start && address < m.end && m.category == "malloc large allocation"
        }) {
            return Some(MallocChunk {
                start: m.start,
                size: m.size(),
                state: "in use, own mapping".into(),
            });
        }
        let h = self
            .malloc_heaps
            .iter()
            .find(|h| address >= h.start && address < h.end)?;
        let (mut c, top) = first_chunk(dump, h.start, h.end, h.main_arena)?;
        if address < c {
            return None;
        }
        let mut cursor = Cursor::new(dump, true);
        while h.end.saturating_sub(c) >= 16 {
            let size = cursor.u64(c + 8)? & !7;
            if size < 32 || size > h.end - c || size & 15 != 0 {
                return None;
            }
            if c == top || c + size >= h.end {
                return Some(MallocChunk {
                    start: c,
                    size: h.end - c,
                    state: "top chunk, never handed out".into(),
                });
            }
            let in_use = cursor.u64(c + size + 8).unwrap_or(0) & 1 != 0;
            if address < c + size + if in_use { 8 } else { 0 } {
                return Some(MallocChunk {
                    start: c,
                    size,
                    state: if in_use {
                        "in use, or freed to a tcache or fastbin list"
                    } else {
                        "free"
                    }
                    .into(),
                });
            }
            c += size;
        }
        None
    }
}

pub fn non_zero_bytes(dump: &CoreDump, start: u64, end: u64) -> u64 {
    non_zero_bytes_with_progress(dump, start, end, &|_| {})
}

fn non_zero_bytes_with_progress(
    dump: &CoreDump,
    start: u64,
    end: u64,
    progress: &impl Fn(u64),
) -> u64 {
    let (mut total, mut counted) = (0, None);
    dump.scan(start, end, |address, bytes| {
        let mut at = 0;
        let mut reported = 0;
        while at < bytes.len() {
            let page = (address + at as u64 - start) / 4096;
            let page_start = start + page * 4096;
            let next = ((page_start + 4096).min(end) - address).min(bytes.len() as u64) as usize;
            if counted != Some(page) && bytes[at..next].iter().any(|&b| b != 0) {
                counted = Some(page);
                total += 4096.min(end - page_start);
            }
            at = next;
            if at - reported >= 1 << 20 {
                progress((at - reported) as u64);
                reported = at;
            }
        }
        progress((at - reported) as u64);
    });
    total
}

fn filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
fn classify(
    dump: &CoreDump,
    cursor: &mut Cursor<'_>,
    heap: &ManagedHeap,
    index: usize,
    handles: &HashSet<u64>,
    stacks: &HashMap<usize, i32>,
    executable: Option<&FileMapping>,
) -> (String, String) {
    let seg = &dump.segments[index];
    let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
    if heap.owns_memory(seg.address, seg.end()) {
        return pair(heap.label(), "regions and objects");
    }
    if handles.contains(&seg.address) {
        return pair("GC handle table", "64 KB handle table segment");
    }
    if let Some(tid) = stacks.get(&index) {
        return ("thread stack".into(), format!("thread {tid}"));
    }
    if dump.platform == Platform::MacOSArm64 {
        if dump.in_shared_cache(seg.address) {
            return pair("dyld shared cache", "system libraries");
        }
        if let Some(f) = dump.file_mapping(seg.address) {
            if f.path.ends_with(".dll") {
                return pair("assembly images", &f.path);
            }
            return (
                "native libraries".into(),
                format!(
                    "{}{}",
                    filename(&f.path),
                    if seg.writable {
                        " data"
                    } else {
                        " code and read-only data"
                    }
                ),
            );
        }
        if seg.executable {
            return pair("JIT and stub code", "executable anonymous memory");
        }
        return if seg.writable {
            pair("anonymous", "malloc zones and runtime memory")
        } else {
            pair("anonymous, read-only", "")
        };
    }
    if dump.exited_thread_stacks.contains(&index) {
        return pair("thread stack", "exited thread, kept by glibc for reuse");
    }
    if let Some(f) = dump.file_mapping(seg.address) {
        let name = filename(&f.path);
        if f.path.starts_with("/memfd:doublemapper") {
            return pair(
                "JIT and stub code (memfd)",
                if seg.writable {
                    "writable view"
                } else {
                    "executable view"
                },
            );
        }
        if f.path.starts_with("/memfd:") {
            return pair("runtime shared memory (memfd)", name);
        }
        if f.path.ends_with(".dll") {
            return pair("assembly images", name);
        }
        if f.path.ends_with(".so") || f.path.contains(".so.") {
            return (
                "native libraries".into(),
                format!(
                    "{name}{}",
                    if seg.writable {
                        " data"
                    } else {
                        " code and read-only data"
                    }
                ),
            );
        }
        return pair("other files", name);
    }
    if let Some(f) = dump.files.iter().rev().find(|f| f.end == seg.address)
        && (f.path.ends_with(".so") || f.path.contains(".so."))
    {
        return (
            "native libraries".into(),
            format!("{} .bss", filename(&f.path)),
        );
    }
    if seg.executable {
        return pair("JIT and stub code", "executable anonymous memory");
    }
    if !seg.writable {
        return pair("anonymous, read-only", "");
    }
    if arena_heap_end(cursor, seg.address).is_some() {
        return pair("malloc heap", "arena heap");
    }
    if let Some(exe) = executable
        && seg.address >= exe.end
        && seg.address - exe.end < 4 << 30
        && is_chunk_start(cursor, seg.address, seg.end())
    {
        return pair("malloc heap", "main arena (brk)");
    }
    if let (Some(prev), Some(size)) = (cursor.u64(seg.address), cursor.u64(seg.address + 8))
        && size & 2 != 0
        && prev < 4096
        && (size & !7).checked_add(prev) == Some(seg.size)
    {
        return pair("malloc large allocation", "chunk above the mmap threshold");
    }
    if seg.address & 0xffff == 0 {
        return pair(
            "anonymous, 64 KB aligned",
            "runtime reservations such as loader heaps",
        );
    }
    pair("anonymous", "")
}
pub(crate) fn arena_heap_end(c: &mut Cursor<'_>, start: u64) -> Option<u64> {
    if start & (HEAP_MAX - 1) != 0 {
        return None;
    }
    let arena = c.u64(start)?;
    let size = c.u64(start.checked_add(16)?)?;
    let protected = c.u64(start.checked_add(24)?)?;
    if arena == 0
        || arena & 7 != 0
        || !(4096..=HEAP_MAX).contains(&size)
        || size & 4095 != 0
        || protected < size
        || protected > HEAP_MAX
        || protected & 4095 != 0
    {
        return None;
    }
    start.checked_add(protected)
}
fn is_chunk_start(c: &mut Cursor<'_>, start: u64, end: u64) -> bool {
    let mut at = start;
    let mut previous_size = None;
    for _ in 0..8 {
        let Some(size) = at.checked_add(8).and_then(|p| c.u64(p)) else {
            return false;
        };
        let s = size & !7;
        if size & 6 != 0 || s < 32 || s & 15 != 0 || s > end.saturating_sub(at) {
            return false;
        }
        if size & 1 == 0 && (previous_size.is_none() || c.u64(at) != previous_size) {
            return false;
        }
        previous_size = Some(s);
        at += s;
        if end - at < 16 {
            return true;
        }
    }
    true
}
pub fn first_chunk(dump: &CoreDump, start: u64, end: u64, main: bool) -> Option<(u64, u64)> {
    if main {
        return Some((start, 0));
    }
    let arena = dump.u64(start)?;
    let top = arena
        .checked_add(STATE_TOP)
        .and_then(|p| dump.u64(p))
        .unwrap_or(0);
    Some((
        if arena >= start && arena < end {
            align_up(arena.checked_add(STATE_SIZE)?, 16)?
        } else {
            start.checked_add(48)?
        },
        top,
    ))
}
pub fn resident_bytes(c: &mut Cursor<'_>, start: u64, end: u64) -> u64 {
    let mut total = 0;
    let mut page = start & !4095;
    while page < end {
        let Some(next) = page.checked_add(4096) else {
            break;
        };
        if c.u64(page).is_some()
            && c.u64(page + 4088).is_some()
            && (0..4096).step_by(8).any(|k| c.u64(page + k) != Some(0))
        {
            total += next.min(end) - page.max(start);
        }
        page = next;
    }
    total
}
fn walk_malloc_heap(
    dump: &CoreDump,
    c: &mut Cursor<'_>,
    start: u64,
    end: u64,
    main: bool,
) -> Option<MallocHeap> {
    let (mut at, top) = first_chunk(dump, start, end, main)?;
    if at < start || at > end {
        return None;
    }
    let mut h = MallocHeap {
        start,
        end,
        main_arena: main,
        arena: if main { 0 } else { c.u64(start).unwrap_or(0) },
        header_bytes: at - start,
        ..MallocHeap::default()
    };
    let mut previous: Option<(u64, u64)> = None;
    fn count(h: &mut MallocHeap, c: &mut Cursor<'_>, at: u64, size: u64, in_use: bool) {
        if in_use {
            h.in_use_bytes += size;
            h.in_use_chunks += 1;
        } else {
            h.free_bytes += size;
            h.free_chunks += 1;
            h.largest_free = h.largest_free.max(size);
            h.free_resident += resident_bytes(c, at, at + size);
            if let Some(lo) = at.checked_add(48).and_then(|n| align_up(n, 4096)) {
                let hi = (at + size) & !4095;
                if hi > lo {
                    h.trim_releasable += resident_bytes(c, lo, hi);
                }
            }
        }
    }
    while end.saturating_sub(at) >= 16 {
        let Some(raw) = c.u64(at + 8) else {
            break;
        };
        let size = raw & !7;
        if size == 16 && !main {
            if let Some((a, s)) = previous {
                count(&mut h, c, a, s, raw & 1 != 0);
            }
            h.walk_complete = true;
            return Some(h);
        }
        if size < 32 || size > end - at || size & 15 != 0 {
            break;
        }
        if let Some((a, s)) = previous {
            count(&mut h, c, a, s, raw & 1 != 0);
        }
        if at == top || at + size >= end {
            h.top_bytes = end - at;
            h.top_resident = resident_bytes(c, at, end);
            if main
                && end - at > 128 << 10
                && let Some(lo) = align_up(at + (128 << 10), 4096)
            {
                h.trim_releasable += resident_bytes(c, lo, end);
            }
            h.walk_complete = true;
            return Some(h);
        }
        previous = Some((at, size));
        at += size;
    }
    Some(h)
}
fn glibc_version(dump: &CoreDump) -> Option<String> {
    let libc: Vec<_> = dump
        .files
        .iter()
        .filter(|f| f.path.ends_with("/libc.so.6") || f.path.contains("/libc-2."))
        .collect();
    let lo = libc.iter().map(|f| f.start).min()?;
    let hi = libc.iter().map(|f| f.end).max()?;
    let needle = b"stable release version ";
    let address = dump.find_bytes(lo, hi, needle)?;
    let bytes = dump.bytes(address + needle.len() as u64, 12)?;
    let count = bytes
        .iter()
        .take_while(|&&b| b.is_ascii_digit() || b == b'.')
        .count();
    Some(
        String::from_utf8_lossy(&bytes[..count])
            .trim_end_matches('.')
            .to_owned(),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(C)]
pub struct MallocParameters {
    pub address: u64,
    pub trim_threshold: u64,
    pub mmap_threshold: u64,
    pub arena_max: u64,
    pub dynamic_threshold: bool,
    pub mmapped_chunks: i32,
    pub mmapped_bytes: u64,
}
impl NativeMemory {
    pub fn parameters(&self, dump: &CoreDump) -> Option<MallocParameters> {
        let brk = self.malloc_heaps.iter().find(|h| h.main_arena)?.start;
        let version: Vec<u32> = self
            .glibc_version
            .as_deref()
            .unwrap_or("")
            .split('.')
            .filter_map(|v| v.parse().ok())
            .collect();
        let large = version.len() == 2 && (version[0], version[1]) >= (2, 35);
        let (n_mmaps, no_dyn, mmapped, sbrk, tcache_bins) = if large {
            (60, 72, 80, 96, 104)
        } else {
            (40, 52, 56, 72, 80)
        };
        let libc: Vec<_> = dump
            .files
            .iter()
            .filter(|f| filename(&f.path).starts_with("libc.so"))
            .collect();
        let mut cursor = Cursor::new(dump, true);
        for s in dump
            .segments
            .iter()
            .filter(|s| s.writable && libc.iter().any(|f| f.start < s.end() && s.address < f.end))
        {
            for a in (s.address..s.end().saturating_sub(7)).step_by(8) {
                if cursor.u64(a) != Some(brk) || a - s.address < sbrk {
                    continue;
                }
                let p = a - sbrk;
                if let (
                    Some(trim),
                    Some(mmap),
                    Some(test),
                    Some(max),
                    Some(n),
                    Some(no_dyn),
                    Some(mapped),
                    Some(bins),
                ) = (
                    cursor.u64(p),
                    cursor.u64(p + 16),
                    cursor.u64(p + 24),
                    cursor.u64(p + 32),
                    cursor.u32(p + n_mmaps),
                    cursor.u32(p + no_dyn),
                    cursor.u64(p + mmapped),
                    cursor.u64(p + tcache_bins),
                ) && (1..=128).contains(&bins)
                    && (1..=4096).contains(&test)
                    && no_dyn <= 1
                    && mmap <= 1 << 40
                    && trim <= 1 << 46
                {
                    return Some(MallocParameters {
                        address: p,
                        trim_threshold: trim,
                        mmap_threshold: mmap,
                        arena_max: max,
                        dynamic_threshold: no_dyn == 0,
                        mmapped_chunks: n as i32,
                        mmapped_bytes: mapped,
                    });
                }
            }
        }
        None
    }
}
impl crate::analysis::HeapSnapshot {
    pub fn malloc_parameters(&self) -> crate::Result<std::sync::Arc<Option<MallocParameters>>> {
        let compute = || Ok(self.native.parameters(&self.dump));
        self.saved("malloc parameters", compute)
    }
}
