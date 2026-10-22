use heap_core::{
    metadata::{compressed_integer, instruction},
    portable_pdb::signed_integer,
};

#[test]
fn calls_inside_operands_are_not_instructions() {
    let il = [
        0x21, 0x28, 1, 0, 0, 0x0a, 0, 0, 0, 0x28, 2, 0, 0, 0x0a, 0x2a,
    ];
    let mut at = 0;
    let mut calls = Vec::new();
    while let Some(i) = instruction(&il, &mut at) {
        if i.opcode == 0x28 {
            calls.push(i.offset);
        }
    }
    assert_eq!(calls, vec![9]);
    assert_eq!(at, il.len());
}
#[test]
fn truncated_switch_and_reserved_compressed_integer_are_rejected() {
    let mut at = 0;
    assert!(instruction(&[0x45, 2, 0, 0, 0, 0, 0, 0, 0], &mut at).is_none());
    assert_eq!(at, 0);
    for bytes in [&[0xe0, 0, 0, 0][..], &[0xff, 0, 0, 0][..], &[0xc0, 1][..]] {
        let mut at = 0;
        assert!(compressed_integer(bytes, &mut at).is_none());
        assert_eq!(at, 0);
    }
}
#[test]
fn compressed_integer_boundaries() {
    for (bytes, value) in [
        (vec![0x7f], 0x7f),
        (vec![0x80, 0x80], 0x80),
        (vec![0xbf, 0xff], 0x3fff),
        (vec![0xc0, 0, 0x40, 0], 0x4000),
        (vec![0xdf, 0xff, 0xff, 0xff], 0x1fff_ffff),
    ] {
        let mut at = 0;
        assert_eq!(compressed_integer(&bytes, &mut at), Some(value));
        assert_eq!(at, bytes.len());
    }
    assert_eq!(signed_integer(&[0x7f], &mut 0), Some(-1));
    assert_eq!(signed_integer(&[0x01], &mut 0), Some(-64));
    assert_eq!(signed_integer(&[0x80, 0x01], &mut 0), Some(-8192));
}
