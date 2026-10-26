use crate::{models::*, types::Index};
use heap_core::{analysis::HeapSnapshot, cache::CacheChoice};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
};

#[derive(Debug)]
pub enum AnalysisError {
    Failed { message: String },
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for AnalysisError {}

impl From<heap_core::Error> for AnalysisError {
    fn from(error: heap_core::Error) -> Self {
        Self::Failed { message: error.0 }
    }
}

pub type Result<T> = std::result::Result<T, AnalysisError>;

pub trait Progress: Send + Sync {
    fn update(&self, phase: &str, fraction: f64);
    fn cancelled(&self) -> bool;
}

pub struct SnapshotInfo {
    pub object_count: Index,
    pub live_count: Index,
    pub live_bytes: u64,
    pub pending_count: Index,
    pub pending_bytes: u64,
    pub source_names: Vec<String>,
    pub statics_arrays: Vec<Index>,
    pub region_kinds: Arc<Vec<String>>,
    pub handle_counts: HashMap<String, Index>,
    pub handle_groups: Vec<HandleGroup>,
    pub native_categories: Vec<Category>,
    pub platform: Platform,
    pub runtime_version: Option<String>,
    pub runtime_path: Option<String>,
    pub from_cache: bool,
    pub cache_directory: Option<String>,
    pub smaps_path: Option<String>,
    pub residency_categories: Vec<ResidencyCategory>,
    pub versions: Versions,
    pub total: RegionStats,
    pub generations: Vec<RegionStats>,
    pub queues: Vec<RegionStats>,
    pub size_classes: Vec<RegionStats>,
    pub occupancy: Vec<OccupancyHistogram>,
    pub fragmentation_share: f64,
}

#[repr(C)]
pub struct ObjectInfo {
    pub address: u64,
    pub size: u64,
    pub type_id: u32,
    pub region: Index,
    pub live: bool,
    pub pending: bool,
    pub retained: u64,
    pub dominator: i32,
}

pub struct RootPath {
    pub source: Option<Index>,
    pub objects: Vec<Index>,
}

pub struct ProcessMemory {
    pub parts: Vec<MemoryPart>,
    pub resident: bool,
}

#[repr(C)]
pub struct HeapResidency {
    pub resident: u64,
    pub huge_pages: u64,
}

#[derive(Default)]
pub struct ResidencyCategory {
    pub name: String,
    pub vmas: Index,
    pub size: u64,
    pub rss: u64,
    pub huge: u64,
    pub swap: u64,
}

pub struct StateMachineField {
    pub value: FieldValue,
    pub retained_by: Vec<Index>,
}

pub struct Snapshot {
    pub(crate) core: Arc<HeapSnapshot>,
}

impl Snapshot {
    pub(crate) fn checked<T>(&self, value: heap_core::Result<T>) -> Result<T> {
        self.core.dump.check()?;
        Ok(value?)
    }

    pub(crate) fn validate_object(&self, index: Index) -> Result<()> {
        if index >= self.core.heap.count() {
            return Err(AnalysisError::Failed {
                message: "Object index is out of range".into(),
            });
        }
        Ok(())
    }
}

impl Snapshot {
    pub fn open(
        path: &str,
        dll_folders: Vec<String>,
        smaps_path: Option<String>,
        options: AnalysisOptions,
        rebuild: bool,
        cache_directory: Option<String>,
        progress: Option<Box<dyn Progress>>,
    ) -> Result<Arc<Self>> {
        let core = HeapSnapshot::open(
            Path::new(path),
            dll_folders.into_iter().map(Into::into).collect(),
            smaps_path.map(Into::into),
            options,
            if let Some(directory) = cache_directory {
                CacheChoice::Folder(directory.into())
            } else if rebuild {
                CacheChoice::Rebuild
            } else {
                CacheChoice::Automatic
            },
            |phase, fraction| {
                if let Some(progress) = &progress {
                    progress.update(phase, fraction);
                }
            },
        )?;
        // Initialize this lazy value before exposing it through a borrowed getter.
        core.residency();
        Ok(Arc::new(Self {
            core: Arc::new(core),
        }))
    }

