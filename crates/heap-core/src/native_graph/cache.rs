use super::{Chunk, ChunkState, NativeGraphData, Reach};
use crate::{
    Error, Result, analysis::HeapSnapshot, cache::CacheGuard, column::ColumnStore,
    compressed::atomic_write,
};
use bytemuck::Pod;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs,
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata<'a> {
    format: u32,
    root_chunks: Cow<'a, BTreeSet<i32>>,
    root_source: Cow<'a, BTreeMap<i32, String>>,
    root_stack_slot: Cow<'a, BTreeMap<i32, (u64, i32)>>,
    texts: Cow<'a, BTreeMap<i32, String>>,
    tcache_chunks: usize,
    fastbin_chunks: usize,
}

impl NativeGraphData {
    pub(super) fn open(snapshot: &HeapSnapshot) -> Result<Self> {
        let directory = snapshot
            .results_directory
            .as_ref()
            .map(|p| p.join("native-graph"));
        let _guard = directory
            .as_ref()
            .map(|p| CacheGuard::lock(p))
            .transpose()?;
        let store = ColumnStore::new(directory.clone())?;
        if let Ok(data) = Self::load(&store)
            && data.validate(snapshot.heap.count()).is_ok()
        {
            return Ok(data);
        }
        if let Some(dir) = &directory {
            let _ = fs::remove_file(dir.join("manifest.json"));
        }
        let data = Self::new(snapshot)?;
        data.validate(snapshot.heap.count())?;
        snapshot.dump.check()?;
        if directory.is_some() {
            data.save(&store)?;
        }
        Ok(data)
    }

    fn load(store: &ColumnStore) -> Result<Self> {
        let directory = store
            .directory
            .as_ref()
            .ok_or_else(|| Error::cache("no cache folder"))?;
        let bytes = fs::read(directory.join("manifest.json"))?;
        let metadata: Metadata<'_> = serde_json::from_slice(&bytes)?;
        if metadata.format != 1 {
            return Err(Error::cache("unsupported native graph cache format"));
        }
        let states = [
            ChunkState::InUse,
            ChunkState::Free,
            ChunkState::Cached,
            ChunkState::Mmapped,
            ChunkState::Mapping,
        ];
        let chunks = store
            .open::<[u64; 3]>("chunks")?
            .iter()
            .map(|&[start, size, state]| {
                let state = usize::try_from(state)
                    .ok()
                    .and_then(|i| states.get(i))
                    .copied()
                    .ok_or_else(|| Error::cache("invalid native chunk state"))?;
                Ok(Chunk { start, size, state })
            })
            .collect::<Result<_>>()?;
        let reach = store
            .open::<u8>("reach")?
            .iter()
            .map(|&r| {
                Reach::ALL
                    .get(r as usize)
                    .copied()
                    .ok_or_else(|| Error::cache("invalid native reachability"))
            })
            .collect::<Result<_>>()?;
        let edge_exact = store
            .open::<u8>("edgeExact")?
            .iter()
            .map(|&value| match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(Error::cache("invalid native pointer flag")),
            })
            .collect::<Result<_>>()?;
        let retained_keys = store.open::<usize>("nativeRetained.keys")?;
        let retained_values = store.open::<u64>("nativeRetained.values")?;
        if retained_keys.len() != retained_values.len()
            || retained_keys.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(Error::cache("invalid native retained sizes"));
        }
        Ok(Self {
            chunks,
            reach,
            edge_start: store.open::<u64>("edgeStart")?.to_vec(),
            edge_targets: store.open::<i32>("edgeTargets")?.to_vec(),
            edge_exact,
            referrer_start: store.open::<u64>("referrerStart")?.to_vec(),
            referrer_items: store.open::<i32>("referrerItems")?.to_vec(),
            owned_chunks: load_map(store, "ownedChunks")?,
            chunk_owners: load_map(store, "chunkOwners")?,
            managed_references: load_map(store, "managedReferences")?,
            handle_references: load_map(store, "handleReferences")?,
            native_retained: retained_keys
                .iter()
                .copied()
                .zip(retained_values.iter().copied())
                .collect(),
            chunk_dominator: store.open::<i32>("chunkDominator")?.to_vec(),
            first_words: store.open::<u64>("firstWords")?.to_vec(),
            root_chunks: metadata.root_chunks.into_owned(),
            root_source: metadata.root_source.into_owned(),
            root_stack_slot: metadata.root_stack_slot.into_owned(),
            texts: metadata.texts.into_owned(),
            tcache_chunks: metadata.tcache_chunks,
            fastbin_chunks: metadata.fastbin_chunks,
        })
    }

    fn save(&self, store: &ColumnStore) -> Result<()> {
        save_column(
            store,
            "chunks",
            &self
                .chunks
                .iter()
                .map(|c| [c.start, c.size, c.state as u64])
                .collect::<Vec<_>>(),
        )?;
        save_column(
            store,
            "reach",
            &self.reach.iter().map(|&r| r as u8).collect::<Vec<_>>(),
        )?;
        save_column(
            store,
            "edgeExact",
            &self
                .edge_exact
                .iter()
                .map(|&v| u8::from(v))
                .collect::<Vec<_>>(),
        )?;
        save_column(store, "edgeStart", &self.edge_start)?;
        save_column(store, "edgeTargets", &self.edge_targets)?;
        save_column(store, "referrerStart", &self.referrer_start)?;
        save_column(store, "referrerItems", &self.referrer_items)?;
        save_column(store, "chunkDominator", &self.chunk_dominator)?;
        save_column(store, "firstWords", &self.first_words)?;
        save_map(store, "ownedChunks", &self.owned_chunks)?;
        save_map(store, "chunkOwners", &self.chunk_owners)?;
        save_map(store, "managedReferences", &self.managed_references)?;
        save_map(store, "handleReferences", &self.handle_references)?;
        save_column(
            store,
            "nativeRetained.keys",
            &self.native_retained.keys().copied().collect::<Vec<_>>(),
        )?;
        save_column(
            store,
            "nativeRetained.values",
            &self.native_retained.values().copied().collect::<Vec<_>>(),
        )?;
        let metadata = Metadata {
            format: 1,
            root_chunks: Cow::Borrowed(&self.root_chunks),
            root_source: Cow::Borrowed(&self.root_source),
            root_stack_slot: Cow::Borrowed(&self.root_stack_slot),
            texts: Cow::Borrowed(&self.texts),
            tcache_chunks: self.tcache_chunks,
            fastbin_chunks: self.fastbin_chunks,
        };
        let directory = store
            .directory
            .as_ref()
            .ok_or_else(|| Error::cache("no cache folder"))?;
        atomic_write(
            &directory.join("manifest.json"),
            &serde_json::to_vec(&metadata)?,
        )
    }
}

