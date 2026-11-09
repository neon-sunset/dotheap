use super::{create_directory, persist, shuffle};
use crate::{Error, Result};
use memmap2::{MmapMut, MmapOptions};
use rayon::prelude::*;
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::Path,
};

const MAGIC: &[u8; 8] = b"DTHCOL01";
const HEADER: usize = 24;
const BLOCK: usize = 1 << 20;
const BATCH: usize = 64;

fn block_size(width: usize) -> Result<usize> {
    if width == 0 || width > BLOCK {
        return Err(Error::cache("invalid column element size"));
    }
    Ok(BLOCK / width * width)
}

struct Encoder {
    compressor: zstd::bulk::Compressor<'static>,
    transformed: Vec<u8>,
    differences: Vec<u8>,
}

impl Encoder {
    fn new() -> Result<Self> {
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
        Ok(Self {
            compressor,
            transformed: Vec::new(),
            differences: Vec::new(),
        })
    }

    fn encode(&mut self, bytes: &[u8], width: usize, delta: bool) -> Result<Vec<u8>> {
        self.transformed.resize(bytes.len() + 1, 0);
        let (mode, output) = self.transformed.split_at_mut(1);
        mode[0] = if matches!(width, 4 | 8) {
            if delta { 2 } else { 1 }
        } else {
            0
        };
        if mode[0] == 0 {
            output.copy_from_slice(bytes);
        } else {
            let input = if delta {
                self.differences.resize(bytes.len(), 0);
                shuffle::delta(bytes, &mut self.differences, width);
                &self.differences
            } else {
                bytes
            };
            shuffle::encode(input, output, width);
        }
        Ok(self.compressor.compress(&self.transformed)?)
    }
}

pub(super) fn write(path: &Path, bytes: &[u8], width: usize, delta: bool) -> Result<()> {
    let block = block_size(width)?;
    let blocks = bytes.len().div_ceil(block);
    let mut offsets = Vec::with_capacity(blocks + 1);
    let parent = path
        .parent()
        .ok_or_else(|| Error::cache("column has no parent folder"))?;
    create_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(MAGIC)?;
    file.write_all(&(width as u64).to_le_bytes())?;
    file.write_all(&(bytes.len() as u64).to_le_bytes())?;
    let mut offset = (HEADER + (blocks + 1) * 8) as u64;
    file.seek(SeekFrom::Start(offset))?;
    for batch in bytes.chunks(block * BATCH) {
        let encoded: Vec<_> = batch
            .par_chunks(block)
            .with_min_len(4)
            .map_init(Encoder::new, |encoder, bytes| {
                encoder
                    .as_mut()
                    .map_err(|e| e.clone())?
                    .encode(bytes, width, delta)
            })
            .collect();
        for bytes in encoded {
            let bytes = bytes?;
            offsets.push(offset);
            file.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
    }
    offsets.push(offset);
    file.seek(SeekFrom::Start(HEADER as u64))?;
    for offset in offsets {
        file.write_all(&offset.to_le_bytes())?;
    }
    file.as_file().sync_all()?;
    persist(file, path)
}

pub(super) fn read(path: &Path, width: usize) -> Result<(MmapMut, usize)> {
    let block = block_size(width)?;
    let file = File::open(path)?;
    if file.metadata()?.len() < (HEADER + 8) as u64 {
        return Err(Error::cache("truncated column header"));
    }
    // Columns are replaced atomically; the mapped inode stays immutable.
    let packed = unsafe { MmapOptions::new().map(&file)? };
    if &packed[..8] != MAGIC || u64_at(&packed, 8)? != width as u64 {
        return Err(Error::cache("invalid column header"));
    }
    let bytes =
        usize::try_from(u64_at(&packed, 16)?).map_err(|_| Error::cache("column is too large"))?;
    if bytes % width != 0 {
        return Err(Error::cache("invalid column length"));
    }
    let blocks = bytes.div_ceil(block);
    let index_end = blocks
        .checked_add(1)
        .and_then(|n| n.checked_mul(8))
        .and_then(|n| n.checked_add(HEADER))
        .filter(|&n| n <= packed.len())
        .ok_or_else(|| Error::cache("truncated column index"))?;
    let offsets = (0..=blocks)
        .map(|i| {
            usize::try_from(u64_at(&packed, HEADER + i * 8)?)
                .map_err(|_| Error::cache("invalid column offset"))
        })
        .collect::<Result<Vec<_>>>()?;
    if offsets[0] != index_end
        || offsets[blocks] != packed.len()
        || offsets.windows(2).any(|w| w[0] >= w[1])
    {
        return Err(Error::cache("invalid column index"));
    }
    for (i, offsets) in offsets.windows(2).enumerate() {
        let frame = &packed[offsets[0]..offsets[1]];
        let expected = (bytes - i * block).min(block) + 1;
        if zstd::zstd_safe::get_frame_content_size(frame)
            .ok()
            .flatten()
            != Some(expected as u64)
        {
            return Err(Error::cache("invalid column frame size"));
        }
    }
    let mut output = MmapOptions::new().len(bytes.max(1)).map_anon()?;
    output[..bytes]
        .par_chunks_mut(block * 8)
        .enumerate()
        .try_for_each(|(batch, output)| -> Result<()> {
            let mut decoder = zstd::bulk::Decompressor::new()?;
            let mut scratch = vec![0; block + 1];
            for (j, out) in output.chunks_mut(block).enumerate() {
                let i = batch * 8 + j;
                let frame = &packed[offsets[i]..offsets[i + 1]];
                let decoded = decoder.decompress_to_buffer(frame, &mut scratch[..out.len() + 1])?;
                if decoded != out.len() + 1 {
                    return Err(Error::cache("invalid column frame length"));
                }
                let input = &scratch[1..decoded];
                match scratch[0] {
                    0 => out.copy_from_slice(input),
                    mode @ (1 | 2) if matches!(width, 4 | 8) => {
                        shuffle::decode(input, out, width);
                        if mode == 2 {
                            shuffle::undelta(out, width);
                        }
                    }
                    _ => return Err(Error::cache("invalid column transform")),
                }
            }
            Ok(())
        })?;
    Ok((output, bytes / width))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| Error::cache("truncated column"))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}