    pub fn info(&self, progress: Option<Box<dyn Progress>>) -> Result<SnapshotInfo> {
        let s = &self.core;
        let last = std::sync::Mutex::new((String::new(), 0.0));
        let update = |phase: &str, fraction: f64| {
            let mut last = last.lock().unwrap();
            if let Some(progress) = &progress
                && (phase != last.0 || fraction >= last.1 + 0.005 || fraction == 1.0)
            {
                progress.update(phase, fraction);
                *last = (phase.into(), fraction);
            }
        };
        let has_threads = s.graph.sources.iter().any(|source| {
            matches!(
                source.kind,
                RootKind::Stack { .. }
                    | RootKind::Registers { .. }
                    | RootKind::ThreadStatics { .. }
            )
        });
        if has_threads {
            s.thread_stacks_with_progress(|fraction| {
                update("Reading thread stacks", fraction * 0.75)
            })?;
        }
        let source_names = (0..s.graph.sources.len())
            .map(|i| s.describe_source(i))
            .collect::<heap_core::Result<_>>()?;
        let static_start = if has_threads { 0.75 } else { 0.0 };
        let statics_arrays = s
            .statics_arrays_with_progress(|fraction| {
                update(
                    "Reading static fields",
                    static_start + fraction * (0.95 - static_start),
                )
            })?
            .iter()
            .copied()
            .collect();
        update("Preparing the view", 0.95);
        let mut categories = BTreeMap::<String, ResidencyCategory>::new();
        for mapping in s.smaps.iter().flatten() {
            let name = mapping.category(&s.heap);
            let entry = categories
                .entry(name.clone())
                .or_insert_with(|| ResidencyCategory {
                    name,
                    ..Default::default()
                });
            entry.vmas += 1;
            entry.size += mapping.end - mapping.start;
            entry.rss += mapping.rss;
            entry.huge += mapping.anon_huge;
            entry.swap += mapping.swap;
        }
        let mut residency_categories: Vec<_> = categories.into_values().collect();
        residency_categories.sort_by(|a, b| b.rss.cmp(&a.rss).then(a.name.cmp(&b.name)));
        let info = self.checked(Ok(SnapshotInfo {
            object_count: s.heap.count(),
            live_count: s.graph.live_count,
            live_bytes: s.graph.live_bytes,
            pending_count: s.graph.pending_count,
            pending_bytes: s.graph.pending_bytes,
            source_names,
            statics_arrays,
            region_kinds: s.satori_region_kinds()?,
            handle_counts: s
                .graph
                .handle_counts
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            handle_groups: s.handle_groups(),
            native_categories: s.native.categories(),
            platform: s.dump.platform,
            runtime_version: s.runtime.as_ref().and_then(|r| r.version.clone()),
            runtime_path: s.runtime.as_ref().map(|r| r.path.clone()),
            from_cache: s.from_cache,
            cache_directory: s
                .cache_directory
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            smaps_path: s
                .smaps_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            residency_categories,
            versions: s.versions(),
            total: s.totals(),
            generations: s.by_generation(),
            queues: s.by_queue(),
            size_classes: s.by_size_class(),
            occupancy: s.occupancy(),
            fragmentation_share: s.fragmentation_share(),
        }))?;
        update("Preparing the view", 1.0);
        Ok(info)
    }

    pub fn read_error(&self) -> Option<String> {
        self.core.dump.check().err().map(|e| e.0)
    }

    pub fn object_info(&self, index: Index) -> Option<ObjectInfo> {
        let s = &self.core;
        (index < s.heap.count()).then(|| ObjectInfo {
            address: s.heap.addresses[index],
            size: s.heap.size(index),
            type_id: s.heap.type_ids[index],
            region: s.heap.region_of(index),
            live: s.graph.live[index] != 0,
            pending: s.graph.pending[index] != 0,
            retained: s.graph.retained[index],
            dominator: s.graph.idom[index],
        })
    }

    pub fn region_objects(&self, region: Index) -> Option<Vec<ObjectInfo>> {
        let region = self.core.heap.regions.get(region)?;
        Some(
            (region.first_object..region.end_object)
                .map(|i| self.object_info(i).unwrap())
                .collect(),
        )
    }

    pub fn object_containing(&self, address: u64) -> Option<Index> {
        self.core.heap.object_containing(address)
    }

    pub fn region_containing(&self, address: u64) -> Option<Index> {
        self.core.heap.region_containing(address)
    }

    pub fn type_name(&self, index: Index) -> Result<String> {
        self.validate_object(index)?;
        self.checked(Ok(self.core.type_name(index)))
    }

