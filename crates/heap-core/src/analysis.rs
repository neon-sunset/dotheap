use crate::{
    Result,
    cache::{CacheChoice, CacheGuard},
    column::ColumnStore,
    compressed::{atomic_write, identity},
    core_dump::{CoreDump, Layout},
    graph::{AnalysisOptions, GraphSaved, HeapGraph},
    heap::{GRANULE, Gc, HeapSaved, ManagedHeap, Region},
    native_memory::{NativeMemory, non_zero_bytes},
    progress::Progress,
    runtime::RuntimeInfo,
    smaps::{self, Residency, SmapsMapping},
    type_names::TypeNames,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

const CACHE_FORMAT: &str = "rust-8";

pub fn convert_dump(
    source: &Path,
    target: Option<&Path>,
    progress: impl FnMut(f64) -> bool,
) -> Result<Vec<String>> {
    let before = identity(source)?;
    let old_caches = [false, true].map(|native_memory_roots| {
        let options = AnalysisOptions {
            native_memory_roots,
        };
        (options, CacheChoice::Automatic.directory(source, options))
    });
    let companion = target
        .and_then(|target| {
            smaps::next_to_dump(source).map(|source| {
                let mut target = smaps::path_for_dump(target);
                if source.extension().is_some_and(|e| e == "gz") {
                    target.set_extension("gz");
                } else if source.extension().is_some_and(|e| e == "smaps") {
                    target.set_extension("");
                }
                (source, target)
            })
        })
        .filter(|(source, target)| source != target);
    let companion = if let Some((source, target)) = companion {
        if target.exists() {
            return Err(crate::Error::open(
                target.display(),
                "a memory map already exists at this path",
            ));
        }
        let mut output = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
        std::io::copy(&mut fs::File::open(source)?, &mut output)?;
        output.as_file().sync_all()?;
        Some((output, target))
    } else {
        None
    };
    crate::compressed::convert(source, target, progress)?;
    if let Some((output, target)) = companion {
        output
            .persist_noclobber(&target)
            .map_err(|e| crate::Error::open(target.display(), e.error))?;
    }
    let mut warnings = Vec::new();
    let destination = target.unwrap_or(source);
    for (options, from) in old_caches {
        if let Ok(Some(from)) = from
            && from.is_dir()
            && let Ok(Some(to)) = CacheChoice::Automatic.directory(destination, options)
            && let Err(error) =
                finish_converted_cache(&before, destination, &from, &to, target.is_none())
            && target.is_none()
        {
            warnings.push(format!(
                "The dump was converted, but its old compressed cache could not be removed: {error}"
            ));
        }
    }
    Ok(warnings)
}

fn finish_converted_cache(
    before: &str,
    target: &Path,
    from: &Path,
    to: &Path,
    replaced: bool,
) -> Result<()> {
    let Some(_source_guard) = CacheGuard::try_lock(from)? else {
        return Err(crate::Error::cache(format!("{} is in use", from.display())));
    };
    let _ = reuse_converted_cache(before, target, from, to);
    if replaced && fs::read_to_string(from.join("core.dump")).is_ok_and(|value| value == before) {
        for name in [
            "core.dump",
            "core.blocks",
            "core.index.col",
            "core.starts.col",
        ] {
            if let Err(error) = fs::remove_file(from.join(name))
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(crate::Error::open(from.join(name).display(), error));
            }
        }
    }
    Ok(())
}

