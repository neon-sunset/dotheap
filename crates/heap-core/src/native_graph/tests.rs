use super::*;
use crate::{
    cache::CacheChoice,
    graph::{AnalysisOptions, GcHandle, RootKind},
};
use std::{path::Path, process::Command};

#[test]
fn managed_native_references_follow_field_and_array_layouts() {
    const PAGE: u64 = 0x4000_0000;
    const FIRST: u64 = 0x4020_9018;
    const MT: u64 = PAGE + 0x2000;
    const ELEMENT: u64 = PAGE + 0x9000;
    const POINTER: u64 = 0x7de0_71ee_7a83;

    fn put32(data: &mut [u8], address: u64, value: u32) {
        let at = (8192 + address - PAGE) as usize;
        data[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put64(data: &mut [u8], address: u64, value: u64) {
        let at = (8192 + address - PAGE) as usize;
        data[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn fields(data: &mut [u8], mt: u64, ee: u64, fields: &[(u8, u32)], inherited: u16) {
        put64(data, mt + 40, ee);
        put64(data, ee + 24, ee + 128);
        let at = (8192 + ee + 50 - PAGE) as usize;
        data[at..at + 2].copy_from_slice(&(fields.len() as u16 + inherited).to_le_bytes());
        for (index, &(kind, offset)) in fields.iter().enumerate() {
            put32(data, ee + 128 + index as u64 * 16 + 8, index as u32 + 1);
            put32(
                data,
                ee + 128 + index as u64 * 16 + 12,
                u32::from(kind) << 27 | offset,
            );
        }
    }

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
    let fixture = |name| std::fs::read(directory.path().join(name)).unwrap();
    let check = |name: &str, data: &[u8], expected: Option<&str>| {
        let path = directory.path().join(format!("{name}.dmp"));
        std::fs::write(&path, data).unwrap();
        let snapshot = HeapSnapshot::open(
            &path,
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::None,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(snapshot.heap.addresses[0], FIRST);
        let mut graph = NativeGraphData {
            chunks: vec![Chunk {
                start: 0x7de0_7082_3000,
                size: 87_293_952,
                state: ChunkState::Mmapped,
            }],
            first_words: vec![0],
            edge_start: vec![0; 2],
            referrer_start: vec![0; 2],
            root_chunks: BTreeSet::from([0]),
            ..Default::default()
        };
        graph.scan_managed(&snapshot).unwrap();
        assert_eq!(
            graph.owned_chunks.get(&0).map(Vec::as_slice),
            expected.map(|_| &[0][..]),
            "{name}"
        );
        assert_eq!(
            graph.chunk_owners.get(&0).map(Vec::as_slice),
            expected.map(|_| &[0usize][..]),
            "{name}"
        );
        graph.compute_reach(&snapshot);
        graph.compute_dominators().unwrap();
        assert_eq!(graph.reach, [Reach::NativeRoots]);
        assert_eq!(graph.owning_object(0), None);
        let graph = NativeGraph {
            data: Arc::new(graph),
            symbols: snapshot.symbols().unwrap(),
            nearest_class: OnceLock::new(),
        };
        assert_eq!(
            graph.owner_field(0, 0, &snapshot).as_deref(),
            expected,
            "{name}"
        );
        assert!(!graph.label(0, &snapshot).contains("owned by"));
        assert_eq!(
            graph.label(0, &snapshot).starts_with("referenced by"),
            expected.is_some()
        );
        if expected.is_some() {
            assert_eq!(
                graph
                    .group_label(0, &snapshot)
                    .unwrap()
                    .matches("referenced by")
                    .count(),
                1
            );
        }
    };

    for kind in [0xf, 0x18, 0x19, 0x1b, 0xa, 0xb, 0xd, 0x12, 0x11] {
        let mut data = fixture("native-pointer-fields.dmp");
        fields(&mut data, MT, PAGE + 0xa000, &[(kind, 0)], 0);
        put64(&mut data, FIRST + 8, POINTER);
        check(
            &format!("field-{kind}"),
            &data,
            matches!(kind, 0xf | 0x18 | 0x19 | 0x1b).then_some("field 1"),
        );
    }
    let mut data = fixture("native-pointer-fields.dmp");
    fields(&mut data, MT, PAGE + 0xa000, &[(9, 0), (8, 4)], 0);
    put64(&mut data, FIRST + 8, POINTER);
    check("adjacent-integers", &data, None);
    fields(&mut data, MT, PAGE + 0xa000, &[(0x18, 1000)], 0);
    check("field-outside-object", &data, None);
    fields(&mut data, MT, PAGE + 0xa000, &[(0xf, 1)], 0);
    put64(&mut data, FIRST + 9, POINTER);
    check("packed-pointer", &data, Some("field 1"));

    let mut data = fixture("native-pointer-fields.dmp");
    put64(&mut data, MT + 16, ELEMENT);
    fields(&mut data, ELEMENT, PAGE + 0xb000, &[(0x18, 0)], 0);
    fields(&mut data, MT, PAGE + 0xa000, &[(0xa, 8)], 1);
    put64(&mut data, FIRST + 8, POINTER);
    check("inherited-pointer", &data, Some("field 1"));

    let mut data = fixture("native-pointer-array.dmp");
    put32(&mut data, MT, 0x800a_0028);
    put64(&mut data, MT + 48, ELEMENT);
    put32(&mut data, ELEMENT, 0x40000);
    fields(
        &mut data,
        ELEMENT,
        PAGE + 0xa000,
        &[(0x12, 0), (9, 8), (8, 12), (0x12, 16), (0x12, 24), (8, 32)],
        0,
    );
    put64(&mut data, FIRST + 16 + 40 + 8, POINTER);
    check("dictionary-entry-array", &data, None);
    fields(&mut data, ELEMENT, PAGE + 0xa000, &[(0x18, 8)], 0);
    check("struct-pointer-array", &data, Some("[1].field 1"));
    put64(&mut data, FIRST + 16 + 40 + 8, 0);
    put64(&mut data, FIRST + 96, POINTER);
    check("array-padding", &data, None);

    for (name, flags, header) in [
        ("native-pointer-elements.dmp", 0x800a_0008, 16),
        ("native-pointer-multidimensional.dmp", 0x8008_0008, 32),
    ] {
        let mut data = fixture(name);
        put32(&mut data, MT, flags);
        put64(&mut data, MT + 48, ELEMENT);
        put64(&mut data, FIRST + header + 8, POINTER);
        for (kind, is_pointer) in [(0x18, true), (0x19, true), (0xa, false), (0xd, false)] {
            put32(&mut data, ELEMENT, 0x70000);
            fields(&mut data, ELEMENT, PAGE + 0xa000, &[(kind, 0)], 0);
            check(
                &format!("{name}-{kind}"),
                &data,
                is_pointer.then_some("[1].field 1"),
            );
        }
        put32(&mut data, ELEMENT, 0);
        fields(&mut data, ELEMENT, PAGE + 0xa000, &[(0x18, 0)], 0);
        check(&format!("{name}-managed-references"), &data, None);
        put64(&mut data, MT + 48, ELEMENT | 2);
        for kind in [0xf, 0x1b] {
            put32(&mut data, ELEMENT, kind);
            check(&format!("{name}-typedesc-{kind}"), &data, Some("[1]"));
        }
    }
}

#[test]
fn largest_chunks_match_a_stable_sort_without_changing_group_order() {
    let graph = NativeGraphData {
        chunks: (0..2000)
            .map(|i| Chunk {
                start: i * 4096,
                size: (i * 73 % 17 + 1) * 16,
                state: ChunkState::InUse,
            })
            .collect(),
        ..Default::default()
    };
    let indices: Vec<i32> = (0..2000).rev().collect();
    let mut expected = indices.clone();
    expected.sort_by_key(|&i| Reverse(graph.chunks[i as usize].size));
    for limit in [0, 1, 500, 2000, usize::MAX] {
        assert_eq!(
            graph.largest_chunks(&indices, None, limit).unwrap(),
            expected[..limit.min(expected.len())]
        );
    }
    assert_eq!(indices[0], 1999);
    for size in [16, 64, 272, 999] {
        let filtered: Vec<_> = expected
            .iter()
            .copied()
            .filter(|&i| graph.chunks[i as usize].size == size)
            .collect();
        assert_eq!(
            graph.largest_chunks(&indices, Some(size), 25).unwrap(),
            filtered[..filtered.len().min(25)]
        );
    }
    let sizes = graph.chunk_sizes(&indices).unwrap();
    assert_eq!(sizes.iter().map(|g| g.count).sum::<usize>(), indices.len());
    assert_eq!(
        sizes.iter().map(|g| g.bytes).sum::<u64>(),
        graph.chunks.iter().map(|c| c.size).sum::<u64>()
    );
    assert!(sizes.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    for size in sizes {
        assert_eq!(
            size.count,
            indices
                .iter()
                .filter(|&&i| graph.chunks[i as usize].size == size.size)
                .count()
        );
    }
    assert!(graph.largest_chunks(&[-1], None, 1).is_err());
    assert!(graph.largest_chunks(&[2000], None, 1).is_err());
    assert!(graph.chunk_sizes(&[-1]).is_err());
    assert!(graph.chunk_sizes(&[2000]).is_err());
    assert!(graph.chunk_sizes(&[]).unwrap().is_empty());
}

#[test]
fn cpp_referrers_stop_at_direct_roots_and_managed_references() {
    use crate::symbols::Symbol;

    for rooted in [true, false] {
        let graph = NativeGraph {
            data: Arc::new(NativeGraphData {
                chunks: (0..7)
                    .map(|i| Chunk {
                        start: 0x10000 + i * 0x100,
                        size: 64,
                        state: ChunkState::InUse,
                    })
                    .collect(),
                edge_start: vec![0, 2, 4, 5, 5, 5, 5, 6],
                edge_targets: vec![1, 5, 2, 4, 3, 2],
                edge_exact: vec![true, false, true, true, true, true],
                referrer_start: vec![0, 0, 1, 3, 4, 5, 6, 6],
                referrer_items: vec![0, 1, 6, 2, 1, 0],
                first_words: vec![0x1010, 0, 0, 0, 0, 0, 0],
                root_chunks: if rooted {
                    BTreeSet::from([0, 2, 6])
                } else {
                    BTreeSet::from([0, 6])
                },
                chunk_owners: if rooted {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([(2, vec![0])])
                },
                chunk_dominator: vec![-1, 0, -1, 2, 1, 0, -1],
                ..Default::default()
            }),
            symbols: Arc::new(LibrarySymbols {
                symbols: vec![Symbol {
                    address: 0x1000,
                    size: 64,
                    name: "_ZTV5Owner".into(),
                    library: "owner.so".into(),
                }],
                ..Default::default()
            }),
            nearest_class: OnceLock::new(),
        };
        for c in [1, 4] {
            assert_eq!(graph.nearest_class(c), Some(("Owner".into(), true)));
            assert_eq!(graph.library(c).as_deref(), Some("owner.so"));
        }
        assert_eq!(graph.nearest_class(5), Some(("Owner".into(), false)));
        for c in [2, 3, 6] {
            assert_eq!(graph.nearest_class(c), None);
            assert_eq!(graph.library(c), None);
        }
    }
}

#[test]
fn native_groups_separate_types_from_referrers_and_aggregate_all_sizes() {
    use crate::symbols::Symbol;
    use ChunkAttributionKind::*;

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
    let snapshot = HeapSnapshot::open(
        &directory.path().join("reference-rooted.dmp"),
        Vec::new(),
        None,
        AnalysisOptions::default(),
        CacheChoice::Folder(directory.path().join("cache")),
        |_, _| {},
    )
    .unwrap();
    let mut data = NativeGraphData {
        chunks: [64, 3904, 4064, 3904, 64, 4000, 32, 48, 8192, 32, 32, 32]
            .into_iter()
            .enumerate()
            .map(|(i, size)| Chunk {
                start: 0x10000 + i as u64 * 0x10000,
                size,
                state: ChunkState::InUse,
            })
            .collect(),
        reach: vec![Reach::NativeRoots; 12],
        edge_start: vec![0, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4],
        edge_targets: vec![1, 2, 3, 5],
        edge_exact: vec![true, true, false, true],
        referrer_start: vec![0, 0, 1, 2, 3, 3, 4, 4, 4, 4, 4, 4, 4],
        referrer_items: vec![0, 0, 0, 4],
        first_words: vec![0x1010, 0, 0, 0, 0x2010, 0, 0, 0, 0, 0, 0, 0],
        root_chunks: BTreeSet::from([0, 4, 6, 7, 11]),
        chunk_dominator: vec![-1, 0, -1, -1, -1, 4, -1, -1, -1, -1, -1, -1],
        texts: BTreeMap::from([(1, "buffer contents".into())]),
        ..Default::default()
    };
    data.reach[6] = Reach::Unreferenced;
    data.reach[7] = Reach::Unreferenced;
    data.chunks[8].state = ChunkState::Mapping;
    data.chunks[9].state = ChunkState::Free;
    data.chunks[10].state = ChunkState::Cached;
    data.chunks[11].state = ChunkState::Mmapped;
    let symbols = LibrarySymbols {
        symbols: vec![
            Symbol {
                address: 0x1000,
                size: 64,
                name: "_ZTV5Owner".into(),
                library: "first.so".into(),
            },
            Symbol {
                address: 0x2000,
                size: 64,
                name: "_ZTV5Owner".into(),
                library: "second.so".into(),
            },
        ],
        ..Default::default()
    };
    let graph = snapshot
        .memo("native graph", || {
            Ok(NativeGraph {
                data: Arc::new(data),
                symbols: Arc::new(symbols),
                nearest_class: OnceLock::new(),
            })
        })
        .unwrap();
    let groups = snapshot.chunk_groups().unwrap();
    let group = groups
        .iter()
        .find(|g| g.kind == CppReferrer && g.library.as_deref() == Some("first.so"))
        .unwrap();
    assert_eq!(group.label, "Owner");
    assert_eq!(group.chunks, [1, 2, 3]);
    assert_eq!(group.bytes, 3904 * 2 + 4064);
    assert_eq!((group.min_size, group.max_size), (3904, 4064));
    assert_eq!(graph.attribution(1, &snapshot).unwrap().kind, CppDominator);
    assert_eq!(graph.attribution(2, &snapshot).unwrap().kind, CppReferrer);
    assert_eq!(
        graph.attribution(3, &snapshot).unwrap().kind,
        CppInteriorReferrer
    );
    assert_eq!(
        graph.attribution(3, &snapshot).unwrap().source_chunk,
        Some(0)
    );
    assert!(graph.pointer_is_exact(0, 2));
    assert!(!graph.pointer_is_exact(0, 3));
    assert_eq!(
        groups
            .iter()
            .find(|g| g.kind == CppType && g.library.as_deref() == Some("first.so"))
            .unwrap()
            .chunks,
        [0]
    );
    assert_eq!(
        groups
            .iter()
            .find(|g| g.kind == CppReferrer && g.library.as_deref() == Some("second.so"))
            .unwrap()
            .chunks,
        [5]
    );
    assert_eq!(
        groups
            .iter()
            .find(|g| g.reach == Reach::Unreferenced)
            .unwrap()
            .chunks,
        [6, 7]
    );
    assert_eq!(
        groups
            .iter()
            .map(|g| g.id.as_str())
            .collect::<HashSet<_>>()
            .len(),
        groups.len()
    );
    let mut members: Vec<_> = groups
        .iter()
        .flat_map(|g| g.chunks.iter().copied())
        .collect();
    members.sort();
    assert_eq!(members, [0, 1, 2, 3, 4, 5, 6, 7, 11]);
    for total in graph.totals() {
        assert_eq!(
            total.bytes,
            groups
                .iter()
                .filter(|g| g.reach == total.reach)
                .map(|g| g.bytes)
                .sum::<u64>()
        );
    }
    let ownership = snapshot.ownership().unwrap();
    let children = snapshot.ownership_groups(ownership.chunk_node(0)).unwrap();
    let children = children
        .iter()
        .find(|group| group.kind == Some(CppReferrer))
        .unwrap();
    assert_eq!(children.name, "Owner");
    assert_eq!(children.library.as_deref(), Some("first.so"));
    assert_eq!(children.nodes.len(), 3);
    assert_eq!(children.size, 3904 * 2 + 4064);
    assert_eq!(children.native, children.size);
    let cached = HeapSnapshot::open(
        &directory.path().join("reference-rooted.dmp"),
        Vec::new(),
        None,
        AnalysisOptions::default(),
        CacheChoice::Folder(directory.path().join("cache")),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&*groups).unwrap(),
        serde_json::to_value(&*cached.chunk_groups().unwrap()).unwrap()
    );
}

#[test]
fn type_retention_includes_native_memory_without_counting_nested_instances_twice() {
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
    for (shared, same_type) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut snapshot = HeapSnapshot::open(
            &directory.path().join("reference-rooted.dmp"),
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::None,
            |_, _| {},
        )
        .unwrap();
        let parent_type = snapshot.heap.type_ids[0];
        let held_type = snapshot.heap.type_ids[2];
        snapshot.heap.type_ids[1] = parent_type;
        if same_type {
            snapshot.heap.type_ids[2] = parent_type;
        }
        snapshot.graph.live[2] = 1;
        snapshot.graph.saved.handles = vec![GcHandle {
            kind: 2,
            slot: 0x1000,
            target: Some(2),
        }];
        let mut data = NativeGraphData {
            chunks: vec![Chunk {
                start: 0x10000,
                size: 4096,
                state: ChunkState::InUse,
            }],
            reach: vec![Reach::LiveManaged],
            edge_start: vec![0; 2],
            referrer_start: vec![0; 2],
            chunk_dominator: vec![-3],
            first_words: vec![0],
            owned_chunks: BTreeMap::from([(1, vec![0])]),
            chunk_owners: BTreeMap::from([(0, vec![1])]),
            handle_references: BTreeMap::from([(0, vec![0x1000])]),
            ..Default::default()
        };
        if shared {
            data.root_chunks.insert(0);
            data.root_sources.insert(0, vec!["library data".into()]);
            data.reach[0] = Reach::NativeRoots;
            data.chunk_dominator[0] = -1;
        }
        data.validate(snapshot.heap.count()).unwrap();
        let symbols = snapshot.symbols().unwrap();
        snapshot
            .memo("native graph", || {
                Ok(NativeGraph {
                    data: Arc::new(data),
                    symbols,
                    nearest_class: OnceLock::new(),
                })
            })
            .unwrap();
        let graph = snapshot.ownership().unwrap();
        let own_bytes = snapshot.heap.size(0) + snapshot.heap.size(1);
        let held_bytes = snapshot.heap.size(2);
        let expected = own_bytes + if shared { 0 } else { 4096 + held_bytes };
        assert_eq!(graph.retained(graph.object_node(0)), expected);
        let retained = snapshot.type_retained().unwrap();
        assert_eq!(
            retained[parent_type as usize],
            expected + if shared && same_type { held_bytes } else { 0 }
        );
        assert_eq!(
            retained[held_type as usize],
            if same_type { 0 } else { held_bytes }
        );
        assert_eq!(
            snapshot.ownership_instances(parent_type, 2).unwrap(),
            [0, 1]
        );
        assert_eq!(snapshot.ownership_instances(parent_type, 1).unwrap(), [0]);
        let groups = snapshot
            .ownership_dominator_types(if same_type { parent_type } else { held_type })
            .unwrap();
        let native_owner = groups
            .iter()
            .find(|group| group.nodes == [graph.chunk_node(0)])
            .unwrap();
        assert_eq!(native_owner.type_id, None);
        assert_eq!(native_owner.bytes, held_bytes);
    }
}

#[test]
fn shared_strong_handle_keeps_all_native_owners() {
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
    for shared in [false, true] {
        let mut snapshot = HeapSnapshot::open(
            &directory.path().join("reference-rooted.dmp"),
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::None,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(snapshot.graph.sources.len(), 1);
        snapshot.graph.saved.sources[0].kind = RootKind::Handles {
            kind: "strong".into(),
        };
        snapshot.graph.saved.handles = vec![GcHandle {
            kind: 2,
            slot: 0x1000,
            target: Some(0),
        }];
        let mut data = NativeGraphData {
            chunks: vec![
                Chunk {
                    start: 0x10000,
                    size: 64,
                    state: ChunkState::InUse,
                },
                Chunk {
                    start: 0x20000,
                    size: 64,
                    state: ChunkState::InUse,
                },
            ],
            reach: vec![Reach::NativeRoots; 2],
            edge_start: vec![0; 3],
            referrer_start: vec![0; 3],
            chunk_dominator: vec![-1; 2],
            first_words: vec![0; 2],
            root_chunks: BTreeSet::from([0, 1]),
            root_sources: BTreeMap::from([
                (0, vec!["first library".into()]),
                (1, vec!["second library".into()]),
            ]),
            handle_references: BTreeMap::from([(0, vec![0x1000])]),
            ..Default::default()
        };
        if shared {
            data.handle_references.insert(1, vec![0x1000]);
        }
        data.validate(snapshot.heap.count()).unwrap();
        let symbols = snapshot.symbols().unwrap();
        snapshot
            .memo("native graph", || {
                Ok(NativeGraph {
                    data: Arc::new(data),
                    symbols,
                    nearest_class: OnceLock::new(),
                })
            })
            .unwrap();
        let ownership = snapshot.ownership().unwrap();
        let sources = ownership.roots();
        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .all(|source| source.objects == 0 && source.chunks == 1)
        );
        for source in &sources {
            assert_eq!(ownership.root_references(source.node).len(), 1);
            assert!(matches!(
                ownership.node(ownership.root_references(source.node)[0] as usize),
                crate::ownership::OwnershipNode::Chunk(_)
            ));
        }
        let object = ownership.object_base;
        let chunk = ownership.chunk_base;
        let bytes = snapshot.heap.size(0) + snapshot.heap.size(1);
        assert_eq!(ownership.retained_managed[0], bytes);
        if shared {
            assert_eq!(ownership.idom[object], 0);
            assert_eq!(ownership.retained_managed[chunk], 0);
            assert_eq!(ownership.retained_managed[chunk + 1], 0);
        } else {
            assert_eq!(ownership.idom[object], chunk as i32);
            assert_eq!(ownership.retained_managed[chunk], bytes);
        }
    }
}

#[test]
fn a_native_allocation_shared_by_roots_is_not_retained_by_either_source() {
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
    for roots in [
        ["library data", "stack of thread 1"],
        ["stack of thread 1", "library data"],
    ] {
        let mut snapshot = HeapSnapshot::open(
            &directory.path().join("reference-rooted.dmp"),
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::None,
            |_, _| {},
        )
        .unwrap();
        snapshot.graph.saved.sources[0].kind = RootKind::Handles {
            kind: "strong".into(),
        };
        snapshot.graph.saved.handles = vec![GcHandle {
            kind: 2,
            slot: 0x1000,
            target: Some(0),
        }];
        let mut data = NativeGraphData {
            chunks: vec![Chunk {
                start: 0x10000,
                size: 64,
                state: ChunkState::InUse,
            }],
            reach: vec![Reach::NativeRoots],
            edge_start: vec![0; 2],
            referrer_start: vec![0; 2],
            chunk_dominator: vec![-1],
            first_words: vec![0],
            handle_references: BTreeMap::from([(0, vec![0x1000])]),
            ..Default::default()
        };
        for root in roots {
            data.add_root(0, root);
            data.add_root(0, root);
        }
        assert_eq!(data.root_sources[&0], ["library data", "stack of thread 1"]);
        data.validate(snapshot.heap.count()).unwrap();
        let symbols = snapshot.symbols().unwrap();
        snapshot
            .memo("native graph", || {
                Ok(NativeGraph {
                    data: Arc::new(data),
                    symbols,
                    nearest_class: OnceLock::new(),
                })
            })
            .unwrap();
        let ownership = snapshot.ownership().unwrap();
        let chunk = ownership.chunk_node(0);
        assert_eq!(ownership.idom[chunk], 0);
        assert_eq!(ownership.idom[ownership.object_node(0)], chunk as i32);
        let roots = ownership.roots();
        assert_eq!(roots.len(), 2);
        for root in roots {
            assert_eq!(ownership.root_references(root.node), [chunk as i32]);
            assert_eq!(ownership.retained_managed[root.node], 0);
            assert_eq!(ownership.retained_native[root.node], 0);
        }
        assert_eq!(
            ownership.retained_managed[chunk],
            snapshot.heap.size(0) + snapshot.heap.size(1)
        );
    }
}

#[test]
fn ownership_roots_include_native_references_after_reopening_the_cache() {
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
    let mut expected = None;
    for _ in 0..2 {
        let snapshot = HeapSnapshot::open(
            &directory.path().join("native-allocations.dmp"),
            Vec::new(),
            None,
            AnalysisOptions::default(),
            CacheChoice::Folder(directory.path().join("cache")),
            |_, _| {},
        )
        .unwrap();
        let graph = snapshot.ownership().unwrap();
        let roots = graph.roots();
        assert_eq!(roots.iter().map(|r| r.objects).sum::<usize>(), 1);
        assert_eq!(roots.iter().map(|r| r.chunks).sum::<usize>(), 1);
        let native = roots.iter().find(|r| r.chunks == 1).unwrap();
        assert_eq!(native.native, 16384);
        assert_eq!(
            graph.root_references(native.node),
            [graph.chunk_node(0) as i32]
        );
        assert_eq!(graph.retained_native[graph.chunk_node(0)], 16384);
        assert!(!graph.is_reachable(graph.chunk_node(3)));
        let rows: Vec<_> = roots
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    graph.root_references(r.node).to_vec(),
                    r.managed,
                    r.native,
                )
            })
            .collect();
        if let Some(expected) = &expected {
            assert_eq!(&rows, expected);
        }
        expected = Some(rows);
    }
}