    pub fn bytes(&self, address: u64, count: Index) -> Result<Option<Vec<u8>>> {
        if count > 16 << 20 {
            return Err(AnalysisError::Failed {
                message: "Read at most 16 MB at a time".into(),
            });
        }
        self.checked(Ok(self
            .core
            .dump
            .bytes(address, count)
            .map(|b| b.into_owned())))
    }

    pub fn instances(&self, type_id: u32, limit: Index) -> Vec<Index> {
        self.core.instances(type_id, limit)
    }

    pub fn referrer_types(&self, type_id: u32) -> Result<Vec<TypeGroup>> {
        self.checked(self.core.referrer_types(type_id))
    }

    pub fn type_id(&self, name: &str) -> Option<u32> {
        self.core
            .type_stats
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.type_id)
    }

    pub fn dominator_types(&self, type_id: u32) -> Result<Vec<TypeGroup>> {
        self.checked(self.core.dominator_types(type_id))
    }

    pub fn handle_groups(&self) -> Result<Vec<HandleGroup>> {
        self.checked(Ok(self.core.handle_groups()))
    }

    pub fn graph_retained(&self, node: Index) -> Option<u64> {
        self.core.graph.retained.get(node).copied()
    }

    pub fn graph_dominator(&self, node: Index) -> Option<i32> {
        self.core.graph.idom.get(node).copied()
    }

    pub fn references(&self, object: Index) -> Result<Vec<i32>> {
        self.validate_object(object)?;
        Ok(self.core.graph.references(object))
    }

    pub fn referrers(&self, object: Index) -> Result<Vec<i32>> {
        self.validate_object(object)?;
        Ok(self.core.graph.referrer_rows.row(object).to_vec())
    }

    pub fn object_sources(&self, object: Index) -> Result<Vec<Index>> {
        self.validate_object(object)?;
        Ok(self
            .core
            .graph
            .sources_of
            .row(object)
            .iter()
            .map(|&i| i as usize)
            .collect())
    }

    pub fn root_objects(&self, source: Index) -> Option<Vec<i32>> {
        (source < self.core.graph.sources.len())
            .then(|| self.core.graph.source_rows.row(source).to_vec())
    }

    pub fn path_to_root(&self, object: Index) -> Option<RootPath> {
        if object >= self.core.heap.count() {
            return None;
        }
        let (source, objects) = self.core.graph.path_to_root(object);
        Some(RootPath { source, objects })
    }

    pub fn node_name(&self, node: Index) -> Result<String> {
        self.validate_node(node)?;
        self.checked(self.core.node_name(node))
    }

    pub fn describe_source(&self, source: Index) -> Result<String> {
        if source >= self.core.graph.sources.len() {
            return Err(AnalysisError::Failed {
                message: "Root source is out of range".into(),
            });
        }
        self.checked(self.core.describe_source(source))
    }

    pub fn dominated_children(&self, node: Index) -> Option<Vec<Index>> {
        (node <= self.core.graph.root_node()).then(|| self.core.graph.dominated_children(node))
    }

    pub fn dominated_groups(&self, node: Index) -> Result<Arc<Vec<DominatorGroup>>> {
        self.validate_node(node)?;
        self.checked(self.core.dominated_groups(node))
    }

    pub fn granule_map(&self) -> Vec<Vec<Granule>> {
        self.core.granule_map()
    }

    pub fn region_strip(&self, region: Index, buckets: Index) -> Option<RegionStrip> {
        if region >= self.core.heap.regions.len() || buckets == 0 || buckets > 65536 {
            return None;
        }
        Some(self.core.region_strip(region, buckets))
    }

    pub fn region_types(&self, region: Index) -> Result<Vec<TypeGroup>> {
        if region >= self.core.heap.regions.len() {
            return Err(AnalysisError::Failed {
                message: "Region index is out of range".into(),
            });
        }
        self.checked(Ok(self.core.region_types(region)))
    }

    pub fn satori_region_kinds(&self) -> Result<Arc<Vec<String>>> {
        self.checked(self.core.satori_region_kinds())
    }

    pub fn statics_arrays(&self) -> Result<Vec<Index>> {
        self.checked(
            self.core
                .statics_arrays()
                .map(|v| v.iter().copied().collect()),
        )
    }

    pub fn process_memory(&self) -> Result<ProcessMemory> {
        self.checked(
            self.core
                .process_memory()
                .map(|(parts, resident)| ProcessMemory { parts, resident }),
        )
    }

