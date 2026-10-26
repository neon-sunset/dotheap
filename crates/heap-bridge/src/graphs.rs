use crate::{
    models::*,
    snapshot::{AnalysisError, Result, Snapshot},
    types::Index,
};
use heap_core::{
    analysis::HeapSnapshot,
    garbage::GarbageGraph,
    native_graph::{ChunkAttribution, ChunkSizeGroup, NativeGraph},
    ownership::{OwnershipGraph, OwnershipRoot},
    thread_stacks::ThreadStacks,
};
use std::sync::Arc;

pub struct NativeInfo {
    pub count: Index,
    pub tcache_chunks: Index,
    pub fastbin_chunks: Index,
    pub cached_bytes: u64,
    pub totals: Vec<ReachTotal>,
}

pub struct NativeChunkInfo {
    pub chunk: Chunk,
    pub reach: Reach,
    pub class_name: Option<String>,
    pub library: Option<String>,
    pub owner: Option<Index>,
    pub root_sources: Vec<String>,
    pub pointers: Vec<i32>,
    pub referrers: Vec<i32>,
    pub owners: Vec<Index>,
    pub managed_references: Vec<Index>,
    pub handle_references: Vec<u64>,
}

pub struct NativeGraphView {
    snapshot: Arc<HeapSnapshot>,
    graph: Arc<NativeGraph>,
}

pub struct OwnershipView {
    graph: Arc<OwnershipGraph>,
}

#[repr(C)]
pub struct OwnershipInfo {
    pub count: Index,
    pub object_base: Index,
    pub chunk_base: Index,
}

pub struct OwnershipEntry {
    pub node: OwnershipNode,
    pub dominator: i32,
    pub managed: u64,
    pub native: u64,
}

pub struct GarbageView {
    graph: Arc<GarbageGraph>,
}

#[repr(C)]
pub struct GarbageInfo {
    pub dead_count: Index,
    pub dead_bytes: u64,
}

pub struct GarbageEntry {
    pub retained: u64,
    pub referrers: Vec<i32>,
    pub path: Vec<Index>,
}

pub struct ThreadStacksView {
    stacks: Arc<ThreadStacks>,
}

impl Snapshot {
    pub fn native_graph(&self) -> Result<Arc<NativeGraphView>> {
        let graph = self.checked(self.core.native_graph())?;
        Ok(Arc::new(NativeGraphView {
            snapshot: self.core.clone(),
            graph,
        }))
    }

    pub fn ownership(&self) -> Result<Arc<OwnershipView>> {
        let graph = self.checked(self.core.ownership())?;
        Ok(Arc::new(OwnershipView { graph }))
    }

    pub fn garbage(&self) -> Result<Arc<GarbageView>> {
        let graph = self.checked(self.core.garbage())?;
        Ok(Arc::new(GarbageView { graph }))
    }

    pub fn thread_stacks(&self) -> Result<Arc<ThreadStacksView>> {
        let stacks = self.checked(self.core.thread_stacks())?;
        Ok(Arc::new(ThreadStacksView { stacks }))
    }
}

impl NativeGraphView {
    pub fn info(&self) -> NativeInfo {
        NativeInfo {
            count: self.graph.chunks.len(),
            tcache_chunks: self.graph.tcache_chunks,
            fastbin_chunks: self.graph.fastbin_chunks,
            cached_bytes: self
                .graph
                .chunks
                .iter()
                .filter(|c| c.state == ChunkState::Cached)
                .map(|c| c.size)
                .sum(),
            totals: self.graph.totals(),
        }
    }

    pub fn chunk(&self, index: Index) -> Option<Chunk> {
        self.graph.chunks.get(index).cloned()
    }

    pub fn reach(&self, index: Index) -> Option<Reach> {
        self.graph.reach.get(index).copied()
    }

    pub fn chunk_info(&self, index: Index) -> Option<NativeChunkInfo> {
        let chunk = self.graph.chunks.get(index)?.clone();
        let key = index as i32;
        Some(NativeChunkInfo {
            chunk,
            reach: self.graph.reach[index],
            class_name: self.graph.class_name(index),
            library: self.graph.library(index),
            owner: self.graph.owning_object(index),
            root_sources: self
                .graph
                .root_sources
                .get(&key)
                .cloned()
                .unwrap_or_default(),
            pointers: self.graph.pointers(index).to_vec(),
            referrers: self.graph.referrers(index).to_vec(),
            owners: self
                .graph
                .chunk_owners
                .get(&key)
                .cloned()
                .unwrap_or_default(),
            managed_references: self
                .graph
                .managed_references
                .get(&key)
                .cloned()
                .unwrap_or_default(),
            handle_references: self
                .graph
                .handle_references
                .get(&key)
                .cloned()
                .unwrap_or_default(),
        })
    }

    pub fn chunk_containing(&self, address: u64) -> Option<Index> {
        self.graph.chunk(address)
    }

