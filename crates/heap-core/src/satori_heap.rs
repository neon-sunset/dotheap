use crate::{
    Error, Result,
    bytes::align_up,
    core_dump::{CoreDump, Cursor},
    heap::{FinalizationQueue, GRANULE, Gc, HeapDiscovery, HeapKind, Page, Region},
    runtime::RuntimeInfo,
};
use std::collections::{HashMap, HashSet};

const FIRST_OBJECT: u64 = 36888;
const PAGE_GRANULE: u64 = 1 << 30;
const QUEUE_KINDS: &[&str] = &[
    "Allocator",
    "WorkChunk",
    "RecyclerEphemeral",
    "RecyclerEphemeralFinalizationTracking",
    "RecyclerTenured",
    "RecyclerTenuredFinalizationTracking",
    "RecyclerNursery",
    "RecyclerFinalizationPending",
    "RecyclerFinalizationScanComplete",
    "RecyclerStaying",
    "RecyclerRelocating",
    "RecyclerRelocated",
    "RecyclerRelocatedToHigherGen",
    "RecyclerRelocationTarget",
    "RecyclerDeferredSweep",
    "RecyclerReusable",
    "RecyclerDemoted",
];

struct Layout {
    name: &'static str,
    used: u64,
    committed: u64,
    trackers: u64,
    prev: u64,
    next: u64,
    queue: u64,
    occupancy: u64,
    count: u64,
    pinned: u64,
}
const LAYOUTS: &[Layout] = &[
    Layout {
        name: "Satori since 9df7b3d86e4 (2026-10-15)",
        used: 88,
        committed: 96,
        trackers: 120,
        prev: 144,
        next: 152,
        queue: 160,
        occupancy: 168,
        count: 220,
        pinned: 228,
    },
    Layout {
        name: "Satori from 3ed9d3c8680 through dc9a20118d2",
        used: 80,
        committed: 88,
        trackers: 112,
        prev: 136,
        next: 144,
        queue: 152,
        occupancy: 160,
        count: 176,
        pinned: 184,
    },
    Layout {
        name: "Satori before 3ed9d3c8680",
        used: 80,
        committed: 88,
        trackers: 112,
        prev: 128,
        next: 136,
        queue: 144,
        occupancy: 152,
        count: 168,
        pinned: 176,
    },
];

fn unsupported(bad: usize, total: usize) -> Error {
    Error(format!(
        "{bad} of {total} checks of the region headers fail with every known Satori layout. \
         The dump may be incomplete or corrupted, or the runtime layout may be unsupported."
    ))
}

pub(crate) fn find(
    dump: &CoreDump,
    runtime: Option<&RuntimeInfo>,
) -> Result<Option<HeapDiscovery>> {
    let (pages, starts, mismatches) = find_pages(dump);
    dump.check()?;
    if pages.is_empty() {
        return Ok(None);
    }
    if mismatches * 10 >= starts.len().max(1) {
        return Err(unsupported(mismatches, starts.len()));
    }
    let scored: Vec<_> = LAYOUTS
        .iter()
        .map(|l| {
            (
                l,
                starts
                    .iter()
                    .map(|&r| score(dump, r, l, &starts))
                    .sum::<usize>(),
            )
        })
        .collect();
    let total = starts.len() * 4;
    let best = scored.iter().map(|(_, s)| *s).max().unwrap();
    if best * 2 < total {
        return Err(unsupported(total - best, total));
    }
    let tied: Vec<_> = scored.iter().filter(|(_, s)| *s == best).collect();
    if tied.len() != 1 {
        return Err(Error(format!(
            "the region headers pass the same number of checks with the Satori layouts {}; \
                dotheap needs a check that tells them apart",
            tied.iter()
                .map(|(l, _)| l.name)
                .collect::<Vec<_>>()
                .join(" and ")
        )));
    }
    let l = tied[0].0;
    let mut regions: Vec<_> = starts
        .iter()
        .enumerate()
        .map(|(i, &r)| read_region(dump, r, i, l))
        .collect();
    assign_allocating_threads(dump, runtime, &mut regions);
    let mut finalization = finalization_queue(dump, &pages);
    classify_gc_storage(dump, &pages, &mut regions, &finalization, l);
    for r in regions.iter().filter(|r| !r.immortal) {
        let mut chunk = dump.u64(r.start + l.trackers).unwrap_or(0);
        let mut seen = HashSet::new();
        while chunk != 0 && seen.len() < 1 << 16 && seen.insert(chunk) {
            let Some(top) = dump.u64(chunk) else {
                break;
            };
            let count = top & 0xffff_ffff;
            if count >= 1 << 24 {
                break;
            }
            if count > 0
                && let Some(data) = chunk.checked_add(16)
                && let Some(end) = data.checked_add(count * 8)
            {
                finalization.push(FinalizationQueue {
                    start: data,
                    end,
                    ready_start: data,
                    ready_end: data,
                    stride: 8,
                    registered_start: data,
                    registered_end: end,
                });
            }
            chunk = chunk
                .checked_add(8)
                .and_then(|at| dump.u64(at))
                .unwrap_or(0);
        }
    }
    Ok(Some(HeapDiscovery {
        pages,
        regions,
        kind: HeapKind {
            gc: Gc::Satori,
            layout: l.name.into(),
            detection: format!("region headers match the layout in {best} of {total} checks"),
            stock: None,
        },
        unparseable: crate::coreclr_heap::thread_allocation_contexts(
            dump,
            runtime.and_then(|r| r.descriptor.as_ref()),
        ),
        finalization,
    }))
}

