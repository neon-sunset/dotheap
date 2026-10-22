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
            graph.largest_chunks(&indices, limit).unwrap(),
            expected[..limit.min(expected.len())]
        );
    }
    assert_eq!(indices[0], 1999);
    assert!(graph.largest_chunks(&[-1], 1).is_err());
    assert!(graph.largest_chunks(&[2000], 1).is_err());
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
            root_source: BTreeMap::from([
                (0, "first library".into()),
                (1, "second library".into()),
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
