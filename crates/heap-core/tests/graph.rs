use heap_core::{
    column::ColumnStore,
    graph::{DominatorGraph, dominators},
};
use std::collections::BTreeSet;

struct Graph {
    edges: Vec<Vec<usize>>,
    parents: Vec<Vec<usize>>,
}
impl Graph {
    fn new(edges: Vec<Vec<usize>>) -> Self {
        let mut parents = vec![vec![]; edges.len()];
        for (v, children) in edges.iter().enumerate() {
            for &w in children {
                parents[w].push(v);
            }
        }
        Self { edges, parents }
    }
}
impl DominatorGraph for Graph {
    fn total(&self) -> usize {
        self.edges.len()
    }
    fn root(&self) -> usize {
        0
    }
    fn first_position(&self, _: usize) -> u64 {
        0
    }
    fn next(&self, v: usize, p: &mut u64) -> Option<usize> {
        let w = self.edges[v].get(*p as usize).copied();
        *p += 1;
        w
    }
    fn predecessors(&self, v: usize, body: &mut dyn FnMut(usize)) {
        for &p in &self.parents[v] {
            body(p);
        }
    }
}

fn check(edges: Vec<Vec<usize>>) {
    let graph = Graph::new(edges);
    let n = graph.total();
    let mut reachable = BTreeSet::from([0]);
    let mut todo = vec![0];
    while let Some(v) = todo.pop() {
        for &w in &graph.edges[v] {
            if reachable.insert(w) {
                todo.push(w);
            }
        }
    }
    let mut sets: Vec<_> = (0..n)
        .map(|v| {
            if v == 0 {
                BTreeSet::from([0])
            } else if reachable.contains(&v) {
                reachable.clone()
            } else {
                BTreeSet::new()
            }
        })
        .collect();
    loop {
        let mut changed = false;
        for &v in reachable.iter().filter(|&&v| v != 0) {
            let mut s = reachable.clone();
            for &p in graph.parents[v].iter().filter(|p| reachable.contains(p)) {
                s = s.intersection(&sets[p]).copied().collect();
            }
            s.insert(v);
            if sets[v] != s {
                sets[v] = s;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let (idom, retained, children) =
        dominators(&graph, &ColumnStore::default(), |v| (v + 1) as u64 * 8).unwrap();
    for v in 0..n {
        let expected = sets[v]
            .iter()
            .copied()
            .filter(|&d| d != v)
            .max_by_key(|&d| sets[d].len())
            .map(|d| d as i32)
            .unwrap_or(-1);
        assert_eq!(idom[v], expected, "node {v}, edges {:?}", graph.edges);
        let bytes: u64 = (0..n)
            .filter(|&w| sets[w].contains(&v))
            .map(|w| (w + 1) as u64 * 8)
            .sum();
        assert_eq!(
            retained[v], bytes,
            "retained node {v}, edges {:?}",
            graph.edges
        );
        let actual: BTreeSet<_> = children.row(v).iter().copied().collect();
        let expected: BTreeSet<_> = (0..n)
            .filter(|&w| idom[w] == v as i32)
            .map(|w| w as i32)
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn all_three_node_graphs_match_set_intersection_definition() {
    for mask in 0..(1 << 9) {
        let edges = (0..3)
            .map(|v| (0..3).filter(|&w| mask & (1 << (v * 3 + w)) != 0).collect())
            .collect();
        check(edges);
    }
}

#[test]
fn larger_graphs_with_cycles_and_duplicate_edges_match_definition() {
    let mut random = 0x2aef_2719_2388_91d5u64;
    for _ in 0..100 {
        let mut edges = vec![vec![]; 32];
        for row in &mut edges {
            for w in 0..32 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                if random.is_multiple_of(17) {
                    row.push(w);
                    if random.is_multiple_of(7) {
                        row.push(w);
                    }
                }
            }
        }
        check(edges);
    }
}