fn classify_gc_storage(
    dump: &CoreDump,
    pages: &[Page],
    regions: &mut [Region],
    finalization: &[FinalizationQueue],
    layout: &Layout,
) {
    let mut heads = Vec::new();
    for r in regions.iter_mut() {
        if r.generation >= 0 {
            heads.extend(dump.u64(r.start + layout.trackers));
            heads.extend(dump.u64(r.start + 56));
        } else if r.queue == "(in no queue)"
            && finalization.iter().any(|q| {
                q.stride == 16
                    && q.start == r.object_start
                    && q.end == r.alloc_start
                    && q.end <= r.used
                    && r.used <= r.committed
            })
        {
            r.gc_storage = Some("finalization queue".into());
        }
    }
    if let Some(heap) = pages.first().and_then(|p| dump.u64(p.start + 32))
        && dump.u64(heap) == Some(heap)
    {
        heads.extend(allocator_work_chunk_heads(dump, heap));
    }
    let work_regions: HashMap<_, _> = regions
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.generation < 0
                && r.queue == "(in no queue)"
                && r.gc_storage.is_none()
                && r.end.checked_sub(r.start) == Some(GRANULE)
                && r.owner_thread_tag == 0
                && r.attachment_point == 0
                && r.alloc_end == r.end
                && r.object_start
                    .checked_add((GRANULE - FIRST_OBJECT - 24) / 4096 * 4096)
                    == Some(r.alloc_start)
                && r.used >= r.alloc_start
                && r.committed >= r.used
        })
        .map(|(i, r)| (r.start, i))
        .collect();
    let mut seen = HashSet::new();
    let mut cursor = Cursor::new(dump, true);
    for mut chunk in heads {
        while chunk != 0 && seen.insert(chunk) {
            let Some(&i) = work_regions.get(&(chunk & !(GRANULE - 1))) else {
                break;
            };
            let r = &mut regions[i];
            if chunk < r.object_start
                || chunk >= r.alloc_start
                || !(chunk - r.object_start).is_multiple_of(4096)
                || !cursor
                    .u64(chunk)
                    .is_some_and(|top| top == u64::MAX || top & 0xffff_ffff <= 510)
            {
                break;
            }
            let Some(next) = cursor.u64(chunk + 8) else {
                break;
            };
            r.gc_storage
                .get_or_insert_with(|| "mark work chunks".into());
            chunk = next;
        }
    }
}

fn allocator_work_chunk_heads(dump: &CoreDump, heap: u64) -> Vec<u64> {
    let Some(list) = heap.checked_add(80).and_then(|at| dump.u64(at)) else {
        return Vec::new();
    };
    let queues: Option<Vec<_>> = (0..9)
        .map(|i| heap.checked_add(8 + i * 8).and_then(|at| dump.u64(at)))
        .collect();
    let Some(queues) = queues else {
        return Vec::new();
    };
    let mut heads = Vec::new();
    if queues.iter().all(|&queue| dump.u32(queue) == Some(0)) {
        heads.extend(dump.u64(list));
    }

    let alignment = if dump.platform.is_arm64() { 128 } else { 64 };
    if let Some(count) = shard_count(dump, list, 128, 128)
        && queues.iter().all(|&queue| {
            shard_count(dump, queue, alignment, 2 * alignment).is_some_and(|count| {
                (0..count).all(|i| dump.u32(queue + alignment + i * 2 * alignment) == Some(0))
            })
        })
    {
        for i in 0..count {
            heads.extend(dump.u64(list + 128 + i * 128));
        }
    }
    heads
}

