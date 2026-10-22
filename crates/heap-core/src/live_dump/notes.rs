use super::{
    CaptureError, Result,
    mappings::Mapping,
    thread_stop::{MACHINE, Registers},
};

pub(super) struct ProcessNotes {
    pub auxv: Vec<u8>,
    pub stat: String,
    pub status: String,
    pub command_line: Vec<u8>,
}

pub(super) fn notes(
    pid: i32,
    registers: &[Registers],
    mappings: &[Mapping],
    page_size: usize,
    process: &ProcessNotes,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut note = |kind: u32, bytes: &[u8]| -> Result<()> {
        let length = u32::try_from(bytes.len())
            .map_err(|_| CaptureError::terminal("the process notes are too large"))?;
        output.extend_from_slice(&5u32.to_le_bytes());
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(&kind.to_le_bytes());
        output.extend_from_slice(b"CORE\0\0\0\0");
        output.extend_from_slice(bytes);
        output.resize(output.len().next_multiple_of(4), 0);
        Ok(())
    };
    let after_name = process
        .stat
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let fields: Vec<_> = after_name.split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|s| s.as_bytes().first())
        .copied()
        .unwrap_or(b'R');
    let number = |index| {
        fields
            .get(index)
            .and_then(|s: &&str| s.parse::<i32>().ok())
            .unwrap_or(0)
    };
    let (ppid, pgrp, sid) = (number(1), number(2), number(3));
    let command = process
        .stat
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')').map(|(name, _)| name))
        .unwrap_or("");
    let id = |key| {
        process
            .status
            .lines()
            .find_map(|line| {
                line.strip_prefix(key)?
                    .split_whitespace()
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .unwrap_or(0)
    };
    for registers in registers {
        let mut data = vec![0; 112];
        put32(&mut data, 32, registers.tid as u32);
        put32(&mut data, 36, ppid as u32);
        put32(&mut data, 40, pgrp as u32);
        put32(&mut data, 44, sid as u32);
        data.extend_from_slice(&registers.general);
        data.extend_from_slice(&u32::from(registers.float.is_some()).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        note(1, &data)?;
        if let Some(float) = &registers.float {
            note(2, float)?;
        }
    }
    let mut data = vec![0; 136];
    data[0] = u8::from(state != b'R');
    data[1] = state;
    put32(&mut data, 16, id("Uid:"));
    put32(&mut data, 20, id("Gid:"));
    put32(&mut data, 24, pid as u32);
    put32(&mut data, 28, ppid as u32);
    put32(&mut data, 32, pgrp as u32);
    put32(&mut data, 36, sid as u32);
    for (i, &byte) in command.as_bytes().iter().take(15).enumerate() {
        data[40 + i] = byte;
    }
    for (i, &byte) in process.command_line.iter().take(79).enumerate() {
        data[56 + i] = if byte == 0 { b' ' } else { byte };
    }
    note(3, &data)?;
    if !process.auxv.is_empty() {
        note(6, &process.auxv)?;
    }
    let files: Vec<_> = mappings.iter().filter(|m| m.file_backed()).collect();
    let mut data = Vec::new();
    data.extend_from_slice(&(files.len() as u64).to_le_bytes());
    data.extend_from_slice(&(page_size as u64).to_le_bytes());
    for mapping in &files {
        data.extend_from_slice(&mapping.start.to_le_bytes());
        data.extend_from_slice(&mapping.end.to_le_bytes());
        data.extend_from_slice(&(mapping.offset / page_size as u64).to_le_bytes());
    }
    for mapping in files {
        data.extend_from_slice(mapping.path.as_bytes());
        data.push(0);
    }
    note(0x46494c45, &data)?;
    Ok(output)
}

pub(super) fn header(mappings: &[Mapping], notes: &[u8], page_size: usize) -> Result<Vec<u8>> {
    if mappings.len() >= 65534 {
        return Err(CaptureError::terminal(
            "the process has more than 65533 mappings",
        ));
    }
    let count = mappings.len() + 1;
    let notes_offset = 64 + 56 * count;
    let header_size = (notes_offset + notes.len()).next_multiple_of(page_size);
    let mut header = vec![0; header_size];
    header[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    put16(&mut header, 16, 4);
    put16(&mut header, 18, MACHINE);
    put32(&mut header, 20, 1);
    put64(&mut header, 32, 64);
    put16(&mut header, 52, 64);
    put16(&mut header, 54, 56);
    put16(&mut header, 56, count as u16);
    let mut program = |index, kind, flags, offset, address, size, align| {
        let at = 64 + index * 56;
        put32(&mut header, at, kind);
        put32(&mut header, at + 4, flags);
        put64(&mut header, at + 8, offset);
        put64(&mut header, at + 16, address);
        put64(&mut header, at + 32, size);
        put64(&mut header, at + 40, size);
        put64(&mut header, at + 48, align);
    };
    program(0, 4, 4, notes_offset as u64, 0, notes.len() as u64, 4);
    let mut offset = header_size as u64;
    for (i, mapping) in mappings.iter().enumerate() {
        let size = mapping.end - mapping.start;
        program(
            i + 1,
            1,
            mapping.flags(),
            offset,
            mapping.start,
            size,
            page_size as u64,
        );
        offset = offset
            .checked_add(size)
            .ok_or_else(|| CaptureError::terminal("the captured address space is too large"))?;
    }
    header[notes_offset..notes_offset + notes.len()].copy_from_slice(notes);
    Ok(header)
}

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
