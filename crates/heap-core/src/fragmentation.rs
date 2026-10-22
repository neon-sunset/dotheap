use crate::{
    Result,
    analysis::HeapSnapshot,
    heap::{GRANULE, Gc, Region},
    type_names::short,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reason {
    pub name: String,
    pub explanation: String,
    pub regions: usize,
    pub waste: u64,
    pub live: u64,
    pub region_indices: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeBytes {
    pub name: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeShare {
    pub name: String,
    pub bytes: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contributor {
    pub id: String,
    pub name: String,
    pub regions: BTreeSet<usize>,
    pub live_bytes: u64,
    pub waste: f64,
    pub samples: Vec<usize>,
    pub survivor_types: Vec<TypeBytes>,
    pub dead_around: Vec<TypeShare>,
    pub dead_around_bytes: f64,
}

impl Contributor {
    fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            ..Self::default()
        }
    }

    fn add(&mut self, snapshot: &HeapSnapshot, object: usize, share: f64) {
        self.regions.insert(snapshot.heap.region_of(object));
        self.live_bytes += snapshot.heap.size(object);
        self.waste += share;
        self.samples.push(object);
    }

    fn select_samples(&mut self, shares: &[f64]) {
        self.samples
            .sort_by(|&a, &b| shares[b].total_cmp(&shares[a]).then(a.cmp(&b)));
        self.samples.truncate(300);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[repr(C)]
pub struct Gen2Heap {
    pub heap: i32,
    pub size: u64,
    pub fragmentation: u64,
}

impl Gen2Heap {
    pub fn above_threshold(&self) -> bool {
        self.fragmentation >= 200_000 && self.fragmentation as f64 >= self.size as f64 * 0.25
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FragmentationReport {
    pub reasons: Vec<Reason>,
    pub gen2_by_heap: Vec<Gen2Heap>,
    pub by_type: Vec<Contributor>,
    pub by_owner: Vec<Contributor>,
    pub by_code: Vec<Contributor>,
    pub total_waste: u64,
}

impl FragmentationReport {
    pub fn server_vote(&self) -> bool {
        self.gen2_by_heap.len() > 1 && self.gen2_by_heap.iter().any(Gen2Heap::above_threshold)
    }
}

impl HeapSnapshot {
    fn fragmentation_by_code(
        &self,
        shares: &[f64],
        top_nodes: &[Option<usize>],
    ) -> Result<Vec<Contributor>> {
        let count = self.heap.count();
        let root = self.graph.root_node();
        let application: Vec<_> = self
            .heap
            .types
            .iter()
            .map(|t| is_application_type(&self.names.name(t.address)))
            .collect();
        let mut app_owner = vec![None; root + 1];
        let mut stack = vec![root];
        while let Some(parent) = stack.pop() {
            for &child in self.graph.dominator_rows.row(parent) {
                let child = child as usize;
                app_owner[child] =
                    if child < count && application[self.heap.type_ids[child] as usize] {
                        Some(child)
                    } else {
                        app_owner[parent]
                    };
                stack.push(child);
            }
        }
        let holders = application_holders(&self.heap, &self.graph, &application);
        let mut dead_by_region: BTreeMap<usize, BTreeMap<u32, u64>> = BTreeMap::new();
        for (index, region) in self
            .heap
            .regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.live_bytes > 0 && r.object_bytes > r.live_bytes)
        {
            let types = dead_by_region.entry(index).or_default();
            for object in
                (region.first_object..region.end_object).filter(|&o| self.graph.state(o) != "live")
            {
                *types.entry(self.heap.type_ids[object]).or_default() += self.heap.size(object);
            }
        }
        let mut areas: BTreeMap<i64, Contributor> = BTreeMap::new();
        let mut survivors: BTreeMap<i64, BTreeMap<u32, u64>> = BTreeMap::new();
        let mut live_by_region: BTreeMap<i64, BTreeMap<usize, u64>> = BTreeMap::new();
        let mut runtime_keys = HashMap::new();
        let mut runtime_names = HashMap::new();
        for (object, &share) in shares.iter().enumerate().filter(|(_, share)| **share > 0.0) {
            let shared = app_owner[object].is_none() && holders[object].is_some();
            let owner = app_owner[object].or(holders[object]);
            let key = if let Some(owner) = owner {
                let type_id = self.heap.type_ids[owner];
                let key = type_id as i64 | if shared { 1 << 40 } else { 0 };
                areas.entry(key).or_insert_with(|| {
                    let name = self.type_name(owner);
                    Contributor::new(
                        format!("c{key}"),
                        format!(
                            "{}  ·  {}{}",
                            namespace(&name),
                            short(&name),
                            if shared { " (shared)" } else { "" }
                        ),
                    )
                });
                key
            } else {
                let top = top_nodes[object].unwrap_or(object);
                if let Some(&key) = runtime_keys.get(&top) {
                    key
                } else {
                    let name = if top >= count {
                        format!("runtime: {}", self.node_name(top)?)
                    } else {
                        format!("runtime: {}", short(&self.type_name(top)))
                    };
                    let next = -1 - runtime_names.len() as i64;
                    let key = *runtime_names.entry(name.clone()).or_insert(next);
                    runtime_keys.insert(top, key);
                    areas
                        .entry(key)
                        .or_insert_with(|| Contributor::new(format!("c{key}"), name));
                    key
                }
            };
            areas.get_mut(&key).unwrap().add(self, object, share);
            *survivors
                .entry(key)
                .or_default()
                .entry(self.heap.type_ids[object])
                .or_default() += self.heap.size(object);
            *live_by_region
                .entry(key)
                .or_default()
                .entry(self.heap.region_of(object))
                .or_default() += self.heap.size(object);
        }
        for (area, regions) in live_by_region {
            let mut dead: BTreeMap<u32, f64> = BTreeMap::new();
            for (region, live) in regions {
                if let Some(types) = dead_by_region.get(&region) {
                    let part = live as f64 / self.heap.regions[region].live_bytes as f64;
                    for (&type_id, &bytes) in types {
                        *dead.entry(type_id).or_default() += bytes as f64 * part;
                    }
                }
            }
            let contributor = areas.get_mut(&area).unwrap();
            contributor.dead_around_bytes = dead.values().sum();
            let mut dead: Vec<_> = dead.into_iter().collect();
            dead.sort_by(|(a, av), (b, bv)| bv.total_cmp(av).then(a.cmp(b)));
            contributor.dead_around = dead
                .into_iter()
                .take(12)
                .map(|(type_id, bytes)| TypeShare {
                    name: self.names.name(self.heap.types[type_id as usize].address),
                    bytes,
                })
                .collect();
            let mut types: Vec<_> = survivors
                .remove(&area)
                .unwrap_or_default()
                .into_iter()
                .collect();
            types.sort_by(|(a, av), (b, bv)| bv.cmp(av).then(a.cmp(b)));
            contributor.survivor_types = types
                .into_iter()
                .take(12)
                .map(|(type_id, bytes)| TypeBytes {
                    name: self.names.name(self.heap.types[type_id as usize].address),
                    bytes,
                })
                .collect();
        }
        Ok(sorted_contributors(areas.into_values(), shares))
    }

    pub fn fragmentation(&self) -> Result<Arc<FragmentationReport>> {
        self.memo("fragmentation", || {
            let mut report = FragmentationReport::default();
            let satori = self.heap.kind.gc == Gc::Satori;
            let mut gen2: BTreeMap<i32, Gen2Heap> = BTreeMap::new();
            if !satori {
                for region in self.heap.regions.iter().filter(|r| {
                    ["gen0", "gen1", "gen2"].contains(&r.generation_name.as_str()) && !r.immortal
                }) {
                    let entry = gen2.entry(region.gc_heap).or_insert_with(|| Gen2Heap {
                        heap: region.gc_heap,
                        ..Gen2Heap::default()
                    });
                    entry.size += region.used_bytes().saturating_sub(region.header_bytes());
                    entry.fragmentation += region.free_bytes + region.dead_bytes();
                }
            }
            let mut reasons: BTreeMap<String, Reason> = BTreeMap::new();
            let mut shares = vec![0.0; self.heap.count()];
            for (index, region) in self.heap.regions.iter().enumerate() {
                let waste = region.reclaimable_bytes();
                if waste == 0 {
                    continue;
                }
                report.total_waste += waste;
                let (name, explanation) = region_reason(region, satori, &gen2);
                let reason = reasons.entry(name.into()).or_insert_with(|| Reason {
                    name: name.into(),
                    explanation: explanation.into(),
                    ..Reason::default()
                });
                reason.regions += 1;
                reason.waste += waste;
                reason.live += region.live_bytes;
                reason.region_indices.push(index);
                if region.live_bytes == 0 {
                    continue;
                }
                for (object, share) in shares
                    .iter_mut()
                    .enumerate()
                    .take(region.end_object)
                    .skip(region.first_object)
                {
                    if self.graph.state(object) == "live" {
                        *share =
                            waste as f64 * self.heap.size(object) as f64 / region.live_bytes as f64;
                    }
                }
            }
            report.gen2_by_heap = gen2.into_values().collect();
            report.reasons = reasons.into_values().collect();
            for reason in &mut report.reasons {
                reason
                    .region_indices
                    .sort_by_key(|&i| std::cmp::Reverse(self.heap.regions[i].reclaimable_bytes()));
            }
            report
                .reasons
                .sort_by(|a, b| b.waste.cmp(&a.waste).then_with(|| a.name.cmp(&b.name)));
            let root = self.graph.root_node();
            let mut owners = vec![None; root + 1];
            let mut stack = vec![root];
            while let Some(parent) = stack.pop() {
                for &child in self.graph.dominator_rows.row(parent) {
                    let child = child as usize;
                    owners[child] = if parent == root {
                        Some(child)
                    } else {
                        owners[parent]
                    };
                    stack.push(child);
                }
            }
            let mut by_type = BTreeMap::new();
            let mut by_owner = BTreeMap::new();
            for (object, &share) in shares.iter().enumerate().filter(|(_, s)| **s > 0.0) {
                let type_id = self.heap.type_ids[object];
                by_type
                    .entry(type_id)
                    .or_insert_with(|| {
                        Contributor::new(format!("t{type_id}"), self.type_name(object))
                    })
                    .add(self, object, share);
                let name = match owners[object] {
                    None => "(not reached)".into(),
                    Some(owner) if owner >= self.heap.count() => self.node_name(owner)?,
                    Some(owner) => format!(
                        "object reachable from several roots: {}",
                        self.type_name(owner)
                    ),
                };
                by_owner
                    .entry(name.clone())
                    .or_insert_with(|| Contributor::new(name.clone(), name))
                    .add(self, object, share);
            }
            report.by_type = sorted_contributors(by_type.into_values(), &shares);
            report.by_owner = sorted_contributors(by_owner.into_values(), &shares);
            report.by_code = self.fragmentation_by_code(&shares, &owners)?;
            Ok(report)
        })
    }
}

fn sorted_contributors(
    items: impl Iterator<Item = Contributor>,
    shares: &[f64],
) -> Vec<Contributor> {
    let mut list: Vec<_> = items.collect();
    for item in &mut list {
        item.select_samples(shares);
    }
    list.sort_by(|a, b| b.waste.total_cmp(&a.waste).then_with(|| a.id.cmp(&b.id)));
    list
}

pub fn is_application_type(name: &str) -> bool {
    !["System.", "Microsoft.", "Internal.", "<"]
        .iter()
        .any(|p| name.starts_with(p))
        && name != "Free"
}

pub fn namespace(name: &str) -> &str {
    let plain = name.split(['<', '[', '+']).next().unwrap_or(name);
    plain
        .rsplit_once('.')
        .map(|(namespace, _)| namespace)
        .unwrap_or(plain)
}

fn region_reason(
    region: &Region,
    satori: bool,
    gen2: &BTreeMap<i32, Gen2Heap>,
) -> (&'static str, &'static str) {
    if !satori {
        if region.immortal {
            return NON_GC;
        }
        if region.queue == "waiting for decommit" {
            return DECOMMITTING;
        }
        match region.generation_name.as_str() {
            "free" => return FREE_REGION,
            "LOH" => return LARGE_OBJECT_HEAP,
            "POH" => return PINNED_OBJECT_HEAP,
            _ => {}
        }
        if region.generation == 2 {
            let vote = gen2.len() > 1 && gen2.values().any(Gen2Heap::above_threshold);
            return if vote
                || gen2
                    .get(&region.gc_heap)
                    .is_some_and(Gen2Heap::above_threshold)
            {
                GEN2_ABOVE
            } else {
                GEN2_BELOW
            };
        }
        return if region.generation == 1 { GEN1 } else { GEN0 };
    }
    if region.immortal {
        return IMMORTAL;
    }
    if region.owner_thread_tag != 0 {
        return THREAD_LOCAL;
    }
    if region.attachment_point != 0 {
        return ALLOCATING;
    }
    if !region.has_generation() {
        return if region.queue == "Allocator" {
            SATORI_FREE_REGION
        } else {
            NO_GENERATION
        };
    }
    if region.size() > GRANULE {
        return LARGE_REGION;
    }
    if region.has_pinned_objects {
        return PINNED;
    }
    let occupancy = if region.occupancy > 0 {
        region.occupancy
    } else {
        region.live_bytes
    };
    if region.generation == 2 {
        return if occupancy > GRANULE / 2 {
            GEN2_HALF_LIVE
        } else {
            GEN2_RELOCATION
        };
    }
    if region.queue == "RecyclerDemoted" {
        DEMOTED
    } else {
        REUSED
    }
}

const FREE_REGION: (&str, &str) = (
    "Free region kept committed",
    "A region on a heap's free list (basic, large or huge) or on the global list of huge free \
        regions. It keeps its committed memory for new regions. After each GC, \
        distribute_free_regions keeps the basic regions that the expected growth of gen0, gen1 and \
        gen2 needs. It moves the rest, and regions that stayed free too long (20 GCs for basic \
        regions, 5 or 2 gen2 GCs for large or huge ones), to the decommit list.",
);

const DECOMMITTING: (&str, &str) = (
    "Waiting for decommit",
    "A free region the GC decided to give back. With server GC, the GC decommits these in steps \
        of up to 16 MB every 100 ms (DECOMMIT_TIME_STEP_MILLISECONDS). With workstation GC, each GC \
        decommits up to 160 KB for each millisecond since the last decommit and puts the rest back \
        on the free lists.",
);

const LARGE_OBJECT_HEAP: (&str, &str) = (
    "Large object heap, not compacted",
    "Objects of 85,000 bytes and more. Every gen2 GC sweeps the LOH. Only a blocking gen2 GC \
        compacts it, and only when asked (GCSettings.LargeObjectHeapCompactionMode = CompactOnce, \
        GCLOHCompact), near the heap hard limit, with GCConserveMemory (5 by default with DATAS) \
        when the LOH is fragmented enough, or in an aggressive GC (GCCollectionMode.Aggressive). \
        Free space is reused through the LOH free list.",
);

const PINNED_OBJECT_HEAP: (&str, &str) = (
    "Pinned object heap",
    "Objects allocated pinned: GC.AllocateArray or GC.AllocateUninitializedArray with pinned: \
        true, and the runtime's object arrays that hold static reference fields. Never compacted; \
        free space is reused through its free list.",
);

const GEN2_ABOVE: (&str, &str) = (
    "Gen2, above the compaction threshold",
    "On this region's heap, free gaps and dead objects in gen0, gen1 and gen2 are at least 25% \
        of their size and 200,000 bytes, so a blocking gen2 GC compacts; with server GC, all heaps \
        then compact. Gen2 GCs that the allocation budget starts are background GCs by default: \
        they mark and sweep while threads run and never compact. A gen2 GC is blocking when induced \
        (GC.Collect without blocking: false), before an out-of-memory failure, in provisional mode, \
        near the heap hard limit, with GCConserveMemory (5 with DATAS) when gen2 and LOH free space \
        is high, or under high memory load (90% by default, GCHighMemPercent) when gen2 \
        fragmentation is high.",
);

const GEN2_BELOW: (&str, &str) = (
    "Gen2, below the compaction threshold",
    "On this region's heap, free gaps and dead objects in gen0, gen1 and gen2 are less than 25% \
        of their size or less than 200,000 bytes. A blocking gen2 GC can still compact: with server \
        GC when another heap is above the threshold, when a sweep would leave too little space for \
        new allocations, when the LOH is compacted, or under high memory load. Otherwise it sweeps. \
        Swept space goes to the gen2 free list and takes the objects that gen1 GCs promote.",
);

const GEN1: (&str, &str) = (
    "Gen1",
    "Ephemeral GCs are always blocking. A gen1 GC compacts when free and dead space in gen0 and \
        gen1 is at least 50% of their size and 80,000 bytes, when a sweep would leave too little \
        space for the gen0 budget, or at 95% memory load. Otherwise it sweeps. Survivors usually go \
        to gen2.",
);

const GEN0: (&str, &str) = (
    "Gen0 and its allocation space",
    "Committed space above the allocated objects is room for the gen0 allocation budget. Dead \
        objects go at the next gen0 GC, which promotes the survivors to gen1 and compacts or \
        sweeps. The server GC has a gen0 budget for each heap. With DATAS, the gen0 budget follows \
        the live size, and the heap count follows the share of time spent in GC.",
);

const NON_GC: (&str, &str) = (
    "NonGC heap",
    "Frozen object heap segments (string literals, RuntimeType objects); never collected or compacted.",
);

const IMMORTAL: (&str, &str) = (
    "Immortal",
    "Generation 3, Satori's equivalent of the NonGC heap (string literals, RuntimeType objects); \
        never collected or compacted.",
);

const THREAD_LOCAL: (&str, &str) = (
    "Thread-local gen0",
    "Escape tracking for one thread. When the region runs out of space, that thread collects it \
        (thread-local GC) and reuses the free space. This happens only if the thread allocated 512 \
        KB since the last such collection and at least 70% of the region becomes free. Otherwise \
        the thread detaches the region and it becomes gen1.",
);

const ALLOCATING: (&str, &str) = (
    "Being allocated into",
    "Attached to an allocation context: a thread's, or the allocator's shared region for small, \
        large or pinned objects. New objects fill its free space. These regions are gen1: shared, \
        large-object and pinned regions start in gen1, and a thread's region is gen1 when it was \
        reused for gen1, when the thread skipped escape tracking, or when more than 512 KB of it \
        escaped.",
);

const NO_GENERATION: (&str, &str) = (
    "Unassigned allocation space",
    "Unused allocation space in regions outside every generation and allocator queue. \
        These can contain allocations being initialized. Unparsed bytes are excluded; \
        the absence of a queue does not establish that a region is free or lost.",
);

const SATORI_FREE_REGION: (&str, &str) = (
    "Free regions in allocator queues",
    "Regions available for allocation. The trimmer can decommit or combine these regions. \
        With DOTNET_gcTHP on (the Linux default), it keeps the first 2 MB of each free range \
        committed. GC work chunks and finalization queue storage are accounted for separately.",
);

const LARGE_REGION: (&str, &str) = (
    "Region of its own",
    "A region larger than 2 MB holds one object that does not fit in a 2 MB region. It starts in \
        gen2 and is never relocated. A gen2 collection frees it when the object is dead.",
);

const PINNED: (&str, &str) = (
    "Pinned objects",
    "A region with pinned objects is not relocated.",
);

const GEN2_HALF_LIVE: (&str, &str) = (
    "Gen2, more than half live",
    "Satori relocates only regions at most half full. In a compacting collection, objects moved \
        from other regions can fill this free space. Otherwise it stays until enough objects in the \
        region die.",
);

const GEN2_RELOCATION: (&str, &str) = (
    "Gen2 relocation candidate",
    "At most half full: compacted when a gen2 collection finds the total reclaim worth it (about \
        1/6 of the condemned space). GC.Collect with compacting: true skips that check only on \
        Satori's main branch from #116 (October 2026); the 10.0-based branch ignores it.",
);

const DEMOTED: (&str, &str) = (
    "Demoted",
    "Gen2 regions moved to gen1 so their free space takes new allocations.",
);

const REUSED: (&str, &str) = (
    "Gen0/gen1, reused for allocation",
    "A gen1 region with more than 64 KB of free-list space goes to the reusable queue while the \
        reuse budget (gcReusableTarget) lasts, and new allocations fill it. Gen1 compacts only when \
        about half the condemned space would be reclaimed.",
);

fn application_holders(
    heap: &crate::heap::ManagedHeap,
    graph: &crate::graph::HeapGraph,
    application: &[bool],
) -> Vec<Option<usize>> {
    let mut holders = vec![None; heap.count()];
    let mut queue = Vec::new();
    for (object, holder) in holders.iter_mut().enumerate() {
        if graph.live[object] != 0 && application[heap.type_ids[object] as usize] {
            *holder = Some(object);
            queue.push(object);
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let object = queue[head];
        head += 1;
        for target in graph.edges(object) {
            let target = target as usize;
            if holders[target].is_none() && graph.live[target] != 0 {
                holders[target] = holders[object];
                queue.push(target);
            }
        }
    }
    holders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen2_requires_both_thresholds_and_server_heaps_share_the_vote() {
        assert!(
            !Gen2Heap {
                heap: 0,
                size: 700_000,
                fragmentation: 199_999
            }
            .above_threshold()
        );
        assert!(
            !Gen2Heap {
                heap: 0,
                size: 800_001,
                fragmentation: 200_000
            }
            .above_threshold()
        );
        assert!(
            Gen2Heap {
                heap: 0,
                size: 800_000,
                fragmentation: 200_000
            }
            .above_threshold()
        );
        let heaps = BTreeMap::from([
            (
                0,
                Gen2Heap {
                    heap: 0,
                    size: 800_000,
                    fragmentation: 200_000,
                },
            ),
            (
                1,
                Gen2Heap {
                    heap: 1,
                    size: 800_000,
                    fragmentation: 0,
                },
            ),
        ]);
        let region = Region {
            gc_heap: 1,
            generation: 2,
            ..Region::default()
        };
        assert_eq!(
            region_reason(&region, false, &heaps).0,
            "Gen2, above the compaction threshold"
        );
    }
    use crate::{cache::CacheChoice, graph::AnalysisOptions};
    use std::{path::Path, process::Command};

    #[test]
    fn dependent_handles_participate_in_application_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/generate-compatibility-fixtures.py");
        assert!(
            Command::new(if cfg!(windows) { "python" } else { "python3" })
                .arg(script)
                .arg(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let mut snapshot = HeapSnapshot::open(
            &directory.path().join("scalar-rooted.dmp"),
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::None,
            |_, _| {},
        )
        .unwrap();
        snapshot.graph.live[1] = 1;
        let mut application = vec![false; snapshot.heap.types.len()];
        application[snapshot.heap.type_ids[0] as usize] = true;
        assert_eq!(
            application_holders(&snapshot.heap, &snapshot.graph, &application),
            [Some(0), None, None]
        );
        snapshot.graph.saved.dependents.insert(0, vec![1]);
        assert_eq!(
            application_holders(&snapshot.heap, &snapshot.graph, &application),
            [Some(0), Some(0), None]
        );
    }
}
