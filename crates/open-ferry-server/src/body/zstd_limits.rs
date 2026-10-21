//! Allocation bounds for the locked ruzstd 0.9.1 decoder.
use super::DecodeError;

const BLOCK: usize = 128 << 10;
const MAX_LITERALS: usize = (1 << 20) - 1;
const FIXED_SCRATCH: usize = 6 << 20;

/// ruzstd 0.9.1 source audit (paths relative to its src):
/// - decoding/ringbuffer.rs:75-123 rounds capacity by `ring_capacity` and
///   allocates the replacement before freeing the previous allocation.
/// - decoding/decode_buffer.rs:217-224 retains at most the window after drain.
///   Valid blocks add at most BLOCK. Malformed zero-sequence literals can add
///   MAX_LITERALS (blocks/literals_section.rs:131-153;
///   decoding/block_decoder.rs:188-206). Sequence execution checks after each
///   repeat (decoding/sequence_execution.rs:12-53), but the largest ML is
///   131074, so its worst overshoot fits BLOCK + MAX_LITERALS too.
/// - Fixed scratch includes 3 MiB for a 20-bit literals Vec plus realloc
///   overlap; 2.25 MiB for up to 98047 twelve-byte sequences with geometric
///   growth/overlap (blocks/sequence_section.rs:21-29,118-146;
///   decoding/sequence_section_decoder.rs:69-70,168-169); 384 KiB for the
///   128 KiB block-content Vec and overlap (decoding/block_decoder.rs:115-120);
///   and 64 KiB for bounded Huffman/FSE tables and metadata
///   (huff0/huff0_decoder.rs:229-255,288-345;
///   fse/fse_decoder.rs:145-161). Their sum is below 6 MiB.
///   `validate_block` bounds FSE probability counts *before* ruzstd's unchecked
///   zero-run resize at fse/fse_decoder.rs:269-280. Without that validation the
///   fixed table allowance would not cover malformed input.
///
/// The reservation never shrinks between frames, covering retained scratch.
pub(super) fn working_bytes(window: usize) -> usize {
    ring_capacity(window + BLOCK) + ring_capacity(window + BLOCK + MAX_LITERALS) + FIXED_SCRATCH
}

fn ring_capacity(required: usize) -> usize {
    let slack = 2 * BLOCK;
    if required <= slack {
        required.next_power_of_two() + 1
    } else {
        (required - slack).next_power_of_two() + slack + 1
    }
}

fn malformed() -> DecodeError {
    DecodeError::Failed(
        "failed to decode zstd request body: malformed frame or entropy header".into(),
    )
}

fn little(input: &[u8], offset: usize, len: usize) -> Result<u64, DecodeError> {
    let bytes = input.get(offset..offset + len).ok_or_else(malformed)?;
    Ok(bytes.iter().enumerate().fold(0, |value, (index, byte)| {
        value | (u64::from(*byte) << (8 * index))
    }))
}

/// FrameDecoder's header module is crate-private in 0.9.1. Read only these
/// scalar fields without allocation, matching decoding/frame.rs:6-84,116-132,
/// before init may allocate. `None` leaves skippable handling to FrameDecoder.
pub(super) fn frame_window(input: &[u8]) -> Result<Option<usize>, DecodeError> {
    let magic = little(input, 0, 4)?;
    if (0x184d2a50..=0x184d2a5f).contains(&magic) {
        return Ok(None);
    }
    if magic != 0xfd2fb528 {
        return Err(malformed());
    }
    let descriptor = *input.get(4).ok_or_else(malformed)?;
    let single = descriptor & 0x20 != 0;
    let dictionary_size = [0, 1, 2, 4][usize::from(descriptor & 3)];
    let content_size_len = match descriptor >> 6 {
        0 => usize::from(single),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let content_offset = 5 + usize::from(!single) + dictionary_size;
    let content_size = little(input, content_offset, content_size_len)?
        + if content_size_len == 2 { 256 } else { 0 };
    let window = if single {
        content_size
    } else {
        let wd = *input.get(5).ok_or_else(malformed)?;
        let base = 1u64 << (10 + (wd >> 3));
        base + (base / 8) * u64::from(wd & 7)
    };
    Ok(Some(
        usize::try_from(window).map_err(|_| DecodeError::TooLarge)?,
    ))
}

struct Bits<'a> {
    input: &'a [u8],
    position: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32, DecodeError> {
        let mut value = 0;
        for bit in 0..count {
            let offset = self.position + bit;
            value |=
                u32::from((self.input.get(offset / 8).ok_or_else(malformed)? >> (offset % 8)) & 1)
                    << bit;
        }
        self.position += count;
        Ok(value)
    }
}