fn shard_count(dump: &CoreDump, address: u64, alignment: u64, stride: u64) -> Option<u64> {
    if address == 0 || !address.is_multiple_of(alignment) {
        return None;
    }
    let count = dump.u64(address)?.checked_add(1)?;
    if !count.is_power_of_two() || count > 1 << 16 {
        return None;
    }
    let end = address.checked_add(alignment + count * stride)?;
    dump.u64(end - 8)?;
    Some(count)
}

fn find_pages(dump: &CoreDump) -> (Vec<Page>, Vec<u64>, usize) {
    let (mut pages, mut starts, mut mismatches) = (Vec::<Page>::new(), Vec::new(), 0);
    for s in &dump.segments {
        let Some(mut a) = align_up(s.address, PAGE_GRANULE) else {
            continue;
        };
        while a.checked_add(64).is_some_and(|end| end <= s.end()) {
            if pages.last().is_none_or(|p| a >= p.end)
                && let (Some(end), Some(first), Some(map)) =
                    (dump.u64(a + 8), dump.u64(a + 24), dump.u64(a + 56))
                && end > a
                && (end - a) % PAGE_GRANULE == 0
                && first > a
                && first < end
                && map > a
                && map < first
            {
                let granules = ((end - a) >> 21) as usize;
                // The region map must fit before the first region, as SatoriPage requires.
                if map
                    .checked_add(granules as u64)
                    .is_some_and(|end| end <= first)
                {
                    pages.push(Page { start: a, end });
                    if let Some(bytes) = dump.bytes(map, granules) {
                        for i in ((first - a) >> 21) as usize..granules {
                            if bytes[i] == 1 {
                                let r = a + ((i as u64) << 21);
                                if dump.u64(r + 32) != Some(a) {
                                    mismatches += 1;
                                }
                                starts.push(r);
                            }
                        }
                    }
                }
            }
            let Some(next) = a.checked_add(PAGE_GRANULE) else {
                break;
            };
            a = next;
        }
    }
    (pages, starts, mismatches)
}

fn score(dump: &CoreDump, r: u64, l: &Layout, starts: &[u64]) -> usize {
    let region_or_null = |v: Option<u64>| {
        v.is_some_and(|v| v == 0 || (v & (GRANULE - 1) == 0 && starts.binary_search(&v).is_ok()))
    };
    let mut n = 0;
    if let (Some(end), Some(used), Some(committed), Some(occupancy)) = (
        dump.u64(r + 24),
        dump.u64(r + l.used),
        dump.u64(r + l.committed),
        dump.u64(r + l.occupancy),
    ) && end > r + FIRST_OBJECT
        && used >= r + FIRST_OBJECT - 8
        && used <= committed
        && committed <= end
        && committed & 0xfff == 0
        && occupancy <= end - r - FIRST_OBJECT
    {
        n += 1;
    }
    n += usize::from(region_or_null(dump.u64(r + l.prev)));
    n += usize::from(region_or_null(dump.u64(r + l.next)));
    if let Some(q) = dump.u64(r + l.queue)
        && (q == 0
            || (q & 7 == 0
                && starts.binary_search(&(q & !(GRANULE - 1))).is_err()
                && dump
                    .u32(q)
                    .is_some_and(|k| (k as usize) < QUEUE_KINDS.len())))
    {
        n += 1;
    }
    n
}

fn read_region(dump: &CoreDump, r: u64, index: usize, l: &Layout) -> Region {
    let queue = dump.u64(r + l.queue).unwrap_or(0);
    let kind = if queue == 0 {
        -1
    } else {
        dump.u32(queue).unwrap_or(u32::MAX) as i32
    };
    let queue = if kind < 0 {
        "(in no queue)".into()
    } else {
        QUEUE_KINDS
            .get(kind as usize)
            .map(|s| (*s).into())
            .unwrap_or_else(|| format!("kind {kind}"))
    };
    let generation = dump.u32(r + 16).unwrap_or(0) as i32;
    let owner_thread_tag = dump.u64(r).unwrap_or(0);
    let attachment_point = dump.u64(r + 48).unwrap_or(0);
    let owner_thread = (owner_thread_tag != 0)
        .then(|| {
            dump.threads
                .iter()
                .find(|t| t.thread_pointer == Some(owner_thread_tag))
                .map(|t| t.tid)
        })
        .flatten();
    Region {
        index,
        start: r,
        end: dump.u64(r + 24).unwrap_or(r),
        object_start: r + FIRST_OBJECT,
        generation,
        generation_name: match generation {
            -1 => "none".into(),
            3 => "immortal".into(),
            _ => format!("gen{generation}"),
        },
        queue,
        gc_heap: -1,
        used: dump.u64(r + l.used).unwrap_or(r),
        committed: dump.u64(r + l.committed).unwrap_or(r),
        reusable_for: dump.u8(r + 40).unwrap_or(0),
        alloc_start: dump.u64(r + 64).unwrap_or(0),
        alloc_end: dump.u64(r + 72).unwrap_or(0),
        occupancy: dump.u64(r + l.occupancy).unwrap_or(0),
        obj_count: dump.u32(r + l.count).unwrap_or(0) as i32,
        has_pinned_objects: dump.u8(r + l.pinned).unwrap_or(0) != 0,
        immortal: generation == 3,
        owner_thread_tag,
        attachment_point,
        owner_thread,
        ..Region::default()
    }
}