fn save_column<T: Pod>(store: &ColumnStore, name: &str, values: &[T]) -> Result<()> {
    store.copy(values, Some(name))?.flush()
}

fn save_map<K: Ord + Pod, V: Pod>(
    store: &ColumnStore,
    name: &str,
    map: &BTreeMap<K, Vec<V>>,
) -> Result<()> {
    let keys: Vec<_> = map.keys().copied().collect();
    let mut starts = Vec::with_capacity(map.len() + 1);
    starts.push(0u64);
    for values in map.values() {
        starts.push(starts.last().unwrap() + values.len() as u64);
    }
    let mut items = store.make::<V>(
        *starts.last().unwrap() as usize,
        Some(&format!("{name}.items")),
    )?;
    for (values, offsets) in map.values().zip(starts.windows(2)) {
        items[offsets[0] as usize..offsets[1] as usize].copy_from_slice(values);
    }
    items.flush()?;
    save_column(store, &format!("{name}.keys"), &keys)?;
    save_column(store, &format!("{name}.start"), &starts)
}

fn load_map<K: Ord + Pod, V: Pod>(store: &ColumnStore, name: &str) -> Result<BTreeMap<K, Vec<V>>> {
    let keys = store.open::<K>(&format!("{name}.keys"))?;
    let starts = store.open::<u64>(&format!("{name}.start"))?;
    let items = store.open::<V>(&format!("{name}.items"))?;
    if starts.len() != keys.len() + 1
        || starts.first() != Some(&0)
        || starts.last() != Some(&(items.len() as u64))
        || starts.windows(2).any(|w| w[0] > w[1])
        || keys.windows(2).any(|w| w[0] >= w[1])
    {
        return Err(Error::cache("invalid native graph map"));
    }
    Ok(keys
        .iter()
        .zip(starts.windows(2))
        .map(|(&key, offsets)| {
            (
                key,
                items[offsets[0] as usize..offsets[1] as usize].to_vec(),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cache_preserves_graphs_and_rejects_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let store = ColumnStore::new(Some(directory.path().into())).unwrap();
        let states = [
            ChunkState::InUse,
            ChunkState::Free,
            ChunkState::Cached,
            ChunkState::Mmapped,
            ChunkState::Mapping,
        ];
        let graph = NativeGraphData {
            chunks: states
                .into_iter()
                .enumerate()
                .map(|(i, state)| Chunk {
                    start: 0x1000 + i as u64 * 64,
                    size: 32,
                    state,
                })
                .collect(),
            reach: Reach::ALL.to_vec(),
            edge_start: vec![0, 1, 1, 2, 2, 2],
            edge_targets: vec![3, 4],
            edge_exact: vec![true, false],
            referrer_start: vec![0, 0, 0, 0, 1, 2],
            referrer_items: vec![0, 2],
            owned_chunks: BTreeMap::from([(0, vec![0, 3]), (1, vec![4])]),
            chunk_owners: BTreeMap::from([(0, vec![0]), (3, vec![0]), (4, vec![1])]),
            root_chunks: BTreeSet::from([0, 2]),
            root_source: BTreeMap::from([(0, "library data".into()), (2, "thread stack".into())]),
            root_stack_slot: BTreeMap::from([(2, (0xffff_ffff_f000, -7))]),
            managed_references: BTreeMap::from([(3, vec![1, 2])]),
            handle_references: BTreeMap::from([(4, vec![0x8000_0000_0000])]),
            native_retained: BTreeMap::from([(0, 64), (1, 32)]),
            chunk_dominator: vec![-2, -1, -1, 0, 2],
            first_words: vec![0, 1, u64::MAX, 3, 4],
            texts: BTreeMap::from([(0, "hello".into()), (3, "native data".into())]),
            tcache_chunks: 1,
            fastbin_chunks: 0,
        };
        graph.validate(3).unwrap();
        graph.save(&store).unwrap();
        let loaded = NativeGraphData::load(&store).unwrap();
        loaded.validate(3).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&graph).unwrap()
        );
        assert_eq!(loaded.chunk(0x1000 - 1), None);
        assert_eq!(loaded.chunk(0x1000 + 16), Some(0));
        assert_eq!(loaded.chunk(0x1000 + 32), Some(0));
        assert_eq!(loaded.chunk(0x1000 + 40), None);
        assert_eq!(loaded.chunk(u64::MAX), None);
        for (name, bytes) in [
            (
                "chunks",
                bytemuck::cast_slice(&[[0x1000u64, 32, 99]]).to_vec(),
            ),
            ("reach", vec![5; 5]),
            ("edgeExact", vec![1, 2]),
            (
                "edgeStart",
                bytemuck::cast_slice(&[0u64, 9, 0, 0, 0, 2]).to_vec(),
            ),
            ("edgeTargets", bytemuck::cast_slice(&[3i32, 5]).to_vec()),
            (
                "ownedChunks.keys",
                bytemuck::cast_slice(&[0usize, 0]).to_vec(),
            ),
            (
                "managedReferences.items",
                bytemuck::cast_slice(&[1usize, 3]).to_vec(),
            ),
            (
                "handleReferences.start",
                bytemuck::cast_slice(&[1u64, 1]).to_vec(),
            ),
            (
                "nativeRetained.keys",
                bytemuck::cast_slice(&[0usize, 3]).to_vec(),
            ),
            (
                "chunkDominator",
                bytemuck::cast_slice(&[-2i32, -1, -1, 4, 3]).to_vec(),
            ),
        ] {
            let path = directory.path().join(format!("{name}.col"));
            let original = fs::read(&path).unwrap();
            atomic_write(&path, &bytes).unwrap();
            assert!(
                NativeGraphData::load(&store)
                    .and_then(|data| data.validate(3))
                    .is_err(),
                "{name}"
            );
            atomic_write(&path, &original).unwrap();
        }
    }
}