fn reuse_converted_cache(before: &str, target: &Path, from: &Path, to: &Path) -> Result<()> {
    let mut manifest: Manifest = serde_json::from_slice(&fs::read(from.join("manifest.json"))?)?;
    if manifest.format != CACHE_FORMAT || manifest.dump_identity != before {
        return Ok(());
    }
    let _target_guard = CacheGuard::lock(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().is_some_and(|e| e == "col")
            && !entry.file_name().to_string_lossy().starts_with("core.")
        {
            fs::hard_link(entry.path(), to.join(entry.file_name()))?;
        }
    }
    manifest.dump_identity = identity(target)?;
    atomic_write(&to.join("manifest.json"), &serde_json::to_vec(&manifest)?)
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TypeStats {
    pub type_id: u32,
    pub name: String,
    pub count: usize,
    pub bytes: u64,
    pub live_count: usize,
    pub live_bytes: u64,
    pub pending_count: usize,
    pub pending_bytes: u64,
    pub gen2_bytes: u64,
    pub retained: u64,
}
impl TypeStats {
    pub fn dead_count(&self) -> usize {
        self.count - self.live_count - self.pending_count
    }
    pub fn dead_bytes(&self) -> u64 {
        self.bytes - self.live_bytes - self.pending_bytes
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RegionStats {
    pub name: String,
    pub regions: usize,
    pub span: u64,
    pub committed: u64,
    pub used: u64,
    pub objects: u64,
    pub live: u64,
    pub pending: u64,
    pub free: u64,
    pub unparsed: u64,
    pub gc_storage: u64,
    pub committed_unused: u64,
    pub headers: u64,
    pub satori_occupancy: Option<u64>,
    pub display_name: Option<String>,
}
impl RegionStats {
    pub fn new<'a>(name: impl Into<String>, regions: impl IntoIterator<Item = &'a Region>) -> Self {
        let mut s = Self {
            name: name.into(),
            satori_occupancy: Some(0),
            ..Self::default()
        };
        for r in regions {
            s.regions += 1;
            s.span += r.size();
            s.committed += r.committed_bytes();
            s.used += r.used_bytes();
            s.objects += r.object_bytes;
            s.live += r.live_bytes;
            s.pending += r.pending_bytes;
            s.free += r.free_bytes;
            s.unparsed += r.unparsed_bytes;
            s.gc_storage += r.gc_storage_bytes();
            s.committed_unused += r.committed_unused_bytes();
            s.headers += r.header_bytes().min(r.used_bytes());
            s.satori_occupancy = if r.has_generation() {
                s.satori_occupancy.map(|n| n + r.occupancy)
            } else {
                None
            };
        }
        if s.satori_occupancy == Some(0) {
            s.satori_occupancy = None;
        }
        s
    }
    pub fn dead(&self) -> u64 {
        self.objects - self.live - self.pending
    }
    pub fn live_of_used(&self) -> f64 {
        if self.used > 0 {
            self.live as f64 / self.used as f64
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OccupancyHistogram {
    pub generation: String,
    pub counts: [usize; 7],
    pub used_below_quarter: u64,
}
impl OccupancyHistogram {
    pub const BOUNDS: [f64; 7] = [0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 1.01];
    pub const LABELS: [&str; 7] = [
        "<5%", "5-10%", "10-25%", "25-50%", "50-75%", "75-90%", "90-100%",
    ];
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryPart {
    pub name: String,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    format: String,
    dump_identity: String,
    options: AnalysisOptions,
    runtime: Option<RuntimeInfo>,
    core: Layout,
    heap: HeapSaved,
    graph: GraphSaved,
    native: NativeMemory,
    type_stats: Vec<TypeStats>,
}

pub struct HeapSnapshot {
    pub dump: Arc<CoreDump>,
    pub runtime: Option<RuntimeInfo>,
    pub heap: ManagedHeap,
    pub graph: HeapGraph,
    pub names: Arc<TypeNames>,
    pub native: NativeMemory,
    pub smaps: Option<Vec<SmapsMapping>>,
    pub smaps_path: Option<PathBuf>,
    pub type_stats: Vec<TypeStats>,
    pub cache_directory: Option<PathBuf>,
    pub from_cache: bool,
    pub dll_folders: Vec<PathBuf>,
    residency: OnceLock<Option<Residency>>,
    memos: Mutex<HashMap<String, Arc<dyn Any + Send + Sync>>>,
    pub(crate) results_directory: Option<PathBuf>,
}
impl HeapSnapshot {
    pub fn open(
        path: &Path,
        dll_folders: Vec<PathBuf>,
        smaps_path: Option<PathBuf>,
        options: AnalysisOptions,
        cache: CacheChoice,
        progress: impl Fn(&str, f64) + Sync,
    ) -> Result<Self> {
        let reported = Mutex::new((String::new(), 0.0, std::time::Instant::now()));
        let update = |name: &str, fraction: f64| {
            let mut last = reported.lock().unwrap();
            let fraction = fraction.clamp(last.1, 1.0);
            if name != last.0
                || fraction == 1.0
                || fraction >= last.1 + 0.002 && last.2.elapsed().as_millis() >= 33
            {
                progress(name, fraction);
                *last = (name.into(), fraction, std::time::Instant::now());
            }
        };
        let progress = Progress::new(&update);
        progress.named("Opening the dump").report(0.0);
        let directory = cache.directory(path, options)?;
        let _guard = directory
            .as_ref()
            .map(|p| CacheGuard::lock(p))
            .transpose()?;
        let before = identity(path)?;
        let mut saved: Option<Manifest> = directory
            .as_ref()
            .filter(|_| cache != CacheChoice::Rebuild)
            .and_then(|dir| fs::read(dir.join("manifest.json")).ok())
            .and_then(|data| serde_json::from_slice::<Manifest>(&data).ok())
            .filter(|m| {
                m.format == CACHE_FORMAT && m.dump_identity == before && m.options == options
            });
        if saved.is_none()
            && let Some(dir) = &directory
        {
            let _ = fs::remove_file(dir.join("manifest.json"));
        }
        let reading = progress.portion(0.0, 0.5).named("Opening the dump");
        let discard_failed_cache = |error| {
            if let Some(directory) = &directory {
                for name in ["manifest.json", "core.dump"] {
                    let _ = fs::remove_file(directory.join(name));
                }
            }
            error
        };
        let opened = CoreDump::open_with_progress(
            path,
            directory.as_deref(),
            saved.as_ref().map(|m| m.core.clone()),
            reading,
        );
        let dump = match opened {
            Ok(dump) => dump,
            Err(_) if saved.is_some() => {
                saved = None;
                CoreDump::open_with_progress(path, directory.as_deref(), None, reading)
                    .map_err(discard_failed_cache)?
            }
            Err(error) => return Err(discard_failed_cache(error)),
        };
        let mut dump = Arc::new(dump);
        let had_saved = saved.is_some();
        let cached = saved.and_then(|m| {
            let [objects, references] = progress.portion(0.5, 0.7).phases([
                ("Opening cached objects", 1.0),
                ("Opening cached references", 2.0),
            ]);
            let store = ColumnStore::new(directory.clone()).ok()?;
            let mut heap =
                ManagedHeap::from_saved_with_progress(dump.clone(), store, m.heap, objects).ok()?;
            if heap.kind.gc == Gc::Satori {
                crate::satori_heap::assign_allocating_threads(
                    &dump,
                    m.runtime.as_ref(),
                    &mut heap.saved.regions,
                );
            }
            let graph = HeapGraph::from_saved_with_progress(&heap, m.graph, references).ok()?;
            if m.type_stats.len() != heap.types.len()
                || m.type_stats.iter().enumerate().any(|(i, t)| {
                    t.type_id as usize != i
                        || t.live_count
                            .checked_add(t.pending_count)
                            .is_none_or(|v| v > t.count)
                        || t.live_bytes
                            .checked_add(t.pending_bytes)
                            .is_none_or(|v| v > t.bytes)
                })
                || m.native.mappings.iter().any(|m| m.start > m.end)
                || m.native.malloc_heaps.iter().any(|h| h.start >= h.end)
            {
                return None;
            }
            Some((m.runtime, heap, graph, m.native, m.type_stats))
        });
        let from_cache = cached.is_some();
        let remaining = progress.portion(reported.lock().unwrap().1.max(0.5), 1.0);
        let [walking, references, native_progress, types, saving] = if from_cache {
            progress.portion(0.7, 1.0).phases([
                ("Walking the heap", 0.0),
                ("References and roots", 0.0),
                ("Native memory", 0.0),
                ("Resolving type names", 19.0),
                ("Checking the dump", 1.0),
            ])
        } else {
            remaining.phases([
                ("Walking the heap", 20.0),
                ("References and roots", 30.0),
                ("Native memory", 30.0),
                ("Types", 15.0),
                ("Saving the analysis", 5.0),
            ])
        };
        let (runtime, heap, graph, native, type_stats) = if let Some(cached) = cached {
            cached
        } else {
            if let Some(directory) = &directory {
                let _ = fs::remove_file(directory.join("manifest.json"));
            }
            if had_saved {
                dump = Arc::new(
                    CoreDump::open(path, directory.as_deref(), None)
                        .map_err(discard_failed_cache)?,
                );
            }
            let store = ColumnStore::new(directory.clone())?;
            walking.report(0.0);
            let runtime = RuntimeInfo::find(&dump);
            let mut heap =
                ManagedHeap::new_with_progress(dump.clone(), runtime.as_ref(), store, walking)?;
            let graph =
                HeapGraph::new_with_progress(&mut heap, runtime.as_ref(), options, references)?;
            let native = NativeMemory::new_with_progress(
                &dump,
                &heap,
                &graph.handle_segment_addresses,
                native_progress,
            );
            (runtime, heap, graph, native, Vec::new())
        };
        types.report(0.0);
        let names = Arc::new(TypeNames::new_with_progress(
            dump.clone(),
            heap.free_method_table,
            &dll_folders,
            heap.types.iter().map(|t| t.address),
            types.portion(0.0, 0.1),
        ));
        let smaps_path = smaps_path.or_else(|| smaps::next_to_dump(path));
        let smaps = smaps_path
            .as_ref()
            .map(|p| smaps::read(p))
            .transpose()?
            .or_else(|| dump.residency.as_ref().map(|r| r.mappings.clone()));
        let symbol_identity = crate::symbols::cache_identity_with_progress(
            &dump,
            types.portion(0.1, 0.6).named("Checking cached symbols"),
        );
        let smaps_identity = smaps_path
            .as_ref()
            .and_then(|p| crate::compressed::identity(p).ok())
            .unwrap_or_default();
        let result_identity = format!(
            "{CACHE_FORMAT}\n{before}\n{}\n{symbol_identity}\n{smaps_identity}",
            options.native_memory_roots
        );
        let results_directory = directory
            .as_ref()
            .filter(|_| dll_folders.is_empty())
            .map(|p| {
                p.join("rust-results")
                    .join(format!("{:x}", crate::cache::hash(&result_identity)))
            });
        let mut snapshot = Self {
            dump,
            runtime,
            heap,
            graph,
            names,
            native,
            smaps,
            smaps_path,
            type_stats,
            cache_directory: directory.clone(),
            from_cache,
            dll_folders,
            residency: OnceLock::new(),
            memos: Mutex::new(HashMap::new()),
            results_directory,
        };
        if !from_cache {
            snapshot.build_type_stats(types.portion(0.6, 1.0));
        } else {
            let count = snapshot.type_stats.len();
            for (i, t) in snapshot.type_stats.iter_mut().enumerate() {
                types.portion(0.6, 1.0).items(i, count);
                t.name = snapshot
                    .names
                    .name(snapshot.heap.types[t.type_id as usize].address);
            }
        }
        types.report(1.0);
        saving.report(0.0);
        snapshot.dump.check().map_err(discard_failed_cache)?;
        if identity(path)? != before {
            return Err(crate::Error::open(
                path.display(),
                "the dump changed while it was being read",
            ));
        }
        if let Some(dir) = directory {
            if !from_cache {
                let manifest = Manifest {
                    format: CACHE_FORMAT.into(),
                    dump_identity: before,
                    options,
                    runtime: snapshot.runtime.clone(),
                    core: snapshot.dump.layout.clone(),
                    heap: snapshot.heap.saved.clone(),
                    graph: snapshot.graph.saved.clone(),
                    native: snapshot.native.clone(),
                    type_stats: snapshot.type_stats.clone(),
                };
                atomic_write(&dir.join("manifest.json"), &serde_json::to_vec(&manifest)?)?;
            }
            atomic_write(
                &dir.join("dump.path"),
                path.canonicalize()?.to_string_lossy().as_bytes(),
            )?;
        }
        saving.report(1.0);
        progress.named("Done").report(1.0);
        Ok(snapshot)
    }
    pub fn memo<T: Send + Sync + 'static>(
        &self,
        key: impl Into<String>,
        compute: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        let key = key.into();
        let cell = {
            let mut memos = self.memos.lock().unwrap();
            memos
                .entry(key)
                .or_insert_with(|| Arc::new(OnceLock::<Result<Arc<T>>>::new()))
                .clone()
                .downcast::<OnceLock<Result<Arc<T>>>>()
                .expect("memo key has a single result type")
        };
        cell.get_or_init(|| {
            let result = compute()?;
            self.dump.check()?;
            Ok(Arc::new(result))
        })
        .clone()
    }
    pub fn saved<T: Serialize + DeserializeOwned + Send + Sync + 'static>(
        &self,
        key: &str,
        compute: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        self.memo(key, || {
            let path = self
                .results_directory
                .as_ref()
                .map(|dir| dir.join(format!("{key}.json")));
            if let Some(path) = &path
                && let Ok(bytes) = fs::read(path)
                && let Ok(value) = serde_json::from_slice(&bytes)
            {
                return Ok(value);
            }
            let value = compute()?;
            self.dump.check()?;
            if let Some(path) = path {
                crate::column::create_directory(path.parent().unwrap())?;
                atomic_write(&path, &serde_json::to_vec(&value)?)?;
            }
            Ok(value)
        })
    }
    fn build_type_stats(&mut self, progress: Progress<'_>) {
        let mut stats: Vec<_> = self
            .heap
            .types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                progress.portion(0.0, 0.6).items(i, self.heap.types.len());
                TypeStats {
                    type_id: i as u32,
                    name: self.names.name(t.address),
                    ..TypeStats::default()
                }
            })
            .collect();
        for i in 0..self.heap.count() {
            if i % 4096 == 0 {
                progress.portion(0.6, 0.8).items(i, self.heap.count());
            }
            let t = &mut stats[self.heap.type_ids[i] as usize];
            let size = self.heap.size(i);
            t.count += 1;
            t.bytes += size;
            if self.graph.pending[i] != 0 {
                t.pending_count += 1;
                t.pending_bytes += size;
            } else if self.graph.live[i] != 0 {
                t.live_count += 1;
                t.live_bytes += size;
            }
        }
        for r in self.heap.regions.iter().filter(|r| r.generation == 2) {
            for i in r.first_object..r.end_object {
                stats[self.heap.type_ids[i] as usize].gen2_bytes += self.heap.size(i);
            }
        }
        let mut on_path = vec![0usize; stats.len()];
        let mut stack = vec![(self.graph.root_node(), 0usize)];
        let mut visited = 0;
        while let Some((node, child)) = stack.last_mut() {
            if let Some(&w) = self.graph.dominator_rows.row(*node).get(*child) {
                *child += 1;
                let w = w as usize;
                if w < self.heap.count() {
                    let t = self.heap.type_ids[w] as usize;
                    if on_path[t] == 0 {
                        stats[t].retained += self.graph.retained[w];
                    }
                    on_path[t] += 1;
                }
                stack.push((w, 0));
            } else {
                let (v, _) = stack.pop().unwrap();
                visited += 1;
                if visited % 4096 == 0 {
                    progress
                        .portion(0.9, 1.0)
                        .items(visited, self.graph.idom.len());
                }
                if v < self.heap.count() {
                    on_path[self.heap.type_ids[v] as usize] -= 1;
                }
            }
        }
        self.type_stats = stats;
        progress.report(1.0);
    }
    pub fn totals(&self) -> RegionStats {
        RegionStats::new("all", &self.heap.regions)
    }
    pub fn fragmentation_share(&self) -> f64 {
        let t = RegionStats::new(
            "in a generation",
            self.heap.regions.iter().filter(|r| r.generation >= 0),
        );
        if t.objects + t.free > 0 {
            (t.dead() + t.free) as f64 / (t.objects + t.free) as f64
        } else {
            0.0
        }
    }
    fn region_groups(&self, key: impl Fn(&Region) -> String) -> Vec<RegionStats> {
        let mut groups = BTreeMap::<String, Vec<&Region>>::new();
        for r in &self.heap.regions {
            groups.entry(key(r)).or_default().push(r);
        }
        groups
            .into_iter()
            .map(|(key, regions)| RegionStats::new(key, regions))
            .collect()
    }
    pub fn by_generation(&self) -> Vec<RegionStats> {
        self.region_groups(|r| r.generation_name.clone())
    }
    pub fn by_queue(&self) -> Vec<RegionStats> {
        let mut groups = self.region_groups(|r| r.queue.clone());
        groups.sort_by(|a, b| b.used.cmp(&a.used).then(a.name.cmp(&b.name)));
        groups
    }
    pub fn size_class(r: &Region) -> String {
        if r.size() == GRANULE {
            "2 MB".into()
        } else if r.size() <= 32 * GRANULE {
            format!("{} MB", r.size() >> 20)
        } else {
            "> 64 MB".into()
        }
    }
    pub fn by_size_class(&self) -> Vec<RegionStats> {
        let mut groups = self.region_groups(Self::size_class);
        groups.sort_by_key(|s| {
            self.heap
                .regions
                .iter()
                .find(|r| Self::size_class(r) == s.name)
                .map(|r| r.size())
                .unwrap_or(0)
        });
        groups
    }
    pub fn occupancy(&self) -> Vec<OccupancyHistogram> {
        let mut groups = BTreeMap::new();
        for r in self.heap.regions.iter().filter(|r| r.size() == GRANULE) {
            let h = groups
                .entry(r.generation_name.clone())
                .or_insert_with(|| OccupancyHistogram {
                    generation: r.generation_name.clone(),
                    counts: [0; 7],
                    used_below_quarter: 0,
                });
            let fraction = r.live_bytes as f64 / r.size() as f64;
            h.counts[OccupancyHistogram::BOUNDS
                .iter()
                .position(|&v| fraction < v)
                .unwrap_or(6)] += 1;
            if fraction < 0.25 {
                h.used_below_quarter += r.used_bytes();
            }
        }
        groups.into_values().collect()
    }
    pub fn residency(&self) -> Option<&Residency> {
        self.residency
            .get_or_init(|| {
                let smaps = self.smaps.as_ref()?;
                if smaps.iter().any(|m| m.path.starts_with("vmmap: ")) {
                    return None;
                }
                Some(Residency::new(&self.native.mappings, smaps))
            })
            .as_ref()
    }
    pub fn resident_range(&self, from: u64, to: u64) -> Option<u64> {
        if !self.residency()?.agrees() {
            return None;
        }
        Some(smaps::resident(self.smaps.as_ref()?, from, to).round() as u64)
    }
    pub fn heap_residency(&self) -> Option<(u64, u64)> {
        let smaps = self.smaps.as_ref()?;
        if self.residency().is_some_and(|r| !r.agrees()) {
            return None;
        }
        let list = smaps
            .iter()
            .filter(|m| self.heap.owns_memory(m.start, m.end));
        Some(list.fold((0, 0), |(r, h), v| (r + v.rss, h + v.anon_huge)))
    }
    pub fn heap_zero_pages(&self) -> Result<u64> {
        Ok(*self.saved("heapZeroPages", || {
            let mut zero = 0;
            for r in &self.heap.regions {
                let Some(mut at) = crate::bytes::align_up(r.start, GRANULE) else {
                    continue;
                };
                let end = r.committed.min(r.end) & !(GRANULE - 1);
                while at < end {
                    if self
                        .dump
                        .segment_index(at)
                        .is_some_and(|i| at + GRANULE <= self.dump.segments[i].end())
                    {
                        zero += GRANULE - non_zero_bytes(&self.dump, at, at + GRANULE);
                    }
                    at += GRANULE;
                }
            }
            Ok(zero)
        })?)
    }
    pub fn process_memory(&self) -> Result<(Vec<MemoryPart>, bool)> {
        if let Some(residency) = self.residency().filter(|r| r.agrees()) {
            let mut bytes = BTreeMap::<String, u64>::new();
            for m in self
                .native
                .mappings
                .iter()
                .filter(|m| m.category != "dyld shared cache")
            {
                *bytes.entry(m.category.clone()).or_default() +=
                    residency.rss.get(&m.start).copied().unwrap_or(0);
            }
            if residency.not_in_dump > 0 {
                bytes.insert("mappings not in the dump".into(), residency.not_in_dump);
            }
            let mut parts: Vec<_> = bytes
                .into_iter()
                .map(|(name, bytes)| MemoryPart { name, bytes })
                .collect();
            parts.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
            return Ok((parts, true));
        }
        Ok((self.process_memory_estimate()?, false))
    }

    pub fn process_memory_estimate(&self) -> Result<Vec<MemoryPart>> {
        let categories = self.native.categories();
        let with_data = categories
            .iter()
            .find(|c| c.name == self.heap.label())
            .map(|c| c.non_zero_bytes)
            .unwrap_or(0);
        let mut parts = vec![MemoryPart {
            name: self.heap.label().into(),
            bytes: with_data + self.heap_zero_pages()?,
        }];
        parts.extend(
            categories
                .into_iter()
                .filter(|c| c.name != self.heap.label() && c.name != "dyld shared cache")
                .map(|c| MemoryPart {
                    name: c.name,
                    bytes: c.non_zero_bytes,
                }),
        );
        parts.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
        Ok(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn converted_dumps_reuse_object_and_reference_columns() {
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
        let source = directory.path().join("original.zst");
        let target = directory.path().join("converted.zst");
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        std::io::copy(
            &mut fs::File::open(directory.path().join("reference-rooted.dmp")).unwrap(),
            &mut encoder,
        )
        .unwrap();
        fs::write(&source, encoder.finish().unwrap()).unwrap();
        let from = directory.path().join("old-cache");
        let to = directory.path().join("new-cache");
        let open = |path: &Path, cache: &Path| {
            HeapSnapshot::open(
                path,
                Vec::new(),
                None,
                AnalysisOptions::default(),
                CacheChoice::Folder(cache.into()),
                |_, _| {},
            )
            .unwrap()
        };
        {
            let original = open(&source, &from);
            assert!(!original.from_cache);
        }
        crate::compressed::convert(&source, Some(&target), |_| true).unwrap();
        reuse_converted_cache(&identity(&source).unwrap(), &target, &from, &to).unwrap();
        assert!(!to.join("core.index.col").exists());
        let converted = open(&target, &to);
        let original = open(&source, &from);
        assert!(converted.from_cache);
        assert_eq!(&converted.heap.addresses[..], &original.heap.addresses[..]);
        assert_eq!(&converted.graph.idom[..], &original.graph.idom[..]);
        assert_eq!(converted.graph.references(0), original.graph.references(0));
        assert!(!to.join("core.blocks").exists());
    }

    #[test]
    fn replacement_reuses_analysis_and_removes_only_the_obsolete_compressed_cache() {
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
        let source = directory.path().join("original.zst");
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        std::io::copy(
            &mut fs::File::open(directory.path().join("reference-rooted.dmp")).unwrap(),
            &mut encoder,
        )
        .unwrap();
        fs::write(&source, encoder.finish().unwrap()).unwrap();
        let before = identity(&source).unwrap();
        let from = directory.path().join("old-cache");
        let to = directory.path().join("new-cache");
        let open = |cache: &Path| {
            HeapSnapshot::open(
                &source,
                Vec::new(),
                None,
                AnalysisOptions::default(),
                CacheChoice::Folder(cache.into()),
                |_, _| {},
            )
            .unwrap()
        };
        let original = open(&from);
        assert!(from.join("core.blocks").is_file());
        assert!(crate::compressed::convert(&source, None, |_| false).is_err());
        assert!(from.join("core.blocks").is_file());
        crate::compressed::convert(&source, None, |_| true).unwrap();
        finish_converted_cache(&before, &source, &from, &to, true).unwrap();
        for name in [
            "core.blocks",
            "core.dump",
            "core.index.col",
            "core.starts.col",
        ] {
            assert!(!from.join(name).exists(), "{name}");
        }
        assert!(from.join("addresses.col").is_file());
        let converted = open(&to);
        assert!(converted.from_cache);
        assert_eq!(&converted.heap.addresses[..], &original.heap.addresses[..]);
        assert_eq!(
            converted.dump.u64(converted.heap.addresses[0]),
            original.dump.u64(original.heap.addresses[0])
        );
        assert!(!to.join("core.blocks").exists());

        fs::write(from.join("core.dump"), "a different dump identity").unwrap();
        fs::write(from.join("core.blocks"), b"a different cache").unwrap();
        finish_converted_cache(&before, &source, &from, &to, true).unwrap();
        assert_eq!(
            fs::read(from.join("core.blocks")).unwrap(),
            b"a different cache"
        );
    }
}