    pub fn heap_zero_pages(&self) -> Result<u64> {
        self.checked(self.core.heap_zero_pages())
    }

    pub fn memory_estimate(&self) -> Result<Vec<MemoryPart>> {
        self.checked(self.core.process_memory_estimate())
    }

    pub fn blocked_threads_check(&self) -> Result<Arc<CheckReport>> {
        self.checked(self.core.blocked_threads_check())
    }

    pub fn lock_check(&self) -> Result<Arc<CheckReport>> {
        self.checked(self.core.lock_check())
    }

    pub fn resident_range(&self, start: u64, end: u64) -> Option<u64> {
        self.core.resident_range(start, end)
    }

    pub fn residency(&self) -> Option<&Residency> {
        self.core.residency()
    }

    pub fn heap_residency(&self) -> Option<HeapResidency> {
        self.core
            .heap_residency()
            .map(|(resident, huge_pages)| HeapResidency {
                resident,
                huge_pages,
            })
    }

    pub fn malloc_parameters(&self) -> Result<Arc<Option<MallocParameters>>> {
        self.checked(self.core.malloc_parameters())
    }

    pub fn verification_check(&self) -> Result<Arc<CheckReport>> {
        self.checked(self.core.verification_check())
    }

    pub fn heap_verification(&self) -> Result<Arc<HeapVerification>> {
        self.checked(self.core.heap_verification())
    }

    pub fn all_checks(&self) -> Result<Vec<Arc<CheckReport>>> {
        self.checked(self.core.all_checks())
    }

    pub fn summary_findings(&self) -> Result<Arc<Vec<SummaryFinding>>> {
        self.checked(self.core.summary_findings())
    }

    pub fn task_graphs(&self) -> Result<Arc<AsyncAnalysis>> {
        self.checked(self.core.async_analysis())
    }

    pub fn thread_states(&self) -> Result<Arc<Vec<ThreadState>>> {
        self.checked(self.core.thread_states())
    }

    pub fn thread_groups(&self) -> Result<Vec<ThreadGroup>> {
        self.checked(self.core.thread_groups())
    }

    pub fn merged_stacks(&self) -> Result<Arc<Vec<StackNode>>> {
        self.checked(self.core.merged_stacks())
    }

    pub fn queued_work_items(&self) -> Result<Arc<Vec<QueuedWorkItem>>> {
        self.checked(self.core.queued_work_items())
    }

    pub fn timers(&self) -> Result<Arc<Timers>> {
        self.checked(self.core.timers())
    }

    pub fn timer_places(&self, timers: Vec<&TimerInfo>) -> Result<Vec<String>> {
        for timer in &timers {
            self.validate_object(timer.object)?;
        }
        self.checked(self.core.timer_places(&timers))
    }

    pub fn network(&self) -> Result<Arc<Network>> {
        self.checked(self.core.network())
    }

    pub fn files(&self) -> Result<Arc<Files>> {
        self.checked(self.core.files())
    }

    pub fn fragmentation(&self) -> Result<Arc<FragmentationReport>> {
        self.checked(self.core.fragmentation())
    }

    pub fn duplicate_strings(&self, limit: Index) -> Result<Vec<DuplicateString>> {
        self.checked(Ok(self.core.duplicate_strings(limit)))
    }

    pub fn collection_capacity(&self) -> Result<Vec<CollectionCapacity>> {
        self.checked(Ok(self.core.collection_capacity()))
    }

    pub fn mostly_zero_arrays(&self) -> Result<Vec<ZeroArrays>> {
        self.checked(Ok(self.core.mostly_zero_arrays()))
    }

    pub fn boxed_values(&self) -> Result<Vec<TypeCount>> {
        self.checked(Ok(self.core.boxed_values()))
    }

    pub fn finalizable_objects(&self) -> Result<Vec<Finalizable>> {
        self.checked(Ok(self.core.finalizable_objects()))
    }

    pub fn garbage_by_type(&self) -> Result<Vec<GarbageGroup>> {
        self.checked(self.core.garbage_by_type())
    }

    pub fn search(&self, query: &str, limit_per_kind: Index) -> Result<Vec<SearchResult>> {
        self.checked(self.core.search(query, limit_per_kind))
    }

    pub fn chunk_groups(&self) -> Result<Arc<Vec<ChunkGroup>>> {
        self.checked(self.core.chunk_groups())
    }

