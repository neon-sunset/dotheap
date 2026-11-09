pub(super) fn encode(input: &[u8], output: &mut [u8], width: usize) {
    shuffle(input, output, width, false);
}

pub(super) fn decode(input: &[u8], output: &mut [u8], width: usize) {
    shuffle(input, output, width, true);
}

fn shuffle(input: &[u8], output: &mut [u8], width: usize, inverse: bool) {
    assert_eq!(input.len(), output.len());
    assert_eq!(input.len() % width, 0);
    let count = input.len() / width;
    #[cfg(target_arch = "aarch64")]
    let start = if matches!(width, 4 | 8) {
        // Each iteration reads and writes sixteen complete elements.
        unsafe { neon(input, output, width, inverse) }
    } else {
        0
    };
    #[cfg(not(target_arch = "aarch64"))]
    let start = 0;
    for byte in 0..width {
        for i in start..count {
            if inverse {
                output[i * width + byte] = input[byte * count + i];
            } else {
                output[byte * count + i] = input[i * width + byte];
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn neon(input: &[u8], output: &mut [u8], width: usize, inverse: bool) -> usize {
    use std::arch::aarch64::*;
    let count = input.len() / width;
    let end = count / 16 * 16;
    unsafe {
        for i in (0..end).step_by(16) {
            let src = input.as_ptr();
            let dst = output.as_mut_ptr();
            if width == 4 {
                if inverse {
                    let v = uint8x16x4_t(
                        vld1q_u8(src.add(i)),
                        vld1q_u8(src.add(count + i)),
                        vld1q_u8(src.add(count * 2 + i)),
                        vld1q_u8(src.add(count * 3 + i)),
                    );
                    vst4q_u8(dst.add(i * 4), v);
                } else {
                    let v = vld4q_u8(src.add(i * 4));
                    vst1q_u8(dst.add(i), v.0);
                    vst1q_u8(dst.add(count + i), v.1);
                    vst1q_u8(dst.add(count * 2 + i), v.2);
                    vst1q_u8(dst.add(count * 3 + i), v.3);
                }
            } else if inverse {
                let pairs: [(uint16x8_t, uint16x8_t); 4] = std::array::from_fn(|j| {
                    let a = vld1q_u8(src.add(count * (j * 2) + i));
                    let b = vld1q_u8(src.add(count * (j * 2 + 1) + i));
                    (
                        vreinterpretq_u16_u8(vzip1q_u8(a, b)),
                        vreinterpretq_u16_u8(vzip2q_u8(a, b)),
                    )
                });
                vst4q_u16(
                    dst.add(i * 8).cast(),
                    uint16x8x4_t(pairs[0].0, pairs[1].0, pairs[2].0, pairs[3].0),
                );
                vst4q_u16(
                    dst.add(i * 8 + 64).cast(),
                    uint16x8x4_t(pairs[0].1, pairs[1].1, pairs[2].1, pairs[3].1),
                );
            } else {
                let a = vld4q_u16(src.add(i * 8).cast());
                let b = vld4q_u16(src.add(i * 8 + 64).cast());
                for (j, (a, b)) in [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)]
                    .into_iter()
                    .enumerate()
                {
                    let a = vreinterpretq_u8_u16(a);
                    let b = vreinterpretq_u8_u16(b);
                    vst1q_u8(dst.add(count * (j * 2) + i), vuzp1q_u8(a, b));
                    vst1q_u8(dst.add(count * (j * 2 + 1) + i), vuzp2q_u8(a, b));
                }
            }
        }
    }
    end
}

pub(super) fn delta(input: &[u8], output: &mut [u8], width: usize) {
    match width {
        4 => delta_width::<4>(input, output),
        8 => delta_width::<8>(input, output),
        _ => unreachable!(),
    }
}

fn delta_width<const N: usize>(input: &[u8], output: &mut [u8]) {
    let mut previous = 0u64;
    for (value, out) in input
        .as_chunks::<N>()
        .0
        .iter()
        .zip(output.as_chunks_mut::<N>().0)
    {
        let mut bytes = [0; 8];
        bytes[..N].copy_from_slice(value);
        let value = u64::from_le_bytes(bytes);
        out.copy_from_slice(&value.wrapping_sub(previous).to_le_bytes()[..N]);
        previous = value;
    }
}

pub(super) fn undelta(bytes: &mut [u8], width: usize) {
    match width {
        4 => undelta_width::<4>(bytes),
        8 => undelta_width::<8>(bytes),
        _ => unreachable!(),
    }
}

fn undelta_width<const N: usize>(bytes: &mut [u8]) {
    let mut previous = 0u64;
    for value in bytes.as_chunks_mut::<N>().0 {
        let mut bytes = [0; 8];
        bytes[..N].copy_from_slice(value);
        previous = previous.wrapping_add(u64::from_le_bytes(bytes));
        value.copy_from_slice(&previous.to_le_bytes()[..N]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffled_bytes_match_scalar_layout() {
        for width in [1, 2, 4, 8, 24] {
            for count in [0, 1, 7, 15, 16, 17, 31, 32, 33, 1025] {
                let input: Vec<_> = (0..width * count).map(|i| (i * 13 + i / 7) as u8).collect();
                let mut encoded = vec![0; input.len()];
                let mut decoded = vec![0; input.len()];
                encode(&input, &mut encoded, width);
                for i in 0..count {
                    for byte in 0..width {
                        assert_eq!(encoded[byte * count + i], input[i * width + byte]);
                    }
                }
                decode(&encoded, &mut decoded, width);
                assert_eq!(decoded, input);
            }
        }
    }
}
