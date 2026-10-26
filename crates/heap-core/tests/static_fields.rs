use heap_core::{
    core_dump::{CoreDump, Layout, Platform, Segment},
    runtime::ContractDescriptor,
    type_names::TypeNames,
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

struct Memory(Vec<u8>);
impl Memory {
    fn word(&mut self, at: u64, value: u64) {
        self.0[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
    }
    fn int(&mut self, at: u64, value: u32) {
        self.0[at as usize..at as usize + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn short(&mut self, at: u64, value: u16) {
        self.0[at as usize..at as usize + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn table(&mut self, mt: u64, flags: u32, aux: u64, base: u64, thread: Option<u32>) {
        self.int(mt, flags);
        self.int(mt + 8, 2 | 0x100);
        self.word(mt + 24, 0x1300);
        self.word(mt + 32, aux);
        self.word(mt + 40, mt + 0x80);
        self.word(mt + 0x80 + 24, mt + 0xc0);
        self.short(mt + 0x80 + 52, 1);
        self.short(mt + 0x80 + 54, u16::from(thread.is_some()));
        self.word(aux - 24, base | 1);
        if flags & 0x30 != 0 {
            self.word(aux - 32, mt + 0xc0);
            self.word(mt + 0x80 + 24, 0x4000);
        }
        self.int(
            mt + 0xc0 + 8,
            1 | 1 << 24 | if thread.is_some() { 1 << 25 } else { 0 },
        );
        self.int(mt + 0xc0 + 12, 0x12 << 27 | 16);
        if let Some(index) = thread {
            self.int(aux - 40 + 4, index);
        }
    }
    fn names(&self, known: &[u64]) -> TypeNames {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("statics.bin");
        std::fs::write(&path, &self.0).unwrap();
        let dump = CoreDump::open(
            &path,
            None,
            Some(Layout {
                platform: Platform::LinuxX64,
                segments: vec![Segment {
                    address: 0,
                    size: self.0.len() as u64,
                    file_offset: 0,
                    writable: true,
                    executable: false,
                }],
                files: vec![],
                threads: vec![],
                exited_thread_stacks: HashSet::new(),
                shared_cache_ranges: vec![],
                residency: None,
            }),
        )
        .unwrap();
        TypeNames::new(Arc::new(dump), u64::MAX, &[], known.iter().copied())
    }
}
fn descriptor() -> ContractDescriptor {
    let types = [
        ("DynamicStaticsInfo", vec![("!", 24), ("GCStatics", 0)]),
        ("MethodTable", vec![("AuxiliaryData", 32)]),
        (
            "Module",
            vec![
                ("TypeDefToMethodTableMap", 64),
                ("AvailableTypeParams", 128),
            ],
        ),
        (
            "ModuleLookupMap",
            vec![
                ("Next", 0),
                ("TableData", 8),
                ("Count", 16),
                ("SupportedFlagsMask", 24),
            ],
        ),
        (
            "EEClass",
            vec![
                ("FieldDescList", 24),
                ("NumStaticFields", 52),
                ("NumThreadStaticFields", 54),
            ],
        ),
        ("ThreadStaticsInfo", vec![("!", 40), ("GCTlsIndex", 4)]),
        ("ThreadStore", vec![("FirstThreadLink", 0)]),
        (
            "Thread",
            vec![("LinkNext", 0), ("OSId", 8), ("ThreadLocalDataPtr", 16)],
        ),
        (
            "ThreadLocalData",
            vec![
                ("NonCollectibleTlsDataCount", 0),
                ("CollectibleTlsDataCount", 4),
                ("NonCollectibleTlsArrayData", 8),
                ("CollectibleTlsArrayData", 16),
                ("InFlightData", 32),
            ],
        ),
        (
            "InFlightTLSData",
            vec![("Next", 0), ("TlsIndex", 8), ("TLSData", 16)],
        ),
        (
            "EETypeHashTable",
            vec![
                ("Buckets", 0),
                ("VolatileEntryValue", 0),
                ("VolatileEntryNextEntry", 8),
            ],
        ),
        ("AppDomain", vec![("AssemblyList", 16)]),
        ("ArrayListBase", vec![("Count", 0), ("FirstBlock", 8)]),
        (
            "ArrayListBlock",
            vec![("Next", 0), ("Size", 8), ("ArrayStart", 16)],
        ),
        ("Assembly", vec![("Module", 0)]),
        ("FieldDesc", vec![("!", 16)]),
    ]
    .into_iter()
    .map(|(name, fields)| {
        (
            name.into(),
            fields
                .into_iter()
                .map(|(name, offset)| (name.into(), offset))
                .collect(),
        )
    })
    .collect();
    ContractDescriptor {
        address: 0,
        types,
        globals: BTreeMap::from([("AppDomain".into(), 0x1000), ("ThreadStore".into(), 0x1008)]),
        contracts: BTreeMap::new(),
    }
}

#[test]
fn statics_include_types_without_instances_and_closed_generic_bases() {
    let mut m = Memory(vec![0; 0xa000]);
    m.word(0x1000, 0x1100);
    m.int(0x1110, 1);
    m.int(0x1120, 1);
    m.word(0x1128, 0x1200);
    m.word(0x1200, 0x1300);
    m.word(0x1348, 0x1400);
    m.int(0x1350, 1);
    m.word(0x1400, 0x2800);
    m.word(0x1380, 0x1500);
    m.word(0x1500, 0x1600);
    m.word(0x1600, 1);
    m.word(0x1618, 0x1700);
    m.word(0x1700, 0x2a01);
    m.word(0x1708, 1);
    m.table(0x2000, 0, 0x5000, 0, None);
    m.word(0x2010, 0x2200);
    m.table(0x2200, 0x10, 0x5100, 0x7100, None);
    m.table(0x2400, 0xc0000, 0x5200, 0x7200, None);
    m.word(0x2430, 0x2c00);
    m.table(0x2800, 0, 0x5400, 0x7400, None);
    m.table(0x2a00, 0x10, 0x5500, 0x7500, None);
    m.table(0x2c00, 0, 0x5600, 0x7600, None);
    let names = m.names(&[0x2000, 0x2400]);
    let fields = names.static_field_slots(Some(&descriptor()));
    for (slot, mt) in [
        (0x7110, 0x2200),
        (0x7210, 0x2400),
        (0x7410, 0x2800),
        (0x7510, 0x2a00),
    ] {
        assert_eq!(fields.get(&slot).unwrap().method_table, mt);
        assert_eq!(fields[&slot].token, 1);
    }
    assert!(
        !fields.contains_key(&0x7610),
        "an interface's per-instantiation data is not an array element MethodTable"
    );
    assert_eq!(fields.len(), 4);
}

#[test]
fn thread_statics_follow_noncollectible_collectible_and_inflight_storage() {
    let mut m = Memory(vec![0; 0xa000]);
    m.table(0x2000, 0, 0x5000, 0, Some(3));
    m.table(0x2200, 0x10, 0x5100, 0, Some(0x0100_0001));
    m.table(0x2400, 0, 0x5200, 0, Some(5));
    m.table(0x2600, 0, 0x5300, 0, Some(u32::MAX));
    m.word(0x1008, 0x1800);
    m.word(0x1800, 0x1900);
    m.word(0x1900, 0x1940);
    m.word(0x1908, 101);
    m.word(0x1910, 0x8100);
    m.word(0x1948, 202);
    m.word(0x1950, 0x8200);
    m.int(0x8100, 4);
    m.int(0x8104, 2);
    m.word(0x8108, 0x9000);
    m.word(0x8110, 0x9100);
    m.word(0x9018, 0x9200);
    m.word(0x9108, 0x9300);
    m.word(0x9300, 0x9400);
    m.word(0x8120, 0x9500);
    m.int(0x9508, 5);
    m.word(0x9510, 0x9600);
    m.word(0x9600, 0x9700);
    m.int(0x8200, 4);
    m.word(0x8208, 0x9800);
    m.word(0x9818, 0x9900);
    let fields = m
        .names(&[0x2000, 0x2200, 0x2400, 0x2600])
        .static_field_slots(Some(&descriptor()));
    for (slot, mt, tid) in [
        (0x9210, 0x2000, 101),
        (0x9410, 0x2200, 101),
        (0x9710, 0x2400, 101),
        (0x9910, 0x2000, 202),
    ] {
        let f = fields.get(&slot).unwrap();
        assert_eq!((f.method_table, f.thread), (mt, Some(tid)));
    }
    assert_eq!(fields.len(), 4);
}

#[test]
fn array_component_size_is_not_generic_instantiation_flags() {
    let mut m = Memory(vec![0; 0x5000]);
    m.int(0x2000, 0x800a0030);
    m.word(0x2030, 0x3008);
    m.int(0x3004, 1 << 16 | 1);
    m.word(0x3008, 0x4000);
    m.word(0x4000, 0x2100);
    m.int(0x2200, 0x30);
    m.word(0x2230, 0x3008);
    let names = m.names(&[0x2000, 0x2200]);
    assert!(names.type_argument_handles(0x2000).is_empty());
    assert_eq!(names.type_argument_handles(0x2200), [0x2100]);
}