pub fn thread_holding(dump: &CoreDump, address: u64) -> Option<i32> {
    if address == 0 {
        return None;
    }
    let segment = dump.segment_index(address)?;
    dump.threads
        .iter()
        .find(|t| dump.segment_index(t.stack_pointer) == Some(segment))
        .map(|t| t.tid)
}

pub(crate) fn assign_allocating_threads(
    dump: &CoreDump,
    runtime: Option<&RuntimeInfo>,
    regions: &mut [Region],
) {
    let mut owners = HashMap::new();
    if dump.platform.is_linux()
        && let Some(descriptor) = runtime.and_then(|r| r.descriptor.as_ref())
    {
        for (context, tid) in descriptor.allocation_contexts(dump) {
            if tid <= 0 || !dump.threads.iter().any(|t| t.tid == tid) {
                continue;
            }
            // gc_alloc_context::gc_reserved_1 and gc_reserved_2 hold the regular and large regions.
            for offset in [32, 40] {
                if let Some(slot) = context.checked_add(offset) {
                    owners
                        .entry(slot)
                        .and_modify(|owner| {
                            if *owner != Some(tid) {
                                *owner = None;
                            }
                        })
                        .or_insert(Some(tid));
                }
            }
        }
    }
    for region in regions {
        region.allocating_thread = if region.attachment_point != 0
            && dump.u64(region.attachment_point) == Some(region.start)
        {
            match owners.get(&region.attachment_point) {
                Some(owner) => *owner,
                None => thread_holding(dump, region.attachment_point),
            }
        } else {
            None
        };
    }
}

fn finalization_queue(dump: &CoreDump, pages: &[Page]) -> Vec<FinalizationQueue> {
    let Some(heap) = pages
        .first()
        .and_then(|p| dump.u64(p.start + 32))
        .filter(|&h| h != 0)
    else {
        return vec![];
    };
    let Some(limit) = heap.checked_add(0x10000) else {
        return vec![];
    };
    for q in (heap..limit).step_by(64) {
        if q.checked_add(152).and_then(|p| dump.u64(p)) != Some(heap) {
            continue;
        }
        let (Some(enqueue), Some(dequeue), Some(mask), Some(data)) = (
            dump.u64(q),
            dump.u64(q + 64),
            dump.u64(q + 128),
            dump.u64(q + 144),
        ) else {
            continue;
        };
        if data == 0
            || mask >= 1 << 32
            || (mask + 1) & mask != 0
            || enqueue < dequeue
            || enqueue - dequeue > mask + 1
        {
            continue;
        }
        let capacity = mask + 1;
        let Some(end) = data.checked_add(capacity * 16) else {
            continue;
        };
        let make = |start, stop| FinalizationQueue {
            start: data,
            end,
            ready_start: start,
            ready_end: stop,
            stride: 16,
            registered_start: 0,
            registered_end: 0,
        };
        if enqueue == dequeue {
            return vec![make(data, data)];
        }
        let (first, count) = (dequeue & mask, enqueue - dequeue);
        let run = count.min(capacity - first);
        let mut queues = vec![make(data + first * 16, data + (first + run) * 16)];
        if count > run {
            queues.push(make(data, data + (count - run) * 16));
        }
        return queues;
    }
    vec![]
}