    pub fn owned_chunks(&self, object: Index) -> &[i32] {
        self.graph
            .owned_chunks
            .get(&object)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn is_root_chunk(&self, chunk: i32) -> bool {
        self.graph.root_chunks.contains(&chunk)
    }

    pub fn largest_chunks(&self, chunks: &[i32], size: u64, limit: Index) -> Result<Vec<i32>> {
        Ok(self
            .graph
            .largest_chunks(chunks, (size != 0).then_some(size), limit)?)
    }

    pub fn chunk_sizes(&self, chunks: &[i32]) -> Result<Vec<ChunkSizeGroup>> {
        Ok(self.graph.chunk_sizes(chunks)?)
    }

    pub fn attribution(&self, chunk: Index) -> Result<ChunkAttribution> {
        if chunk >= self.graph.chunks.len() {
            return Err(AnalysisError::Failed {
                message: "Chunk index is out of range".into(),
            });
        }
        let result = self.graph.attribution(chunk, &self.snapshot);
        self.snapshot.dump.check()?;
        Ok(result?)
    }

    pub fn pointer_is_exact(&self, source: Index, target: Index) -> bool {
        source < self.graph.chunks.len()
            && target < self.graph.chunks.len()
            && self.graph.pointer_is_exact(source, target)
    }

    pub fn native_retained(&self, object: Index) -> u64 {
        self.graph
            .native_retained
            .get(&object)
            .copied()
            .unwrap_or_default()
    }

    pub fn class_name(&self, chunk: Index) -> Option<String> {
        if chunk >= self.graph.chunks.len() {
            return None;
        }
        self.graph.class_name(chunk)
    }

    pub fn group_label(&self, chunk: Index) -> Result<String> {
        if chunk >= self.graph.chunks.len() {
            return Err(AnalysisError::Failed {
                message: "Chunk index is out of range".into(),
            });
        }
        let value = self.graph.group_label(chunk, &self.snapshot);
        self.snapshot.dump.check()?;
        Ok(value?)
    }

    pub fn owner_field(&self, object: Index, chunk: Index) -> Result<Option<String>> {
        if object >= self.snapshot.heap.count() || chunk >= self.graph.chunks.len() {
            return Err(AnalysisError::Failed {
                message: "Object or chunk index is out of range".into(),
            });
        }
        let value = self.graph.owner_field(object, chunk, &self.snapshot);
        self.snapshot.dump.check()?;
        Ok(value)
    }
}

impl OwnershipView {
    pub fn roots(&self) -> Vec<OwnershipRoot> {
        self.graph.roots()
    }

    pub fn root_references(&self, node: Index) -> &[i32] {
        self.graph.root_references(node)
    }

    pub fn info(&self) -> OwnershipInfo {
        OwnershipInfo {
            count: self.graph.count,
            object_base: self.graph.object_base,
            chunk_base: self.graph.chunk_base,
        }
    }

    pub fn entry(&self, node: Index) -> Option<OwnershipEntry> {
        (node < self.graph.count).then(|| OwnershipEntry {
            node: self.graph.node(node),
            dominator: self.graph.idom[node],
            managed: self.graph.retained_managed[node],
            native: self.graph.retained_native[node],
        })
    }

    pub fn children(&self, node: Index) -> Vec<Index> {
        if node >= self.graph.count {
            return Vec::new();
        }
        self.graph.children(node)
    }

    pub fn has_children(&self, node: Index) -> bool {
        node < self.graph.count && !self.graph.children_rows.row(node).is_empty()
    }

    pub fn path_to_root(&self, node: Index) -> Vec<Index> {
        if node >= self.graph.count {
            return Vec::new();
        }
        self.graph.path_to_root(node)
    }
}

impl GarbageView {
    pub fn info(&self) -> GarbageInfo {
        GarbageInfo {
            dead_count: self.graph.dead_count,
            dead_bytes: self.graph.dead_bytes,
        }
    }

    pub fn entry(&self, object: Index) -> Option<GarbageEntry> {
        (object < self.graph.retained.len()).then(|| GarbageEntry {
            retained: self.graph.retained[object],
            referrers: self.graph.referrers.row(object).to_vec(),
            path: self.graph.path_from_unreferenced(object),
        })
    }
}

impl ThreadStacksView {
    pub fn threads(&self) -> &[ThreadInfo] {
        &self.stacks.threads
    }

    pub fn describe(&self, address: u64) -> String {
        self.stacks.describe(address)
    }

    pub fn label(&self, tid: i32) -> String {
        self.stacks.label(tid)
    }

    pub fn summary(&self, tid: i32) -> Option<String> {
        self.stacks.thread(tid).map(|t| self.stacks.summary(t))
    }
}

impl NativeGraphView {
    pub fn chunks(&self) -> &[Chunk] {
        &self.graph.chunks
    }
    pub fn reaches(&self) -> &[Reach] {
        &self.graph.reach
    }
}

impl OwnershipView {
    pub fn dominators(&self) -> &[i32] {
        &self.graph.idom
    }
    pub fn retained_managed(&self) -> &[u64] {
        &self.graph.retained_managed
    }
    pub fn retained_native(&self) -> &[u64] {
        &self.graph.retained_native
    }
    pub fn children_starts(&self) -> &[u64] {
        &self.graph.children_rows.start
    }
}

impl GarbageView {
    pub fn retained(&self) -> &[u64] {
        &self.graph.retained
    }
}

impl GarbageView {
    pub fn unreferenced(&self) -> &[i32] {
        &self.graph.unreferenced
    }
}
