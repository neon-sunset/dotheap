use heap_core::{core_dump::CoreDump, metadata::AssemblyMetadata};

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn assembly(name: &str, loaded: bool) -> Vec<u8> {
    let mut bytes = vec![0; if loaded { 0x4000 } else { 0x800 }];
    put16(&mut bytes, 0, 0x5a4d);
    put32(&mut bytes, 0x3c, 0x80);
    put32(&mut bytes, 0x80, 0x4550);
    put16(&mut bytes, 0x86, 1);
    put16(&mut bytes, 0x94, 0xf0);
    let optional = 0x98;
    put16(&mut bytes, optional, 0x20b);
    put32(&mut bytes, optional + 32, 0x2000);
    put32(&mut bytes, optional + 36, 0x200);
    put32(&mut bytes, optional + 56, 0x4000);
    put32(&mut bytes, optional + 60, 0x200);
    put32(&mut bytes, optional + 108, 16);
    put32(&mut bytes, optional + 112 + 14 * 8, 0x2000);
    put32(&mut bytes, optional + 116 + 14 * 8, 72);
    let section = optional + 0xf0;
    bytes[section..section + 5].copy_from_slice(b".text");
    put32(&mut bytes, section + 8, 0x600);
    put32(&mut bytes, section + 12, 0x2000);
    put32(&mut bytes, section + 16, 0x600);
    put32(&mut bytes, section + 20, 0x200);
    let cli = if loaded { 0x2000 } else { 0x200 };
    put32(&mut bytes, cli, 72);
    put32(&mut bytes, cli + 8, 0x2100);
    put32(&mut bytes, cli + 12, 0x100);

    let metadata = cli + 0x100;
    bytes[metadata..metadata + 4].copy_from_slice(b"BSJB");
    put16(&mut bytes, metadata + 4, 1);
    put16(&mut bytes, metadata + 6, 1);
    put32(&mut bytes, metadata + 12, 4);
    bytes[metadata + 16..metadata + 20].copy_from_slice(b"v4\0\0");
    put16(&mut bytes, metadata + 22, 2);
    put32(&mut bytes, metadata + 24, 0x40);
    put32(&mut bytes, metadata + 28, 68);
    bytes[metadata + 32..metadata + 35].copy_from_slice(b"#~\0");
    put32(&mut bytes, metadata + 36, 0x90);
    put32(&mut bytes, metadata + 40, (name.len() + 11) as u32);
    bytes[metadata + 44..metadata + 53].copy_from_slice(b"#Strings\0");

    let tables = metadata + 0x40;
    put64(&mut bytes, tables + 8, (1 << 2) | (1 << 32));
    put32(&mut bytes, tables + 24, 1);
    put32(&mut bytes, tables + 28, 1);
    put16(&mut bytes, tables + 32 + 4, 1);
    put16(&mut bytes, tables + 32 + 10, 1);
    put16(&mut bytes, tables + 32 + 12, 1);
    put16(&mut bytes, tables + 32 + 14 + 18, 10);
    let strings = metadata + 0x90;
    bytes[strings..strings + 10].copy_from_slice(b"\0<Module>\0");
    bytes[strings + 10..strings + 10 + name.len()].copy_from_slice(name.as_bytes());
    bytes
}

#[test]
fn adjacent_flat_assemblies_and_loaded_images_have_distinct_ranges() {
    let flat = assembly("Alpha", false);
    let second = assembly("Beta", false);
    let loaded = assembly("Gamma", true);
    for (image, name, length) in [(&flat, "Alpha", 0x800), (&loaded, "Gamma", 0x4000)] {
        let metadata = AssemblyMetadata::from_bytes(image.clone()).unwrap();
        assert_eq!(metadata.assembly_name().as_deref(), Some(name));
        assert_eq!(metadata.image_length(), Some(length));
    }

    let mut core = vec![0; 0x9000];
    put32(&mut core, 0, 0xfeedfacf);
    put32(&mut core, 4, 0x0100_000c);
    put32(&mut core, 12, 4);
    put32(&mut core, 16, 1);
    put32(&mut core, 20, 72);
    let command = 32;
    put32(&mut core, command, 0x19);
    put32(&mut core, command + 4, 72);
    put64(&mut core, command + 24, 0x100000);
    put64(&mut core, command + 32, 0x8000);
    put64(&mut core, command + 40, 0x1000);
    put64(&mut core, command + 48, 0x8000);
    put32(&mut core, command + 60, 1);
    core[0x1000..0x1800].copy_from_slice(&flat);
    core[0x2000..0x2800].copy_from_slice(&second);
    core[0x5000..0x9000].copy_from_slice(&loaded);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("assemblies.dmp");
    std::fs::write(&path, core).unwrap();
    let dump = CoreDump::open(&path, None, None).unwrap();
    let images: Vec<_> = dump
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.start, f.end))
        .collect();
    assert_eq!(
        images,
        [
            ("Alpha.dll", 0x100000, 0x100800),
            ("Beta.dll", 0x101000, 0x101800),
            ("Gamma.dll", 0x104000, 0x108000),
        ]
    );
    assert!(dump.file_mapping(0x100900).is_none());
    assert_eq!(dump.file_mapping(0x101100).unwrap().path, "Beta.dll");
}

#[test]
fn missing_blob_pages_do_not_hide_available_assembly_metadata() {
    let mut image = assembly("Partial", false);
    let metadata = 0x300;
    let strings = image[metadata + 0x90..metadata + 0x90 + 18].to_vec();
    image.copy_within(metadata + 0x40..metadata + 0x40 + 68, metadata + 0x60);
    image[metadata + 0xb0..metadata + 0xb0 + 18].copy_from_slice(&strings);
    put16(&mut image, metadata + 22, 3);
    put32(&mut image, metadata + 24, 0x60);
    put32(&mut image, metadata + 36, 0xb0);
    put32(&mut image, metadata + 56, 0x480);
    put32(&mut image, metadata + 60, 0x100);
    image[metadata + 64..metadata + 70].copy_from_slice(b"#Blob\0");
    let parsed = AssemblyMetadata::from_bytes(image).unwrap();
    assert_eq!(parsed.assembly_name().as_deref(), Some("Partial"));
    assert_eq!(parsed.type_name(1).as_deref(), Some("<Module>"));
    assert!(parsed.field_signature(1).is_none());
}
