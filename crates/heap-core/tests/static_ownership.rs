use heap_core::{
    analysis::HeapSnapshot, cache::CacheChoice, graph::AnalysisOptions, static_ownership::OwnerKind,
};
use std::{path::Path, process::Command};

#[test]
fn static_owners_count_shared_values_once_without_changing_physical_retention() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new(if cfg!(windows) { "python" } else { "python3" })
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../scripts/generate-compatibility-fixtures.py")
            )
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    for mode in ["fields", "types", "unnamed", "owned"] {
        let s = HeapSnapshot::open(
            &dir.path().join(format!("static-shared-{mode}.dmp")),
            vec![],
            None,
            AnalysisOptions::default(),
            CacheChoice::None,
            |_, _| {},
        )
        .unwrap();
        let physical = s.ownership().unwrap();
        let retained = s.type_retained().unwrap();
        let physical_storage = physical.retained(physical.object_node(0));
        assert_eq!(physical_storage, if mode == "owned" { 80 } else { 104 });
        assert_eq!(s.graph.live_count, 3);
        for managed_only in [false, true] {
            let tree = s.owner_tree(managed_only).unwrap();
            let root = tree.entry(&s, tree.root).unwrap().unwrap();
            assert_eq!(root.total(), 104);
            let mut pending = vec![tree.root];
            let mut types = vec![];
            let mut fields = vec![];
            let mut seen = std::collections::HashSet::new();
            while let Some(id) = pending.pop() {
                assert!(seen.insert(id), "an allocation must appear only once");
                let e = tree.entry(&s, id).unwrap().unwrap();
                let children = tree.children(&s, id);
                let bytes: u64 = children
                    .iter()
                    .map(|&n| tree.entry(&s, n).unwrap().unwrap().total())
                    .sum();
                assert_eq!(e.total(), e.size + bytes, "{mode}: {}", e.name);
                match e.kind {
                    OwnerKind::Type => types.push(e.total()),
                    OwnerKind::Field => {
                        fields.push(e.total());
                        assert_eq!(e.targets.len(), 1);
                    }
                    _ => {}
                }
                pending.extend(children);
            }
            assert_eq!(
                types,
                if mode == "fields" {
                    vec![48]
                } else if mode == "owned" {
                    vec![24]
                } else if mode == "types" {
                    vec![0, 0]
                } else {
                    vec![0]
                }
            );
            assert!(
                fields.iter().all(|&bytes| bytes == 0),
                "shared allocations must not be charged to an arbitrary field"
            );
            assert_eq!(fields.len(), if mode == "unnamed" { 2 } else { 4 });
            if mode == "owned" {
                assert!(
                    tree.roots
                        .iter()
                        .all(|&id| tree.entry(&s, id).unwrap().unwrap().kind != OwnerKind::Type),
                    "a field whose storage has an owner is not an independent root"
                );
            }
        }
        let local = s.storage_owner_tree(0).unwrap();
        assert_eq!(
            local
                .entry(&s, physical.object_node(0))
                .unwrap()
                .unwrap()
                .total(),
            physical_storage
        );
        assert_eq!(physical.retained(physical.object_node(0)), physical_storage);
        assert_eq!(&*s.type_retained().unwrap(), &*retained);
    }
}
