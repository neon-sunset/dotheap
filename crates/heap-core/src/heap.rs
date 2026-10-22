use crate::{
    Error, Result,
    bytes::align_up,
    column::{Column, ColumnStore},
    core_dump::{CoreDump, Cursor},
    runtime::RuntimeInfo,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

pub const GRANULE: u64 = 1 << 21;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[repr(C)]
pub struct Page {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Gc {
    Satori,
    #[serde(rename = "CoreCLR")]
    CoreClr,
    #[serde(rename = "none")]
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_adaptation: Option<bool>,
    pub background: bool,
}
impl Default for GcState {
    fn default() -> Self {
        Self {
            dynamic_adaptation: None,
            background: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeapKind {
    pub gc: Gc,
    pub layout: String,
    pub detection: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stock: Option<GcState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub index: usize,
    pub start: u64,
    pub end: u64,
    pub object_start: u64,
    pub generation: i32,
    pub generation_name: String,
    pub queue: String,
    pub gc_storage: Option<String>,
    pub gc_heap: i32,
    pub used: u64,
    pub committed: u64,
    pub reusable_for: u8,
    pub alloc_start: u64,
    pub alloc_end: u64,
    pub occupancy: u64,
    pub obj_count: i32,
    pub has_pinned_objects: bool,
    pub immortal: bool,
    pub owner_thread_tag: u64,
    pub attachment_point: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_thread: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocating_thread: Option<i32>,
    pub first_object: usize,
    pub end_object: usize,
    pub object_bytes: u64,
    pub free_bytes: u64,
    pub unparsed_bytes: u64,
    pub live_bytes: u64,
    pub pending_bytes: u64,
    pub walk_stopped: bool,
    pub walk_stopped_at: Option<u64>,
    pub walk_stopped_after_gap: bool,
    pub first_unparsed_object: Option<u64>,
}
impl Region {
    pub fn size(&self) -> u64 {
        self.end - self.start
    }
    pub fn header_bytes(&self) -> u64 {
        self.object_start - self.start
    }
    pub fn used_bytes(&self) -> u64 {
        self.used.min(self.end).saturating_sub(self.start)
    }
    pub fn committed_bytes(&self) -> u64 {
        self.committed.min(self.end).saturating_sub(self.start)
    }
    pub fn gc_storage_bytes(&self) -> u64 {
        if self.gc_storage.is_some() {
            self.committed_bytes().saturating_sub(self.header_bytes())
        } else {
            0
        }
    }
    pub fn committed_unused_bytes(&self) -> u64 {
        if self.gc_storage.is_some() {
            0
        } else {
            self.committed_bytes().saturating_sub(self.used_bytes())
        }
    }
    pub fn reclaimable_bytes(&self) -> u64 {
        self.dead_bytes() + self.free_bytes + self.committed_unused_bytes()
    }
    pub fn dead_bytes(&self) -> u64 {
        self.object_bytes - self.live_bytes - self.pending_bytes
    }
    pub fn has_generation(&self) -> bool {
        (0..=3).contains(&self.generation)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(C)]
pub struct MethodTableInfo {
    pub address: u64,
    pub flags: u32,
    pub base_size: u32,
}
impl MethodTableInfo {
    pub const HAS_COMPONENT_SIZE: u32 = 0x8000_0000;
    pub const CONTAINS_GC_POINTERS: u32 = 0x0100_0000;
    pub fn component_size(self) -> u32 {
        if self.flags & Self::HAS_COMPONENT_SIZE != 0 {
            self.flags & 0xffff
        } else {
            0
        }
    }
    pub fn contains_pointers(self) -> bool {
        self.flags & Self::CONTAINS_GC_POINTERS != 0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalizationQueue {
    pub start: u64,
    pub end: u64,
    pub ready_start: u64,
    pub ready_end: u64,
    pub stride: u64,
    pub registered_start: u64,
    pub registered_end: u64,
}
impl FinalizationQueue {
    pub fn waiting(&self) -> usize {
        ((self.ready_end - self.ready_start) / self.stride) as usize
    }
    pub fn ready_slots(&self) -> impl Iterator<Item = u64> + '_ {
        (self.ready_start..self.ready_end).step_by(self.stride as usize)
    }
    pub fn registered_slots(&self) -> impl Iterator<Item = u64> + '_ {
        (self.registered_start..self.registered_end).step_by(8)
    }
}

pub(crate) struct HeapDiscovery {
    pub pages: Vec<Page>,
    pub regions: Vec<Region>,
    pub kind: HeapKind,
    pub unparseable: Vec<(u64, u64)>,
    pub finalization: Vec<FinalizationQueue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapSaved {
    pub kind: HeapKind,
    pub pages: Vec<Page>,
    pub regions: Vec<Region>,
    pub types: Vec<MethodTableInfo>,
    pub free_method_table: u64,
    pub large_sizes: BTreeMap<usize, u64>,
    pub low: u64,
    pub high: u64,
    pub index_low: u64,
    pub index_high: u64,
    pub finalization: Vec<FinalizationQueue>,
}

pub struct ManagedHeap {
    pub dump: Arc<CoreDump>,
    pub store: ColumnStore,
    pub saved: HeapSaved,
    pub addresses: Column<u64>,
    pub type_ids: Column<u32>,
    pub sizes32: Column<u32>,
    pub lengths: Column<u32>,
    granule_first: Column<i32>,
}
impl std::ops::Deref for ManagedHeap {
    type Target = HeapSaved;
    fn deref(&self) -> &HeapSaved {
        &self.saved
    }
}

#[derive(Default)]
struct RegionWalk {
    objects: usize,
    object_bytes: u64,
    free_bytes: u64,
    unparsed: u64,
    stopped: bool,
    stopped_at: Option<u64>,
    stopped_after_gap: bool,
    first_unparsed_object: Option<u64>,
}

impl ManagedHeap {
    pub fn new(
        dump: Arc<CoreDump>,
        runtime: Option<&RuntimeInfo>,
        store: ColumnStore,
    ) -> Result<Self> {
        let found = if let Some(found) = crate::satori_heap::find(&dump, runtime)? {
            found
        } else if let Some(found) =
            runtime.and_then(|runtime| crate::coreclr_heap::find(&dump, runtime))
        {
            found
        } else {
            HeapDiscovery {
                pages: vec![],
                regions: vec![],
                kind: HeapKind {
                    gc: Gc::None,
                    layout: "no managed heap".into(),
                    detection: if runtime.is_some() {
                        "neither Satori pages nor the CoreCLR GC's DAC variables were found"
                    } else {
                        "libcoreclr is not in the dump"
                    }
                    .into(),
                    stock: None,
                },
                unparseable: vec![],
                finalization: vec![],
            }
        };
        let mut regions = found.regions;
        regions.sort_by_key(|r| r.start);
        for (i, r) in regions.iter_mut().enumerate() {
            r.index = i;
        }
        let mut gaps = found.unparseable;
        gaps.sort_unstable();
        // Overlapping allocation contexts must not make the walker move backwards.
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in gaps {
            if end > start {
                if let Some(last) = merged.last_mut()
                    && last.1 >= start
                {
                    last.1 = last.1.max(end);
                } else {
                    merged.push((start, end));
                }
            }
        }
        let low = regions.first().map(|r| r.start).unwrap_or(0);
        let high = regions.iter().map(|r| r.end).max().unwrap_or(0);
        let index_low = regions
            .iter()
            .find(|r| !r.immortal)
            .map(|r| r.start)
            .unwrap_or(low)
            & !(GRANULE - 1);
        let index_high = regions
            .iter()
            .filter(|r| !r.immortal)
            .map(|r| r.end)
            .max()
            .unwrap_or(high);
        let saved = HeapSaved {
            kind: found.kind,
            pages: found.pages,
            regions,
            types: vec![],
            free_method_table: 0,
            large_sizes: BTreeMap::new(),
            low,
            high,
            index_low,
            index_high,
            finalization: found.finalization,
        };
        let mut heap = Self {
            addresses: store.make(0, None)?,
            type_ids: store.make(0, None)?,
            sizes32: store.make(0, None)?,
            lengths: store.make(0, None)?,
            granule_first: store.make(0, None)?,
            dump,
            store,
            saved,
        };
        heap.walk(&merged)?;
        heap.dump.check()?;
        Ok(heap)
    }

    pub fn from_saved(dump: Arc<CoreDump>, store: ColumnStore, saved: HeapSaved) -> Result<Self> {
        let heap = Self {
            addresses: store.open("addresses")?,
            type_ids: store.open("typeIds")?,
            sizes32: store.open("sizes")?,
            lengths: store.open("lengths")?,
            granule_first: store.open("granuleFirst")?,
            dump,
            store,
            saved,
        };
        let n = heap.addresses.len();
        if heap.type_ids.len() != n
            || heap.sizes32.len() != n
            || heap.lengths.len() != n
            || heap
                .type_ids
                .iter()
                .any(|&t| t as usize >= heap.types.len())
        {
            return Err(Error::cache("object columns do not match the manifest"));
        }
        if heap.index_low > heap.index_high
            || heap.low > heap.high
            || heap.addresses.windows(2).any(|v| v[0] >= v[1])
            || heap.pages.iter().any(|p| p.start > p.end)
            || heap.finalization.iter().any(|q| {
                q.stride == 0
                    || q.ready_start > q.ready_end
                    || q.registered_start > q.registered_end
                    || (q.ready_end - q.ready_start) % q.stride != 0
            })
        {
            return Err(Error::cache("invalid heap bounds"));
        }
        let mut next_object = 0;
        let mut previous_end = 0;
        for (index, region) in heap.regions.iter().enumerate() {
            if region.index != index
                || region.start < previous_end
                || region.end <= region.start
                || !(region.start..=region.end).contains(&region.object_start)
                || region.first_object != next_object
                || region.end_object < region.first_object
                || region.end_object > n
                || region
                    .live_bytes
                    .checked_add(region.pending_bytes)
                    .is_none_or(|v| v > region.object_bytes)
            {
                return Err(Error::cache("invalid heap region"));
            }
            for object in region.first_object..region.end_object {
                let size = if heap.sizes32[object] == u32::MAX {
                    heap.large_sizes.get(&object).copied()
                } else {
                    Some(heap.sizes32[object] as u64)
                };
                if heap.addresses[object] < region.object_start
                    || size
                        .filter(|&size| size > 0)
                        .and_then(|size| heap.addresses[object].checked_add(size))
                        .is_none_or(|end| end > region.end)
                {
                    return Err(Error::cache("an object lies outside its heap region"));
                }
            }
            next_object = region.end_object;
            previous_end = region.end;
        }
        if next_object != n {
            return Err(Error::cache("heap regions do not cover the object columns"));
        }
        if !heap.granule_first.is_empty() {
            let granules = (heap.index_high - heap.index_low).div_ceil(GRANULE);
            if heap.granule_first.len() as u64 != granules + 1
                || heap.granule_first.iter().any(|&i| i < 0 || i as usize > n)
                || heap.granule_first.windows(2).any(|v| v[0] > v[1])
            {
                return Err(Error::cache("invalid heap address index"));
            }
        }
        Ok(heap)
    }

    pub fn count(&self) -> usize {
        self.addresses.len()
    }
    #[inline]
    pub fn size(&self, i: usize) -> u64 {
        let size = self.sizes32[i];
        if size == u32::MAX {
            self.large_sizes[&i]
        } else {
            size as u64
        }
    }
    pub fn method_table(&self, i: usize) -> MethodTableInfo {
        self.types[self.type_ids[i] as usize]
    }
    pub fn object_at(&self, address: u64) -> Option<usize> {
        let i = self.lower_bound(address);
        (self.addresses.get(i) == Some(&address)).then_some(i)
    }
    pub fn object_containing(&self, address: u64) -> Option<usize> {
        let i = self.lower_bound(address);
        if self.addresses.get(i) == Some(&address) {
            return Some(i);
        }
        let i = i.checked_sub(1)?;
        (address - self.addresses[i] < self.size(i)).then_some(i)
    }
    #[inline]
    pub fn lower_bound(&self, address: u64) -> usize {
        let (lo, hi) = if address >= self.index_low
            && address < self.index_high
            && !self.granule_first.is_empty()
        {
            let g = ((address - self.index_low) >> 21) as usize;
            (
                self.granule_first[g] as usize,
                self.granule_first[g + 1] as usize,
            )
        } else if let Some(r) = self.region_containing(address) {
            (self.regions[r].first_object, self.regions[r].end_object)
        } else {
            (0, self.count())
        };
        lo + self.addresses[lo..hi].partition_point(|&a| a < address)
    }
    pub fn region_of(&self, i: usize) -> usize {
        self.regions
            .partition_point(|r| r.first_object <= i)
            .saturating_sub(1)
    }
    pub fn region_containing(&self, address: u64) -> Option<usize> {
        let i = self
            .regions
            .partition_point(|r| r.start <= address)
            .checked_sub(1)?;
        (address < self.regions[i].end).then_some(i)
    }
    pub fn label(&self) -> &'static str {
        if self.kind.gc == Gc::Satori {
            "Satori heap"
        } else {
            "GC heap"
        }
    }
    pub fn owns_memory(&self, start: u64, end: u64) -> bool {
        if self.kind.gc == Gc::Satori {
            return self.pages.iter().any(|p| start < p.end && end > p.start);
        }
        let i = self.regions.partition_point(|r| r.end <= start);
        i < self.regions.len() && self.regions[i].start < end
    }

    fn walk(&mut self, gaps: &[(u64, u64)]) -> Result<()> {
        let results: Vec<_> = self
            .regions
            .par_iter()
            .map(|r| walk_region(&self.dump, self.kind.gc, gaps, r, |_, _, _, _| {}))
            .collect();
        self.dump.check()?;
        let mut total = 0usize;
        for (r, result) in self.saved.regions.iter_mut().zip(results) {
            r.first_object = total;
            total = total
                .checked_add(result.objects)
                .filter(|&n| n <= i32::MAX as usize)
                .ok_or_else(|| Error("the dump has more than 2,147,483,647 objects".into()))?;
            r.end_object = total;
            r.object_bytes = result.object_bytes;
            r.free_bytes = result.free_bytes;
            r.unparsed_bytes = result.unparsed;
            r.walk_stopped = result.stopped;
            r.walk_stopped_at = result.stopped_at;
            r.walk_stopped_after_gap = result.stopped_after_gap;
            r.first_unparsed_object = result.first_unparsed_object;
        }
        self.addresses = self.store.make(total, Some("addresses"))?;
        self.sizes32 = self.store.make(total, Some("sizes"))?;
        self.lengths = self.store.make(total, Some("lengths"))?;
        let mut tables: Column<u64> = self.store.make(total, None)?;
        let (mut addresses, mut mts, mut sizes, mut lengths) = (
            &mut self.addresses[..],
            &mut tables[..],
            &mut self.sizes32[..],
            &mut self.lengths[..],
        );
        let mut parts = Vec::with_capacity(self.saved.regions.len());
        for r in &self.saved.regions {
            let n = r.end_object - r.first_object;
            let (a, ar) = addresses.split_at_mut(n);
            addresses = ar;
            let (t, tr) = mts.split_at_mut(n);
            mts = tr;
            let (s, sr) = sizes.split_at_mut(n);
            sizes = sr;
            let (l, lr) = lengths.split_at_mut(n);
            lengths = lr;
            parts.push((r, a, t, s, l));
        }
        let dump = &self.dump;
        let gc = self.saved.kind.gc;
        let valid = parts.into_par_iter().all(|(r, a, t, s, l)| {
            let mut i = 0;
            let result = walk_region(dump, gc, gaps, r, |address, mt, size, length| {
                if i < a.len() {
                    a[i] = address;
                    t[i] = mt;
                    s[i] = size.min(u32::MAX as u64) as u32;
                    l[i] = length;
                }
                i += 1;
            });
            result.objects == a.len()
        });
        self.dump.check()?;
        if !valid {
            return Err(Error(
                "the dump changed while its objects were being read".into(),
            ));
        }
        let mut type_index = HashMap::new();
        self.type_ids = self.store.make(total, Some("typeIds"))?;
        for (i, &mt) in tables.iter().enumerate() {
            let id = *type_index.entry(mt).or_insert_with(|| {
                let id = self.saved.types.len() as u32;
                self.saved.types.push(MethodTableInfo {
                    address: mt,
                    flags: self.dump.u32(mt).unwrap_or(0),
                    base_size: self.dump.u32(mt.wrapping_add(4)).unwrap_or(0),
                });
                id
            });
            self.type_ids[i] = id;
            if self.sizes32[i] == u32::MAX {
                let t = self.saved.types[id as usize];
                self.saved.large_sizes.insert(
                    i,
                    align_up(
                        t.base_size as u64 + self.lengths[i] as u64 * t.component_size() as u64,
                        8,
                    )
                    .unwrap(),
                );
            }
        }
        self.saved.free_method_table = self
            .regions
            .iter()
            .find_map(|r| {
                let mt = self.dump.u64(r.object_start)? & !7;
                (mt != 0
                    && self.dump.u32(mt) == Some(0x8000_0001)
                    && self.dump.u32(mt.wrapping_add(4)) == Some(24))
                .then_some(mt)
            })
            .unwrap_or(0);
        let granules = (self.index_high - self.index_low).div_ceil(GRANULE) as usize;
        // Distant frozen mappings can make a dense address index larger than the heap itself.
        if granules <= 1 << 24 {
            self.granule_first = self.store.make(granules + 1, Some("granuleFirst"))?;
            let mut i = 0;
            for g in 0..=granules {
                let address = self.index_low + ((g as u64) << 21);
                while i < total && self.addresses[i] < address {
                    i += 1;
                }
                self.granule_first[g] = i as i32;
            }
        } else {
            self.granule_first = self.store.make(0, Some("granuleFirst"))?;
        }
        Ok(())
    }
}

fn walk_region(
    dump: &CoreDump,
    gc: Gc,
    gaps: &[(u64, u64)],
    r: &Region,
    mut emit: impl FnMut(u64, u64, u64, u32),
) -> RegionWalk {
    let mut w = RegionWalk::default();
    if r.gc_storage.is_some() {
        return w;
    }
    let (mut objects, mut tables) = (Cursor::new(dump, true), Cursor::new(dump, false));
    let (mut last_mt, mut flags, mut base, mut free) = (0, 0, 0, false);
    let limit = r.used.min(r.end);
    let mut gap = gaps.partition_point(|&(_, end)| end <= r.start);
    let (mut o, mut after_zeros) = (r.object_start, false);
    while o <= limit.saturating_sub(8) && o < limit {
        while gap < gaps.len() && gaps[gap].1 <= o {
            gap += 1;
        }
        if gap < gaps.len() && gaps[gap].0 <= o {
            w.unparsed += gaps[gap].1.min(limit) - o;
            o = gaps[gap].1;
            after_zeros = true;
            continue;
        }
        if r.alloc_start != 0 && o >= r.alloc_start && o < r.alloc_end {
            w.free_bytes += r.alloc_end.min(limit) - o;
            o = r.alloc_end;
            continue;
        }
        if gc == Gc::Satori && r.generation < 0 {
            let end = if r.alloc_start > o {
                r.alloc_start.min(limit)
            } else {
                limit
            };
            w.unparsed += end - o;
            o = end;
            continue;
        }
        let Some(raw) = objects.u64(o) else {
            w.stopped = true;
            w.stopped_at = Some(o);
            w.stopped_after_gap = after_zeros;
            w.unparsed += limit - o;
            break;
        };
        let mt = raw & !7;
        if mt == 0 {
            w.first_unparsed_object.get_or_insert(o);
            let mut next = o + 8;
            while next <= limit.saturating_sub(8) && objects.u64(next) == Some(0) {
                next += 8;
            }
            w.unparsed += next - o;
            o = next;
            after_zeros = true;
            continue;
        }
        let (mut size, mut length, mut valid) = (0, 0, true);
        if mt != last_mt {
            if let (Some(f), Some(b)) = (
                tables.u32(mt),
                mt.checked_add(4).and_then(|at| tables.u32(at)),
            ) {
                last_mt = mt;
                flags = f;
                base = b;
                free = flags == 0x8000_0001
                    && base == 24
                    && mt.checked_add(24).and_then(|at| tables.u64(at)) == Some(0);
            } else {
                valid = false;
            }
        }
        if valid {
            size = base as u64;
            if flags & MethodTableInfo::HAS_COMPONENT_SIZE != 0 {
                if let Some(n) = objects.u32(o + 8) {
                    length = n;
                    size = (size + n as u64 * (flags & 0xffff) as u64 + 7) & !7;
                } else {
                    valid = false;
                }
            }
            valid = valid
                && size >= 24
                && size.is_multiple_of(8)
                && o.checked_add(size)
                    .is_some_and(|end| end <= r.end && (free || end <= limit.min(r.committed)));
            if valid && after_zeros {
                let after = if o + size < limit {
                    objects.u64(o + size).map(|v| v & !7)
                } else {
                    Some(0)
                };
                valid = (16..1 << 24).contains(&base)
                    && o + size <= limit
                    && after.is_some_and(|mt| {
                        mt == 0
                            || mt
                                .checked_add(4)
                                .and_then(|at| tables.u32(at))
                                .is_some_and(|b| (16..1 << 24).contains(&b))
                    });
            }
        }
        if !valid {
            if after_zeros && o < r.alloc_end {
                w.unparsed += r.alloc_end.min(limit) - o;
                o = r.alloc_end;
                after_zeros = false;
                last_mt = 0;
                continue;
            }
            w.stopped = true;
            w.stopped_at = Some(o);
            w.stopped_after_gap = after_zeros;
            w.unparsed += limit - o;
            break;
        }
        after_zeros = false;
        if free {
            w.free_bytes += (o + size).min(limit) - o;
        } else {
            emit(o, mt, size, length);
            w.objects += 1;
            w.object_bytes += size;
        }
        o += size;
    }
    w
}

pub fn pages_for(regions: &[Region]) -> Vec<Page> {
    let mut starts = BTreeSet::new();
    for r in regions {
        let mut a = r.start & !((1 << 30) - 1);
        while a < r.end {
            starts.insert(a);
            let Some(next) = a.checked_add(1 << 30) else {
                break;
            };
            a = next;
        }
    }
    starts
        .into_iter()
        .filter_map(|start| start.checked_add(1 << 30).map(|end| Page { start, end }))
        .collect()
}
