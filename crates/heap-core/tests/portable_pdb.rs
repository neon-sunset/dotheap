use heap_core::portable_pdb::PortablePdb;

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn absolute_document_paths_keep_the_empty_leading_component() {
    let mut bytes = vec![0; 0x110];
    bytes[..4].copy_from_slice(b"BSJB");
    put32(&mut bytes, 12, 4);
    bytes[16..20].copy_from_slice(b"v4\0\0");
    put16(&mut bytes, 22, 4);
    let mut at = 24;
    for (name, start, size) in [
        ("#Pdb", 0x60, 32),
        ("#~", 0x80, 54),
        ("#Blob", 0xc0, 37),
        ("#GUID", 0x100, 16),
    ] {
        put32(&mut bytes, at, start);
        put32(&mut bytes, at + 4, size);
        bytes[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
        at += 8 + (name.len() + 1).div_ceil(4) * 4;
    }
    bytes[0x88..0x90]
        .copy_from_slice(&((1u64 << 0x30) | (1u64 << 0x31) | (1u64 << 0x37)).to_le_bytes());
    for row_count in [0x98, 0x9c, 0xa0] {
        put32(&mut bytes, row_count, 1);
    }
    put16(&mut bytes, 0xa4, 12);
    put16(&mut bytes, 0xac, 1);
    put16(&mut bytes, 0xae, 16);
    put16(&mut bytes, 0xb0, 1 << 5);
    put16(&mut bytes, 0xb2, 1);
    put16(&mut bytes, 0xb4, 23);
    bytes[0xc1] = 10;
    bytes[0xc2..0xcc].copy_from_slice(b"Program.cs");
    bytes[0xcc..0xd0].copy_from_slice(&[3, b'/', 0, 1]);
    bytes[0xd0..0xd7].copy_from_slice(&[6, 0, 0, 0, 1, 10, 1]);
    bytes[0xd7] = 13;
    put32(&mut bytes, 0xdc, 1);
    put32(&mut bytes, 0xe0, 2);
    bytes[0xe4] = 1;
    bytes[0x100..0x110].copy_from_slice(&[
        0xc5, 0x2a, 0xfd, 0x54, 0x25, 0xe9, 0x1a, 0x40, 0x9c, 0x2a, 0xf9, 0x4f, 0x17, 0x10, 0x72,
        0xf8,
    ]);
    let pdb = PortablePdb::parse(bytes.clone()).unwrap();
    assert_eq!(pdb.document_name(1).as_deref(), Some("/Program.cs"));
    assert_eq!(
        pdb.await_locations(1, None).get(&0),
        Some(&("/Program.cs".into(), 10))
    );
    bytes[0xcf] = 0x7f;
    assert!(
        PortablePdb::parse(bytes)
            .unwrap()
            .document_name(1)
            .is_none()
    );
}
