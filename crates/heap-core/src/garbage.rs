use crate::{
    Result,
    analysis::HeapSnapshot,
    column::{Column, ColumnStore},
    graph::{CompressedRows, DominatorGraph, dominators},
};
use std::{collections::HashSet, sync::Arc};

pub struct GarbageGraph {
    pub references: CompressedRows,
    pub referrers: CompressedRows,
    pub unreferenced: Vec<i32>,
    pub retained: Column<u64>,
    pub idom: Column<i32>,
    pub parent: Column<i32>,
    pub dead_count: usize,
    pub dead_bytes: u64,
}
impl HeapSnapshot {
    pub fn garbage(&self) -> Result<Arc<GarbageGraph>> {
        self.memo("garbage graph", || GarbageGraph::new(self))
    }
}
impl GarbageGraph {
    pub fn new(s: &HeapSnapshot) -> Result<Self> {
        let store = ColumnStore::new(None)?;
        let n = s.heap.count();
        let references = CompressedRows::build(n, &store, |i| {
            s.graph
                .edges(i)
                .take(if s.graph.live[i] == 0 { usize::MAX } else { 0 })
                .filter(move |&j| j as usize != i && s.graph.live[j as usize] == 0)
        })?;
        let referrers = references.transpose(n, &store, None)?;
        let mut entries = Vec::new();
        let (mut dead_count, mut dead_bytes) = (0, 0);
        for i in 0..n {
            if s.graph.live[i] == 0 {
                dead_count += 1;
                dead_bytes += s.heap.size(i);
                if referrers.row(i).is_empty() {
                    entries.push(i as i32);
                }
            }
        }
        let mut parent: Column<i32> = store.make(n, None)?;
        parent.fill(i32::MIN);
        let mut queue = entries.clone();
        for &t in &entries {
            parent[t as usize] = -1;
        }
        fn drain(
            queue: &mut Vec<i32>,
            head: &mut usize,
            parent: &mut [i32],
            references: &CompressedRows,
        ) {
            while *head < queue.len() {
                let v = queue[*head];
                *head += 1;
                for &w in references.row(v as usize) {
                    if parent[w as usize] == i32::MIN {
                        parent[w as usize] = v;
                        queue.push(w);
                    }
                }
            }
        }
        let mut head = 0;
        drain(&mut queue, &mut head, &mut parent, &references);
        for i in 0..n {
            if s.graph.live[i] == 0 && parent[i] == i32::MIN {
                parent[i] = -1;
                entries.push(i as i32);
                queue.push(i as i32);
                drain(&mut queue, &mut head, &mut parent, &references);
            }
        }
        let graph = EntryGraph {
            references: &references,
            referrers: &referrers,
            entry_set: entries.iter().copied().collect(),
            entries: &entries,
        };
        let (idom, retained, _) = dominators(&graph, &store, |v| {
            if v < n && s.graph.live[v] == 0 {
                s.heap.size(v)
            } else {
                0
            }
        })?;
        Ok(Self {
            references,
            referrers,
            unreferenced: entries,
            retained,
            idom,
            parent,
            dead_count,
            dead_bytes,
        })
    }
    pub fn path_from_unreferenced(&self, i: usize) -> Vec<usize> {
        let mut chain = vec![i];
        let mut p = self.parent[i];
        while p >= 0 && chain.len() < 10_000 {
            chain.push(p as usize);
            p = self.parent[p as usize];
        }
        chain.reverse();
        chain
    }
}
pub(crate) struct EntryGraph<'a> {
    pub references: &'a CompressedRows,
    pub referrers: &'a CompressedRows,
    pub entries: &'a [i32],
    pub entry_set: HashSet<i32>,
}
impl DominatorGraph for EntryGraph<'_> {
    fn total(&self) -> usize {
        self.references.count() + 1
    }
    fn root(&self) -> usize {
        self.references.count()
    }
    fn first_position(&self, _: usize) -> u64 {
        0
    }
    fn next(&self, v: usize, position: &mut u64) -> Option<usize> {
        let row = if v == self.root() {
            self.entries
        } else {
            self.references.row(v)
        };
        let &w = row.get(*position as usize)?;
        *position += 1;
        Some(w as usize)
    }
    fn predecessors(&self, v: usize, emit: &mut dyn FnMut(usize)) {
        if v == self.root() {
            return;
        }
        for &r in self.referrers.row(v) {
            emit(r as usize);
        }
        if self.entry_set.contains(&(v as i32)) {
            emit(self.root());
        }
    }
}