/// Nonallocating equivalent of fse/fse_decoder.rs:225-302, with the symbol
/// bound checked before every zero-run growth instead of after the table.
fn fse_header(input: &[u8], max_log: u32, max_symbol: usize) -> Result<usize, DecodeError> {
    let mut bits = Bits { input, position: 0 };
    let log = bits.read(4)? + 5;
    if log > max_log {
        return Err(malformed());
    }
    let total = 1u32 << log;
    let mut sum = 0;
    let mut symbols = 0;
    while sum < total {
        let remaining = total - sum + 1;
        let count = (32 - remaining.leading_zeros()) as usize;
        let unchecked = bits.read(count)?;
        let threshold = ((1u32 << count) - 1) - remaining;
        let mask = (1u32 << (count - 1)) - 1;
        let small = unchecked & mask;
        let value = if small < threshold {
            bits.position -= 1;
            small
        } else if unchecked > mask {
            unchecked - threshold
        } else {
            unchecked
        };
        symbols += 1;
        if symbols > max_symbol + 1 {
            return Err(malformed());
        }
        if value == 1 {
            loop {
                let skip = bits.read(2)? as usize;
                symbols += skip;
                if symbols > max_symbol + 1 {
                    return Err(malformed());
                }
                if skip != 3 {
                    break;
                }
            }
        } else {
            sum += if value == 0 { 1 } else { value - 1 };
        }
    }
    if sum != total {
        return Err(malformed());
    }
    Ok(bits.position.div_ceil(8))
}

/// Preflight only allocation-driving entropy headers; ruzstd still performs
/// the actual decoding, history, integrity and complete format validation.
pub(super) fn validate_block(input: &[u8]) -> Result<(), DecodeError> {
    let block_header = little(input, 0, 3)?;
    if (block_header >> 1) & 3 != 2 {
        return Ok(());
    }
    let size = (block_header >> 3) as usize;
    let raw = input.get(3..3 + size).ok_or_else(malformed)?;
    let first = *raw.first().ok_or_else(malformed)?;
    let kind = first & 3;
    let format = (first >> 2) & 3;
    let (header_len, encoded) = if kind < 2 {
        let len = match format {
            0 | 2 => 1,
            1 => 2,
            _ => 3,
        };
        let regenerated = little(raw, 0, len)? >> if len == 1 { 3 } else { 4 };
        (len, if kind == 1 { 1 } else { regenerated as usize })
    } else {
        let (len, width) = match format {
            0 | 1 => (3, 10),
            2 => (4, 14),
            _ => (5, 18),
        };
        (len, (little(raw, 0, len)? >> (4 + width)) as usize)
    };
    let literals = raw
        .get(header_len..header_len + encoded)
        .ok_or_else(malformed)?;
    if kind == 2
        && let Some(&length) = literals.first()
        && length < 128
    {
        let header = literals
            .get(1..1 + usize::from(length))
            .ok_or_else(malformed)?;
        fse_header(header, 6, 255)?;
    }
    let sequences = raw.get(header_len + encoded..).ok_or_else(malformed)?;
    let first = *sequences.first().ok_or_else(malformed)?;
    if first == 0 {
        return Ok(());
    }
    let count_len = if first < 128 {
        1
    } else if first < 255 {
        2
    } else {
        3
    };
    let modes = *sequences.get(count_len).ok_or_else(malformed)?;
    let mut source = sequences.get(count_len + 1..).ok_or_else(malformed)?;
    for (shift, log, symbol) in [(6, 9, 35), (4, 8, 31), (2, 9, 52)] {
        let used = match (modes >> shift) & 3 {
            1 => 1,
            2 => fse_header(source, log, symbol)?,
            _ => 0,
        };
        source = source.get(used..).ok_or_else(malformed)?;
    }
    Ok(())
}