pub(crate) fn verify_links(
    heap: &crate::heap::ManagedHeap,
    result: &mut crate::verification::HeapVerification,
) {
    use crate::verification::VerificationKind::{Invalid, MissingMemory, Unverified};
    if heap.kind.gc != Gc::Satori {
        return;
    }
    let Some(layout) = LAYOUTS.iter().find(|l| l.name == heap.kind.layout) else {
        return;
    };
    let dump = &heap.dump;
    let starts: HashSet<_> = heap.regions.iter().map(|r| r.start).collect();
    let mut visited = HashSet::new();
    for region in &heap.regions {
        if dump.u64(region.start + layout.queue) == Some(0) {
            continue;
        }
        let mut chain = HashSet::new();
        let mut at = region.start;
        let mut field = at + layout.next;
        while starts.contains(&at) && visited.insert(at) {
            chain.insert(at);
            field = at + layout.next;
            at = dump.u64(field).unwrap_or(0);
        }
        if chain.contains(&at) && chain.len() > 1 {
            result.issue(
                Invalid,
                "Satori region next",
                field,
                None,
                Some(at),
                "The region queue contains a cycle.",
            );
        }
    }
    for region in &heap.regions {
        let at = region.start;
        let Some(queue) = dump.u64(at + layout.queue) else {
            result.issue(
                MissingMemory,
                "Satori region queue",
                at + layout.queue,
                None,
                None,
                "The dump does not contain the queue pointer.",
            );
            continue;
        };
        if queue != 0 {
            match dump.u32(queue) {
                Some(kind) if (kind as usize) < QUEUE_KINDS.len() => {}
                Some(kind) => result.issue(
                    Invalid,
                    "Satori queue kind",
                    queue,
                    None,
                    Some(kind as u64),
                    "The queue kind is outside the known range.",
                ),
                None => result.issue(
                    MissingMemory,
                    "Satori queue kind",
                    queue,
                    None,
                    None,
                    "The dump does not contain the queue kind.",
                ),
            }
        }
        for (offset, reverse, name) in [
            (layout.next, layout.prev, "Satori region next"),
            (layout.prev, layout.next, "Satori region previous"),
        ] {
            let Some(link) = dump.u64(at + offset) else {
                result.issue(
                    MissingMemory,
                    name,
                    at + offset,
                    None,
                    None,
                    "The dump does not contain the region link.",
                );
                continue;
            };
            if link == 0 {
                continue;
            }
            if link == at || !starts.contains(&link) {
                result.issue(
                    Invalid,
                    name,
                    at + offset,
                    None,
                    Some(link),
                    "The link does not refer to another known region.",
                );
            } else if queue != 0 {
                for (field, expected, message) in [
                    (
                        link + layout.queue,
                        queue,
                        "Linked regions belong to different queues.",
                    ),
                    (
                        link + reverse,
                        at,
                        "The adjacent region does not link back to this region.",
                    ),
                ] {
                    match dump.u64(field) {
                        Some(value) if value == expected => {}
                        Some(value) => {
                            result.issue(Invalid, name, field, None, Some(value), message)
                        }
                        None => result.issue(
                            MissingMemory,
                            name,
                            field,
                            None,
                            None,
                            "The dump does not contain the adjacent region's link.",
                        ),
                    }
                }
            }
        }
        let mut field = at + layout.trackers;
        let mut seen = HashSet::new();
        loop {
            let Some(chunk) = dump.u64(field) else {
                result.issue(
                    MissingMemory,
                    "Satori finalization trackers",
                    field,
                    None,
                    None,
                    "The dump does not contain the tracker link.",
                );
                break;
            };
            if chunk == 0 {
                break;
            }
            if chunk & 7 != 0 || !seen.insert(chunk) {
                result.issue(
                    Invalid,
                    "Satori finalization trackers",
                    field,
                    None,
                    Some(chunk),
                    "The tracker list contains a cycle or a misaligned pointer.",
                );
                break;
            }
            if seen.len() > 65536 {
                result.issue(
                    Unverified,
                    "Satori finalization trackers",
                    field,
                    None,
                    Some(chunk),
                    "The tracker list exceeds the verification limit of 65,536 chunks.",
                );
                break;
            }
            let Some(top) = dump.u64(chunk) else {
                result.issue(
                    MissingMemory,
                    "Satori finalization tracker",
                    chunk,
                    None,
                    None,
                    "The dump does not contain the tracker header.",
                );
                break;
            };
            if top & 0xffff_ffff > 510 {
                result.issue(
                    Invalid,
                    "Satori finalization tracker count",
                    chunk,
                    None,
                    Some(top),
                    "The tracker count exceeds its 4 KiB chunk capacity.",
                );
                break;
            }
            let Some(next) = chunk.checked_add(8) else {
                break;
            };
            field = next;
        }
    }
}
