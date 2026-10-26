use crate::{
    Result,
    analysis::HeapSnapshot,
    bytes::ReadBytes,
    column::ColumnStore,
    core_dump::Cursor,
    graph::{DominatorGraph, dominators, valid_parent_forest},
    native_memory::first_chunk,
    symbols::{LibrarySymbols, demangle},
    type_names::short,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet},
    sync::{Arc, OnceLock},
};

mod cache;
mod managed;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChunkState {
    InUse,
    Free,
    Cached,
    Mmapped,
    Mapping,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Reach {
    NativeRoots,
    LiveManaged,
    PendingManaged,
    DeadManaged,
    Unreferenced,
}
impl Reach {
    pub const ALL: [Self; 5] = [
        Self::NativeRoots,
        Self::LiveManaged,
        Self::PendingManaged,
        Self::DeadManaged,
        Self::Unreferenced,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Self::NativeRoots => "Native roots",
            Self::LiveManaged => "Live objects",
            Self::PendingManaged => "Pending finalization",
            Self::DeadManaged => "Dead objects",
            Self::Unreferenced => "Unreferenced (leak candidates)",
        }
    }
    pub fn holder(self) -> &'static str {
        match self {
            Self::NativeRoots => "native roots",
            Self::LiveManaged => "live managed objects",
            Self::PendingManaged => "managed objects pending finalization",
            Self::DeadManaged => "dead managed objects",
            Self::Unreferenced => "nothing (leak candidate)",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub start: u64,
    pub size: u64,
    pub state: ChunkState,
}
impl Chunk {
    pub fn user_start(&self) -> u64 {
        self.start
            + if self.state == ChunkState::Mapping {
                0
            } else {
                16
            }
    }
    pub fn end(&self) -> u64 {
        self.start + self.size
    }
    pub fn user_end(&self) -> u64 {
        self.end()
            + if [ChunkState::InUse, ChunkState::Cached].contains(&self.state) {
                8
            } else {
                0
            }
    }
    pub fn live(&self) -> bool {
        [ChunkState::InUse, ChunkState::Mmapped, ChunkState::Mapping].contains(&self.state)
    }
    pub fn malloc(&self) -> bool {
        [ChunkState::InUse, ChunkState::Mmapped].contains(&self.state)
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeGraphData {
    pub chunks: Vec<Chunk>,
    pub reach: Vec<Reach>,
    pub edge_start: Vec<u64>,
    pub edge_targets: Vec<i32>,
    pub edge_exact: Vec<bool>,
    pub referrer_start: Vec<u64>,
    pub referrer_items: Vec<i32>,
    pub owned_chunks: BTreeMap<usize, Vec<i32>>,
    pub chunk_owners: BTreeMap<i32, Vec<usize>>,
    pub root_chunks: BTreeSet<i32>,
    pub root_sources: BTreeMap<i32, Vec<String>>,
    pub managed_references: BTreeMap<i32, Vec<usize>>,
    pub handle_references: BTreeMap<i32, Vec<u64>>,
    pub native_retained: BTreeMap<usize, u64>,
    pub chunk_dominator: Vec<i32>,
    pub first_words: Vec<u64>,
    pub texts: BTreeMap<i32, String>,
    pub tcache_chunks: usize,
    pub fastbin_chunks: usize,
}
pub struct NativeGraph {
    pub data: Arc<NativeGraphData>,
    pub symbols: Arc<LibrarySymbols>,
    nearest_class: OnceLock<(Vec<i32>, Vec<bool>)>,
}
impl std::ops::Deref for NativeGraph {
    type Target = NativeGraphData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkGroup {
    pub id: String,
    pub label: String,
    pub kind: ChunkAttributionKind,
    pub reach: Reach,
    pub library: Option<String>,
    pub chunks: Vec<i32>,
    pub bytes: u64,
    pub min_size: u64,
    pub max_size: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChunkAttributionKind {
    CppType,
    CppDominator,
    CppReferrer,
    CppInteriorReferrer,
    ManagedDominator,
    ManagedReferrer,
    NativeRoot,
    Symbol,
    Library,
    Text,
    Unknown,
}
impl ChunkAttributionKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::CppType => "C++ type",
            Self::CppDominator => "C++ dominator",
            Self::CppReferrer => "C++ referrer",
            Self::CppInteriorReferrer => "C++ referrer (interior pointers)",
            Self::ManagedDominator => "Managed dominator",
            Self::ManagedReferrer => "Managed referrer",
            Self::NativeRoot => "Native root",
            Self::Symbol => "Symbol pointer",
            Self::Library => "Library pointer",
            Self::Text => "Text",
            Self::Unknown => "Unknown",
        }
    }

    pub(crate) fn group_kind(self) -> Self {
        match self {
            Self::CppDominator | Self::CppInteriorReferrer => Self::CppReferrer,
            Self::ManagedDominator => Self::ManagedReferrer,
            other => other,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChunkAttribution {
    pub kind: ChunkAttributionKind,
    pub label: String,
    pub library: Option<String>,
    pub source_chunk: Option<usize>,
    pub source_object: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct ChunkSizeGroup {
    pub size: u64,
    pub count: usize,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReachTotal {
    pub reach: Reach,
    pub chunks: usize,
    pub bytes: u64,
}

impl HeapSnapshot {
    pub fn native_graph(&self) -> Result<Arc<NativeGraph>> {
        self.memo("native graph", || {
            let data = Arc::new(NativeGraphData::open(self)?);
            Ok(NativeGraph {
                data,
                symbols: self.symbols()?,
                nearest_class: OnceLock::new(),
            })
        })
    }
    pub fn root_slot_description(&self, i: usize) -> Result<Option<String>> {
        let Some(slot) = self.graph.root_slot(i).filter(|&s| s != 0) else {
            return Ok(None);
        };
        let stacks = self.thread_stacks()?;
        if let Some(t) = stacks
            .threads
            .iter()
            .find(|t| slot >= t.stack_pointer && slot < t.stack_end)
        {
            return Ok(Some(format!(
                "word at 0x{slot:x} in the frame of {} (estimated from return addresses)",
                stacks.describe(stacks.owner(slot, t))
            )));
        }
        let Some(c) = self.native.malloc_chunk(slot, &self.dump) else {
            return Ok(Some(format!("word at 0x{slot:x}")));
        };
        let mut text = format!(
            "word at 0x{slot:x}, offset {} in malloc chunk 0x{:x} of {} bytes",
            slot as i128 - (c.start + 16) as i128,
            c.start + 16,
            c.size
        );
        let g = self.native_graph()?;
        if let Some(j) = g.chunk(slot) {
            match g.chunks[j].state {
                ChunkState::Cached => {
                    text += ", freed (in a tcache or fastbin list): a stale pointer"
                }
                ChunkState::Free => text += ", free: a stale pointer",
                _ => {
                    text += &format!(
                        "; the chunk is held by {}, {}",
                        g.reach[j].holder(),
                        g.group_label(j, self)?
                    )
                }
            }
        } else {
            text += &format!(" ({})", c.state);
        }
        Ok(Some(text))
    }
}
impl NativeGraphData {
    pub fn chunk_sizes(&self, chunks: &[i32]) -> Result<Vec<ChunkSizeGroup>> {
        let mut sizes = BTreeMap::<u64, usize>::new();
        for &index in chunks {
            let chunk = self
                .chunks
                .get(index as usize)
                .ok_or_else(|| crate::Error("Chunk index is out of range".into()))?;
            *sizes.entry(chunk.size).or_default() += 1;
        }
        let mut sizes: Vec<_> = sizes
            .into_iter()
            .map(|(size, count)| ChunkSizeGroup {
                size,
                count,
                bytes: size * count as u64,
            })
            .collect();
        sizes.sort_by_key(|group| (Reverse(group.bytes), Reverse(group.size)));
        Ok(sizes)
    }

    pub fn largest_chunks(
        &self,
        chunks: &[i32],
        size: Option<u64>,
        limit: usize,
    ) -> Result<Vec<i32>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut largest = BinaryHeap::with_capacity(limit.min(chunks.len()));
        for (position, &index) in chunks.iter().enumerate() {
            let chunk = self
                .chunks
                .get(index as usize)
                .ok_or_else(|| crate::Error("Chunk index is out of range".into()))?;
            if size.is_some_and(|size| chunk.size != size) {
                continue;
            }
            let key = (chunk.size, Reverse(position), index);
            if largest.len() < limit {
                largest.push(Reverse(key));
            } else if let Some(mut smallest) = largest.peek_mut()
                && key > smallest.0
            {
                *smallest = Reverse(key);
            }
        }
        Ok(largest
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse((_, _, index))| index)
            .collect())
    }

    pub fn pointer_is_exact(&self, source: usize, target: usize) -> bool {
        (self.edge_start[source] as usize..self.edge_start[source + 1] as usize)
            .any(|edge| self.edge_targets[edge] as usize == target && self.edge_exact[edge])
    }

    fn validate(&self, managed_count: usize) -> Result<()> {
        let n = self.chunks.len();
        let chunk = |i: i32| i >= 0 && (i as usize) < n;
        let rows = |start: &[u64], items: &[i32]| {
            start.len() == n + 1
                && start.first() == Some(&0)
                && start.last() == Some(&(items.len() as u64))
                && start.windows(2).all(|w| w[0] <= w[1])
                && items.iter().all(|&i| i >= 0 && (i as usize) < n)
        };
        if self.reach.len() != n
            || self.chunk_dominator.len() != n
            || self.first_words.len() != n
            || self.edge_exact.len() != self.edge_targets.len()
            || !rows(&self.edge_start, &self.edge_targets)
            || !rows(&self.referrer_start, &self.referrer_items)
            || self.chunks.iter().any(|c| {
                c.size == 0
                    || (c.state != ChunkState::Mapping && c.size < 16)
                    || c.start
                        .checked_add(c.size)
                        .and_then(|end| {
                            end.checked_add(
                                if matches!(c.state, ChunkState::InUse | ChunkState::Cached) {
                                    8
                                } else {
                                    0
                                },
                            )
                        })
                        .is_none()
            })
            || self.chunks.windows(2).any(|w| w[0].end() > w[1].start)
            || !self
                .owned_chunks
                .iter()
                .all(|(&owner, chunks)| owner < managed_count && chunks.iter().all(|&i| chunk(i)))
            || !self
                .chunk_owners
                .iter()
                .chain(&self.managed_references)
                .all(|(&i, owners)| chunk(i) && owners.iter().all(|&o| o < managed_count))
            || !self
                .root_chunks
                .iter()
                .chain(self.root_sources.keys())
                .chain(self.handle_references.keys())
                .chain(self.texts.keys())
                .all(|&i| chunk(i))
            || !self.native_retained.keys().all(|&i| i < managed_count)
            || self.root_sources.iter().any(|(chunk, sources)| {
                !self.root_chunks.contains(chunk)
                    || sources.is_empty()
                    || sources.windows(2).any(|w| w[0] >= w[1])
            })
            || !self.chunk_dominator.iter().all(|&i| {
                i == -1 || chunk(i) || (i <= -2 && (-i64::from(i) - 2) < managed_count as i64)
            })
            || !valid_parent_forest(&self.chunk_dominator, None)
        {
            return Err(crate::Error::cache("invalid native reference graph"));
        }
        Ok(())
    }
    pub fn pointers(&self, i: usize) -> &[i32] {
        &self.edge_targets[self.edge_start[i] as usize..self.edge_start[i + 1] as usize]
    }
    pub fn referrers(&self, i: usize) -> &[i32] {
        &self.referrer_items[self.referrer_start[i] as usize..self.referrer_start[i + 1] as usize]
    }
    pub fn chunk(&self, address: u64) -> Option<usize> {
        if address < self.chunks.first()?.user_start() || address >= self.chunks.last()?.user_end()
        {
            return None;
        }
        let i = self
            .chunks
            .partition_point(|c| c.start <= address)
            .checked_sub(1)?;
        let c = &self.chunks[i];
        if address >= c.user_start() {
            return (address < c.user_end()).then_some(i);
        }
        let prev = i.checked_sub(1)?;
        let c = &self.chunks[prev];
        (address >= c.user_start() && address < c.user_end()).then_some(prev)
    }
    pub fn is_in_use(&self, c: usize) -> bool {
        self.chunks[c].malloc()
    }
    pub fn is_used(&self, c: usize) -> bool {
        self.chunks[c].live()
            && matches!(
                self.reach[c],
                Reach::NativeRoots | Reach::LiveManaged | Reach::PendingManaged
            )
    }
    pub fn owning_object(&self, c: usize) -> Option<usize> {
        let mut d = self.chunk_dominator[c];
        for _ in 0..100_000 {
            if d < 0 {
                break;
            }
            d = *self.chunk_dominator.get(d as usize)?;
        }
        if d <= -2 {
            Some((-i64::from(d) - 2) as usize)
        } else {
            None
        }
    }
    fn new(s: &HeapSnapshot) -> Result<Self> {
        let mut out = Self::default();
        if s.dump.platform.is_linux() {
            out.collect(s);
            if !out.chunks.is_empty() {
                out.mark_cached(s);
                out.scan_chunks(s);
                out.scan_roots(s);
                out.scan_managed(s)?;
                out.compute_reach(s);
                out.compute_dominators()?;
            }
        }
        if out.chunks.is_empty() {
            out.edge_start.push(0);
            out.referrer_start.push(0);
        }
        Ok(out)
    }
    fn collect(&mut self, s: &HeapSnapshot) {
        let dump = &s.dump;
        let mut cursor = Cursor::new(dump, true);
        for h in &s.native.malloc_heaps {
            let Some((mut c, top)) = first_chunk(dump, h.start, h.end, h.main_arena) else {
                continue;
            };
            let mut pending = None;
            while h.end.saturating_sub(c) >= 16 {
                let Some(raw) = cursor.u64(c + 8) else {
                    break;
                };
                let size = raw & !7;
                if size == 16 && !h.main_arena {
                    if let Some((start, size)) = pending {
                        self.chunks.push(Chunk {
                            start,
                            size,
                            state: if raw & 1 != 0 {
                                ChunkState::InUse
                            } else {
                                ChunkState::Free
                            },
                        });
                    }
                    break;
                }
                if size < 32 || size > h.end - c || size & 15 != 0 {
                    break;
                }
                if let Some((start, size)) = pending.take() {
                    self.chunks.push(Chunk {
                        start,
                        size,
                        state: if raw & 1 != 0 {
                            ChunkState::InUse
                        } else {
                            ChunkState::Free
                        },
                    });
                }
                if c == top || c + size >= h.end {
                    break;
                }
                pending = Some((c, size));
                c += size;
            }
        }
        for m in s
            .native
            .mappings
            .iter()
            .filter(|m| m.category == "malloc large allocation")
        {
            self.chunks.push(Chunk {
                start: m.start,
                size: m.size(),
                state: ChunkState::Mmapped,
            });
        }
        let stacks: HashSet<_> = dump
            .threads
            .iter()
            .filter_map(|t| dump.segment_index(t.stack_pointer))
            .collect();
        let large: HashSet<_> = self
            .chunks
            .iter()
            .filter(|c| c.state == ChunkState::Mmapped)
            .map(|c| c.start)
            .collect();
        let mut handle_segments = s.graph.handle_segment_addresses.clone();
        handle_segments.sort_unstable();
        for (i, seg) in dump.segments.iter().enumerate() {
            if !seg.writable
                || stacks.contains(&i)
                || dump.exited_thread_stacks.contains(&i)
                || s.heap.owns_memory(seg.address, seg.end())
                || large.contains(&seg.address)
            {
                continue;
            }
            if handle_segments
                .get(handle_segments.partition_point(|&a| a < seg.address))
                .is_some_and(|&a| a < seg.end())
                || s.native
                    .malloc_heaps
                    .iter()
                    .any(|h| seg.address < h.end && seg.end() > h.start)
            {
                continue;
            }
            if dump.file_mapping(seg.address).is_some()
                || dump.files.iter().any(|f| f.end == seg.address)
            {
                continue;
            }
            self.chunks.push(Chunk {
                start: seg.address,
                size: seg.size,
                state: ChunkState::Mapping,
            });
        }
        self.chunks.sort_by_key(|c| c.start);
        self.first_words = self
            .chunks
            .iter()
            .map(|c| cursor.u64(c.user_start()).unwrap_or(0))
            .collect();
    }
    fn mark_cached(&mut self, s: &HeapSnapshot) {
        let dump = &s.dump;
        let second: Vec<_> = self
            .chunks
            .iter()
            .map(|c| dump.u64(c.user_start() + 8).unwrap_or(0))
            .collect();
        let mut entries = Vec::new();
        let mut structs = HashSet::new();
        for c in 0..self.chunks.len() {
            let chunk = &self.chunks[c];
            if chunk.state != ChunkState::InUse || ![0x290, 0x250].contains(&chunk.size) {
                continue;
            }
            let base = chunk.user_start();
            let width = if chunk.size == 0x290 { 2 } else { 1 };
            let Some(counts) = dump.bytes(base, 64 * width) else {
                continue;
            };
            let count = |k| {
                if width == 2 {
                    counts.u16(k * 2).unwrap() as usize
                } else {
                    counts[k] as usize
                }
            };
            if (0..64).map(count).sum::<usize>() == 0 || (0..64).any(|k| count(k) > 255) {
                continue;
            }
            let mut marked = Vec::new();
            for k in 0..64 {
                let n = count(k);
                if n == 0 {
                    continue;
                }
                let Some(mut e) = dump
                    .u64(base + (64 * width + k * 8) as u64)
                    .filter(|&e| e != 0)
                else {
                    continue;
                };
                let size = k as u64 * 16 + 32;
                let valid = |x| {
                    self.chunk(x).is_some_and(|j| {
                        self.chunks[j].user_start() == x && self.chunks[j].size == size
                    })
                };
                let mut seen = HashSet::new();
                for _ in 0..n {
                    if !valid(e) || !seen.insert(e) {
                        break;
                    }
                    let j = self.chunk(e).unwrap();
                    marked.push(j);
                    e = link(e, self.first_words[j], valid);
                    if e == 0 {
                        break;
                    }
                }
            }
            if !marked.is_empty() {
                structs.insert(base);
            }
            for j in marked {
                if self.chunks[j].state == ChunkState::InUse {
                    self.chunks[j].state = ChunkState::Cached;
                    self.tcache_chunks += 1;
                    entries.push(j);
                }
            }
        }
        let mut keys = BTreeMap::<u64, usize>::new();
        for j in entries {
            if second[j] > 0xffff_ffff {
                *keys.entry(second[j]).or_default() += 1;
            }
        }
        if let Some((&key, &n)) = keys.iter().max_by_key(|(k, v)| (**v, **k))
            && n >= 8
        {
            structs.insert(key);
        }
        for (j, c) in self.chunks.iter_mut().enumerate() {
            if c.state == ChunkState::InUse && c.size <= 1040 && structs.contains(&second[j]) {
                c.state = ChunkState::Cached;
                self.tcache_chunks += 1;
            }
        }
        let mut arenas: BTreeSet<_> = s
            .native
            .malloc_heaps
            .iter()
            .filter(|h| !h.main_arena)
            .filter_map(|h| dump.u64(h.start))
            .collect();
        for mut p in arenas.clone() {
            for _ in 0..1024 {
                let Some(n) = p
                    .checked_add(2160)
                    .and_then(|a| dump.u64(a))
                    .filter(|&n| n != 0)
                else {
                    break;
                };
                if !arenas.insert(n) {
                    break;
                }
                p = n;
            }
        }
        for a in arenas {
            for k in 0..10 {
                let Some(mut p) = a
                    .checked_add(16 + k * 8)
                    .and_then(|a| dump.u64(a))
                    .filter(|&p| p != 0)
                else {
                    continue;
                };
                let mut seen = HashSet::new();
                for _ in 0..100_000 {
                    let valid = |x: u64| {
                        x.checked_add(16)
                            .and_then(|x| self.chunk(x))
                            .is_some_and(|j| self.chunks[j].start == x)
                    };
                    if !valid(p) || !seen.insert(p) {
                        break;
                    }
                    let j = self.chunk(p + 16).unwrap();
                    let next = link(p + 16, self.first_words[j], valid);
                    if self.chunks[j].state == ChunkState::InUse {
                        self.chunks[j].state = ChunkState::Cached;
                        self.fastbin_chunks += 1;
                    }
                    p = next;
                    if p == 0 {
                        break;
                    }
                }
            }
        }
    }
    fn scan_chunks(&mut self, s: &HeapSnapshot) {
        let slots: HashSet<_> = s.graph.handles.iter().map(|h| h.slot).collect();
        let mut cursor = Cursor::new(&s.dump, true);
        self.edge_start = vec![0; self.chunks.len() + 1];
        for i in 0..self.chunks.len() {
            self.edge_start[i] = self.edge_targets.len() as u64;
            let c = &self.chunks[i];
            let start = c.user_start();
            let size = c.user_end() - start;
            if !c.live() || !cursor.holds(start, size as usize) {
                continue;
            }
            if c.malloc()
                && let Some(text) = cursor
                    .c_string(start, 48)
                    .filter(|s| s.chars().count() >= 6)
            {
                self.texts.insert(i as i32, text);
            }
            let mut seen = HashMap::new();
            for k in 0..size / 8 {
                let Some(v) = cursor
                    .u64(start + k * 8)
                    .filter(|&v| v > 0x10000 && v & 7 == 0)
                else {
                    continue;
                };
                if let Some(j) = self.chunk(v) {
                    if j == i || !self.chunks[j].live() {
                        continue;
                    }
                    let exact = v == self.chunks[j].user_start();
                    if let Some(&e) = seen.get(&j) {
                        if exact {
                            self.edge_exact[e] = true;
                        }
                    } else {
                        seen.insert(j, self.edge_targets.len());
                        self.edge_targets.push(j as i32);
                        self.edge_exact.push(exact);
                    }
                } else if let Some(o) = s.heap.object_at(v) {
                    self.managed_references.entry(i as i32).or_default().push(o);
                } else if slots.contains(&v) {
                    self.handle_references.entry(i as i32).or_default().push(v);
                }
            }
        }
        self.edge_start[self.chunks.len()] = self.edge_targets.len() as u64;
        self.referrer_start = vec![0; self.chunks.len() + 1];
        for &t in &self.edge_targets {
            self.referrer_start[t as usize + 1] += 1;
        }
        for i in 0..self.chunks.len() {
            self.referrer_start[i + 1] += self.referrer_start[i];
        }
        let mut fill = self.referrer_start.clone();
        self.referrer_items = vec![0; self.edge_targets.len()];
        for i in 0..self.chunks.len() {
            for e in self.edge_start[i] as usize..self.edge_start[i + 1] as usize {
                let t = self.edge_targets[e] as usize;
                self.referrer_items[fill[t] as usize] = i as i32;
                fill[t] += 1;
            }
        }
    }
    fn add_root(&mut self, c: usize, source: &str) {
        self.root_chunks.insert(c as i32);
        let sources = self.root_sources.entry(c as i32).or_default();
        if let Err(at) = sources.binary_search_by(|value| value.as_str().cmp(source)) {
            sources.insert(at, source.into());
        }
    }
    fn scan_roots(&mut self, s: &HeapSnapshot) {
        let dump = &s.dump;
        let mut stacks = HashMap::new();
        for t in &dump.threads {
            if let Some(i) = dump.segment_index(t.stack_pointer) {
                stacks.insert(i, (t.stack_pointer, t.tid));
            }
            for &r in &t.registers {
                if let Some(j) = self.chunk(r)
                    && self.chunks[j].live()
                {
                    self.add_root(j, &format!("registers of thread {}", t.tid));
                }
            }
        }
        let large: HashSet<_> = self
            .chunks
            .iter()
            .filter(|c| matches!(c.state, ChunkState::Mmapped | ChunkState::Mapping))
            .map(|c| c.start)
            .collect();
        for (i, seg) in dump.segments.iter().enumerate() {
            if !seg.writable
                || dump.exited_thread_stacks.contains(&i)
                || s.heap.owns_memory(seg.address, seg.end())
                || large.contains(&seg.address)
                || s.native
                    .malloc_heaps
                    .iter()
                    .any(|h| seg.address < h.end && seg.end() > h.start)
            {
                continue;
            }
            let start = stacks
                .get(&i)
                .map(|&(sp, _)| seg.address.max(sp.saturating_sub(128) & !7))
                .unwrap_or(seg.address);
            let source = if let Some(&(_, tid)) = stacks.get(&i) {
                format!("stack of thread {tid}")
            } else if let Some(f) = dump.file_mapping(seg.address) {
                format!("{} data", f.path.rsplit('/').next().unwrap_or(&f.path))
            } else if let Some(f) = dump.files.iter().rev().find(|f| f.end == seg.address) {
                format!("{} .bss", f.path.rsplit('/').next().unwrap_or(&f.path))
            } else {
                format!("anonymous memory at 0x{:x}", seg.address)
            };
            dump.scan(start, start + (seg.end() - start) / 8 * 8, |a, bytes| {
                let first = (start.wrapping_sub(a) & 7) as usize;
                for k in (first..bytes.len()).step_by(8) {
                    let slot = a + k as u64;
                    if slot + 8 > seg.end() {
                        break;
                    }
                    if let Some(v) = bytes.u64(k).or_else(|| dump.u64(slot))
                        && let Some(j) = self.chunk(v)
                        && self.chunks[j].live()
                    {
                        self.add_root(j, &source);
                    }
                }
            });
        }
    }
    fn scan_managed(&mut self, s: &HeapSnapshot) -> Result<()> {
        let layouts = managed::layouts(s)?;
        let mut cursor = Cursor::new(&s.dump, true);
        for i in 0..s.heap.count() {
            let layout = &layouts[s.heap.type_ids[i] as usize];
            if layout.is_empty() {
                continue;
            }
            let start = s.heap.addresses[i];
            let size = s.heap.size(i);
            if !cursor.holds(start, size as usize) {
                continue;
            }
            let mut owned = Vec::new();
            for (slot, _, _) in layout.slots(start, size, s.heap.lengths[i]) {
                if let Some(j) = cursor.u64(slot).and_then(|v| self.chunk(v))
                    && self.chunks[j].live()
                    && !owned.contains(&(j as i32))
                {
                    owned.push(j as i32);
                }
            }
            if !owned.is_empty() {
                for &j in &owned {
                    self.chunk_owners.entry(j).or_default().push(i);
                }
                self.owned_chunks.insert(i, owned);
            }
        }
        Ok(())
    }
    fn compute_reach(&mut self, s: &HeapSnapshot) {
        self.reach = vec![Reach::Unreferenced; self.chunks.len()];
        fn spread(this: &mut NativeGraphData, seeds: Vec<i32>, kind: Reach) {
            let mut queue = Vec::new();
            for v in seeds {
                if this.reach[v as usize] == Reach::Unreferenced {
                    this.reach[v as usize] = kind;
                    queue.push(v);
                }
            }
            let mut head = 0;
            while head < queue.len() {
                let c = queue[head] as usize;
                head += 1;
                for e in this.edge_start[c] as usize..this.edge_start[c + 1] as usize {
                    let t = this.edge_targets[e];
                    if this.reach[t as usize] == Reach::Unreferenced {
                        this.reach[t as usize] = kind;
                        queue.push(t);
                    }
                }
            }
        }
        spread(
            self,
            self.root_chunks.iter().copied().collect(),
            Reach::NativeRoots,
        );
        for kind in [
            Reach::LiveManaged,
            Reach::PendingManaged,
            Reach::DeadManaged,
        ] {
            let seeds = self
                .owned_chunks
                .iter()
                .filter(|&(i, _)| match kind {
                    Reach::LiveManaged => s.graph.live[*i] != 0 && s.graph.pending[*i] == 0,
                    Reach::PendingManaged => s.graph.pending[*i] != 0,
                    _ => s.graph.live[*i] == 0,
                })
                .flat_map(|(_, v)| v.iter().copied())
                .collect();
            spread(self, seeds, kind);
        }
    }
    fn compute_dominators(&mut self) -> Result<()> {
        let owners: Vec<_> = self
            .owned_chunks
            .iter()
            .filter(|(_, v)| {
                v.iter().any(|&c| {
                    matches!(
                        self.reach[c as usize],
                        Reach::LiveManaged | Reach::PendingManaged | Reach::NativeRoots
                    )
                })
            })
            .map(|(&o, _)| o)
            .collect();
        let base = owners.len() + 1;
        let mut predecessors = BTreeMap::<i32, Vec<usize>>::new();
        for (k, o) in owners.iter().enumerate() {
            for &c in &self.owned_chunks[o] {
                predecessors.entry(c).or_default().push(k + 1);
            }
        }
        struct Graph<'a> {
            data: &'a NativeGraphData,
            owners: &'a [usize],
            roots: Vec<i32>,
            predecessors: BTreeMap<i32, Vec<usize>>,
        }
        impl DominatorGraph for Graph<'_> {
            fn total(&self) -> usize {
                1 + self.owners.len() + self.data.chunks.len()
            }
            fn root(&self) -> usize {
                0
            }
            fn first_position(&self, _: usize) -> u64 {
                0
            }
            fn next(&self, v: usize, p: &mut u64) -> Option<usize> {
                let base = 1 + self.owners.len();
                let k = *p as usize;
                let next = if v == 0 {
                    if k < self.owners.len() {
                        Some(k + 1)
                    } else {
                        self.roots
                            .get(k - self.owners.len())
                            .map(|&c| base + c as usize)
                    }
                } else if v < base {
                    self.data.owned_chunks[&self.owners[v - 1]]
                        .get(k)
                        .map(|&c| base + c as usize)
                } else {
                    self.data
                        .pointers(v - base)
                        .get(k)
                        .map(|&c| base + c as usize)
                };
                *p += 1;
                next
            }
            fn predecessors(&self, v: usize, emit: &mut dyn FnMut(usize)) {
                let base = 1 + self.owners.len();
                if v < base {
                    if v > 0 {
                        emit(0);
                    }
                    return;
                }
                let c = (v - base) as i32;
                if self.data.root_chunks.contains(&c) {
                    emit(0);
                }
                for &o in self.predecessors.get(&c).into_iter().flatten() {
                    emit(o);
                }
                for &r in self.data.referrers(c as usize) {
                    emit(base + r as usize);
                }
            }
        }
        let graph = Graph {
            data: self,
            owners: &owners,
            roots: self.root_chunks.iter().copied().collect(),
            predecessors,
        };
        let (idom, _, _) = dominators(&graph, &ColumnStore::new(None)?, |_| 0)?;
        self.chunk_dominator = (0..self.chunks.len())
            .map(|c| {
                let d = idom[base + c];
                if d < 1 {
                    -1
                } else if d as usize >= base {
                    d - base as i32
                } else {
                    -((owners[d as usize - 1] as i64) + 2) as i32
                }
            })
            .collect();
        for c in 0..self.chunks.len() {
            if self.chunks[c].malloc()
                && let Some(o) = self.owning_object(c)
            {
                *self.native_retained.entry(o).or_default() += self.chunks[c].size;
            }
        }
        Ok(())
    }
}
fn link(slot: u64, stored: u64, valid: impl Fn(u64) -> bool) -> u64 {
    let revealed = (slot >> 12) ^ stored;
    if valid(revealed) || !valid(stored) {
        revealed
    } else {
        stored
    }
}
impl NativeGraph {
    pub fn class_name(&self, c: usize) -> Option<String> {
        self.symbols.class_name(self.first_words[c])
    }
    fn nearest_classes(&self) -> &(Vec<i32>, Vec<bool>) {
        self.nearest_class.get_or_init(|| {
            let mut nearest = vec![-1; self.chunks.len()];
            let mut queue = Vec::new();
            for (i, nearest_class) in nearest.iter_mut().enumerate() {
                if self.chunks[i].live() && self.class_name(i).is_some() {
                    *nearest_class = i as i32;
                    queue.push(i);
                }
            }
            let mut by_start = Vec::new();
            for starts_only in [true, false] {
                let mut head = 0;
                while head < queue.len() {
                    let c = queue[head];
                    head += 1;
                    if self.chunks[c].state == ChunkState::Mapping && self.referrers(c).len() > 1 {
                        continue;
                    }
                    for e in self.edge_start[c] as usize..self.edge_start[c + 1] as usize {
                        if starts_only && !self.edge_exact[e] {
                            continue;
                        }
                        let t = self.edge_targets[e] as usize;
                        if nearest[t] < 0
                            && !self.root_chunks.contains(&(t as i32))
                            && !self.chunk_owners.contains_key(&(t as i32))
                        {
                            nearest[t] = nearest[c];
                            queue.push(t);
                        }
                    }
                }
                if starts_only {
                    by_start = nearest.iter().map(|&n| n >= 0).collect();
                    queue = (0..nearest.len()).filter(|&i| nearest[i] >= 0).collect();
                }
            }
            (nearest, by_start)
        })
    }
    pub fn nearest_class(&self, c: usize) -> Option<(String, bool)> {
        let (n, exact) = self.nearest_classes();
        let i = n[c];
        if i < 0 || i as usize == c {
            return None;
        }
        Some((self.class_name(i as usize)?, exact[c]))
    }
    pub fn nearest_owner(&self, c: usize) -> Option<usize> {
        let mut seen = HashSet::from([c as i32]);
        let mut queue = vec![c as i32];
        let mut head = 0;
        while head < queue.len() && seen.len() < 4096 {
            let x = queue[head];
            head += 1;
            if let Some(&o) = self.chunk_owners.get(&x).and_then(|v| v.first()) {
                return Some(o);
            }
            for &r in self.referrers(x as usize) {
                if seen.insert(r) {
                    queue.push(r);
                }
            }
        }
        None
    }
    pub fn library(&self, c: usize) -> Option<String> {
        let lib = |c: usize| {
            self.symbols
                .symbol(self.first_words[c])
                .filter(|s| s.name.starts_with("_ZTV"))
                .map(|s| s.library.clone())
        };
        if let Some(l) = lib(c) {
            return Some(l);
        }
        let mut d = self.chunk_dominator[c];
        for _ in 0..10_000 {
            if d < 0 {
                break;
            }
            if let Some(l) = lib(d as usize) {
                return Some(l);
            }
            d = self.chunk_dominator[d as usize];
        }
        let n = self.nearest_classes().0[c];
        if n >= 0 { lib(n as usize) } else { None }
    }
    pub fn owner_field(&self, o: usize, c: usize, s: &HeapSnapshot) -> Option<String> {
        let layouts = managed::layouts(s).ok()?;
        let layout = &layouts[s.heap.type_ids[o] as usize];
        let mut cursor = Cursor::new(&s.dump, true);
        layout
            .slots(s.heap.addresses[o], s.heap.size(o), s.heap.lengths[o])
            .find_map(|(slot, path, index)| {
                (cursor.u64(slot).and_then(|v| self.chunk(v)) == Some(c)).then(|| match index {
                    Some(index) if path.is_empty() => format!("[{index}]"),
                    Some(index) => format!("[{index}].{path}"),
                    None => path.to_owned(),
                })
            })
    }
    pub fn label(&self, c: usize, s: &HeapSnapshot) -> String {
        let chunk = &self.chunks[c];
        if chunk.state == ChunkState::Mapping {
            return format!("anonymous mapping 0x{:x}", chunk.start);
        }
        if let Some(cls) = self.class_name(c) {
            return cls;
        }
        if let Some(&o) = self.chunk_owners.get(&(c as i32)).and_then(|v| v.first()) {
            return format!(
                "referenced by {}{}",
                short(&s.type_name(o)),
                (s.heap.method_table(o).component_size() == 0)
                    .then(|| self.owner_field(o, c, s))
                    .flatten()
                    .map(|f| format!(".{f}"))
                    .unwrap_or_default()
            );
        }
        let first = self.first_words[c];
        if let Some(sym) = self.symbols.symbol(first) {
            return format!(
                "points to {}",
                demangle(&sym.name).unwrap_or_else(|| sym.name.clone())
            );
        }
        if let Some(lib) = self.symbols.library(first) {
            return format!("points into {lib}");
        }
        let text = if chunk.malloc() {
            self.texts.get(&(c as i32)).cloned()
        } else {
            s.dump.c_string(chunk.user_start(), 48)
        };
        if let Some(text) = text.filter(|t| t.chars().count() >= 6) {
            return format!("text \"{text}\"");
        }
        format!("{}-byte chunk", chunk.size)
    }
    pub fn attribution(&self, c: usize, s: &HeapSnapshot) -> Result<ChunkAttribution> {
        use ChunkAttributionKind::*;
        let cpp = |kind, source, label| ChunkAttribution {
            kind,
            label,
            library: self
                .symbols
                .symbol(self.first_words[source])
                .map(|s| s.library.clone()),
            source_chunk: Some(source),
            source_object: None,
        };
        let managed = |kind, object, chunk| {
            let field = (s.heap.method_table(object).component_size() == 0)
                .then(|| self.owner_field(object, chunk, s))
                .flatten();
            ChunkAttribution {
                kind,
                label: format!(
                    "{}{}",
                    s.type_name(object),
                    field.map(|f| format!(".{f}")).unwrap_or_default()
                ),
                library: None,
                source_chunk: None,
                source_object: Some(object),
            }
        };
        let named = |kind, label| ChunkAttribution {
            kind,
            label,
            library: None,
            source_chunk: None,
            source_object: None,
        };
        if let Some(cls) = self.class_name(c) {
            return Ok(cpp(CppType, c, cls));
        }
        let mut d = self.chunk_dominator[c];
        let mut top = c;
        for _ in 0..10_000 {
            if d < 0 {
                break;
            }
            if let Some(cls) = self.class_name(d as usize) {
                return Ok(cpp(CppDominator, d as usize, cls));
            }
            top = d as usize;
            d = self.chunk_dominator[top];
        }
        if d <= -2 {
            return Ok(managed(ManagedDominator, (-i64::from(d) - 2) as usize, top));
        }
        if self.reach[c] == Reach::DeadManaged
            && let Some(o) = self.nearest_owner(c)
        {
            return Ok(managed(ManagedReferrer, o, c));
        }
        let (nearest, exact) = self.nearest_classes();
        if nearest[c] >= 0
            && let Some(name) = self.class_name(nearest[c] as usize)
        {
            return Ok(cpp(
                if exact[c] {
                    CppReferrer
                } else {
                    CppInteriorReferrer
                },
                nearest[c] as usize,
                name,
            ));
        }
        if let Some(&o) = self.chunk_owners.get(&(c as i32)).and_then(|v| v.first()) {
            return Ok(managed(ManagedReferrer, o, c));
        }
        if self.chunks[top].state == ChunkState::Mapping {
            let mut source = named(
                NativeRoot,
                format!("Anonymous memory at 0x{:x}", self.chunks[top].start),
            );
            source.source_chunk = Some(top);
            return Ok(source);
        }
        if let Some(sources) = self.root_sources.get(&(top as i32)) {
            return Ok(named(
                NativeRoot,
                if sources.len() == 1 {
                    sources[0].clone()
                } else {
                    format!("Shared by {} native root sources", sources.len())
                },
            ));
        }
        if let Some(symbol) = self.symbols.symbol(self.first_words[c]) {
            let mut source = named(
                Symbol,
                demangle(&symbol.name).unwrap_or_else(|| symbol.name.clone()),
            );
            source.library = Some(symbol.library.clone());
            return Ok(source);
        }
        if let Some(library) = self.symbols.library(self.first_words[c]) {
            let mut source = named(Library, library.to_owned());
            source.library = Some(library.to_owned());
            return Ok(source);
        }
        if self
            .texts
            .get(&(c as i32))
            .is_some_and(|t| t.chars().count() >= 6)
        {
            return Ok(named(Text, "Text".into()));
        }
        Ok(named(Unknown, "Unidentified chunks".into()))
    }

    pub fn group_label(&self, c: usize, s: &HeapSnapshot) -> Result<String> {
        use ChunkAttributionKind::*;
        let source = self.attribution(c, s)?;
        let own = if source.kind == ManagedReferrer && self.chunk_owners.contains_key(&(c as i32)) {
            format!("{}-byte chunk", self.chunks[c].size)
        } else {
            self.label(c, s)
        };
        Ok(match source.kind {
            CppType => source.label,
            CppDominator | ManagedDominator => format!("{own}; dominated by {}", source.label),
            CppReferrer => format!("{own}; pointers from {}", source.label),
            CppInteriorReferrer => format!(
                "{own}; pointers from {} (includes interior pointers)",
                source.label
            ),
            ManagedReferrer => format!("{own}; referenced by {}", source.label),
            NativeRoot => format!("{own}; reachable from {}", source.label),
            _ => own,
        })
    }
    pub fn groups(&self, s: &HeapSnapshot) -> Result<Vec<ChunkGroup>> {
        let mut groups = BTreeMap::new();
        for i in 0..self.chunks.len() {
            if !self.chunks[i].malloc() {
                continue;
            }
            let attribution = self.attribution(i, s)?;
            let kind = attribution.kind.group_kind();
            let reach = self.reach[i];
            let size = self.chunks[i].size;
            let group = groups
                .entry((
                    reach,
                    kind,
                    attribution.label.clone(),
                    attribution.library.clone(),
                ))
                .or_insert_with(|| ChunkGroup {
                    id: i.to_string(),
                    label: attribution.label,
                    kind,
                    reach,
                    library: attribution.library,
                    chunks: vec![],
                    bytes: 0,
                    min_size: size,
                    max_size: size,
                });
            group.chunks.push(i as i32);
            group.bytes += size;
            group.min_size = group.min_size.min(size);
            group.max_size = group.max_size.max(size);
        }
        let mut list: Vec<_> = groups.into_values().collect();
        list.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then(a.label.cmp(&b.label))
                .then(a.reach.cmp(&b.reach))
                .then(a.kind.cmp(&b.kind))
                .then(a.library.cmp(&b.library))
        });
        Ok(list)
    }
    pub fn totals(&self) -> Vec<ReachTotal> {
        let mut result: Vec<_> = Reach::ALL
            .into_iter()
            .map(|reach| ReachTotal {
                reach,
                chunks: 0,
                bytes: 0,
            })
            .collect();
        for (i, c) in self.chunks.iter().enumerate() {
            if c.malloc() {
                let group = &mut result[self.reach[i] as usize];
                group.chunks += 1;
                group.bytes += c.size;
            }
        }
        result
    }
    pub fn reachable(&self, objects: &[usize]) -> (usize, u64) {
        let mut seen = HashSet::new();
        let mut queue = Vec::new();
        for o in objects {
            for &c in self.owned_chunks.get(o).into_iter().flatten() {
                if seen.insert(c) {
                    queue.push(c);
                }
            }
        }
        let (mut head, mut bytes) = (0, 0);
        while head < queue.len() {
            let c = queue[head] as usize;
            head += 1;
            bytes += self.chunks[c].size;
            for &t in self.pointers(c) {
                if seen.insert(t) {
                    queue.push(t);
                }
            }
        }
        (queue.len(), bytes)
    }
}