    pub fn chunk_group(&self, chunk: Index) -> Result<Option<String>> {
        let Ok(chunk) = i32::try_from(chunk) else {
            return Ok(None);
        };
        Ok(self
            .chunk_groups()?
            .iter()
            .find(|group| group.chunks.binary_search(&chunk).is_ok())
            .map(|group| group.id.clone()))
    }

    pub fn ownership_name(&self, node: Index) -> Result<String> {
        let graph = self.core.ownership()?;
        if node >= graph.count {
            return Err(AnalysisError::Failed {
                message: "Ownership node is out of range".into(),
            });
        }
        self.checked(self.core.ownership_name(node))
    }

    pub fn ownership_groups(&self, node: Index) -> Result<Arc<Vec<OwnershipGroup>>> {
        let graph = self.core.ownership()?;
        if node >= graph.count {
            return Err(AnalysisError::Failed {
                message: "Ownership node is out of range".into(),
            });
        }
        self.checked(self.core.ownership_groups(node))
    }

    pub fn download_symbols(
        &self,
        automatic: bool,
        progress: Box<dyn Progress>,
    ) -> Vec<SymbolDownload> {
        self.core.download_runtime_symbols(
            heap_core::symbol_server::DEFAULT_SERVER,
            automatic,
            || progress.cancelled(),
            |phase, fraction| progress.update(phase, fraction),
        )
    }
}

impl Snapshot {
    fn validate_node(&self, node: Index) -> Result<()> {
        if node > self.core.graph.root_node() {
            return Err(AnalysisError::Failed {
                message: "Graph node is out of range".into(),
            });
        }
        Ok(())
    }
}

impl Snapshot {
    pub fn kind(&self) -> &HeapKind {
        &self.core.heap.kind
    }

    pub fn pages(&self) -> &[Page] {
        &self.core.heap.pages
    }

    pub fn regions(&self) -> &[Region] {
        &self.core.heap.regions
    }

    pub fn types(&self) -> &[MethodTableInfo] {
        &self.core.heap.types
    }

    pub fn type_stats(&self) -> &[TypeStats] {
        &self.core.type_stats
    }

    pub fn type_retained(&self) -> Result<Arc<Vec<u64>>> {
        self.checked(self.core.type_retained())
    }
    pub fn ownership_instances(&self, type_id: u32, limit: Index) -> Result<Vec<Index>> {
        self.checked(self.core.ownership_instances(type_id, limit))
    }
    pub fn ownership_dominator_types(&self, type_id: u32) -> Result<Vec<OwnershipTypeGroup>> {
        self.checked(self.core.ownership_dominator_types(type_id))
    }

    pub fn sources(&self) -> &[RootSource] {
        &self.core.graph.sources
    }

    pub fn handles(&self) -> &[GcHandle] {
        &self.core.graph.handles
    }

    pub fn native(&self) -> &NativeMemory {
        &self.core.native
    }

    pub fn threads(&self) -> &[Thread] {
        &self.core.dump.threads
    }

    pub fn addresses(&self) -> &[u64] {
        &self.core.heap.addresses
    }

    pub fn sizes(&self) -> &[u32] {
        &self.core.heap.sizes32
    }

    pub fn type_ids(&self) -> &[u32] {
        &self.core.heap.type_ids
    }

    pub fn live_flags(&self) -> &[u8] {
        &self.core.graph.live
    }

    pub fn pending_flags(&self) -> &[u8] {
        &self.core.graph.pending
    }

    pub fn retained(&self) -> &[u64] {
        &self.core.graph.retained
    }

    pub fn dominators(&self) -> &[i32] {
        &self.core.graph.idom
    }

    pub fn source_starts(&self) -> &[u64] {
        &self.core.graph.source_rows.start
    }

    pub fn source_items(&self) -> &[i32] {
        &self.core.graph.source_rows.items
    }

    pub fn dominator_starts(&self) -> &[u64] {
        &self.core.graph.dominator_rows.start
    }

    pub fn dominator_items(&self) -> &[i32] {
        &self.core.graph.dominator_rows.items
    }

    pub fn large_sizes(&self) -> &BTreeMap<usize, u64> {
        &self.core.heap.large_sizes
    }
}

impl Snapshot {
    pub fn smaps(&self) -> &Option<Vec<SmapsMapping>> {
        &self.core.smaps
    }
}
