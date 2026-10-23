//! Lowercase hexadecimal for request fingerprints and replay identifiers.

/// Encodes every byte, including leading zeroes, with one output allocation.
pub fn lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 15)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Previous per-byte formatting, retained as the output oracle.
    fn old_lower(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn matches_format_for_every_byte_and_generated_inputs() {
        for byte in 0..=255 {
            assert_eq!(lower(&[byte]), old_lower(&[byte]));
        }
        assert_eq!(
            lower(&(0..=255).collect::<Vec<_>>()),
            old_lower(&(0..=255).collect::<Vec<_>>())
        );
        assert_eq!(lower(&[]), "");
        let mut seed = 0x8957_3419_ade2_f037_u64;
        for length in 0..1024 {
            let bytes = (0..length % 257)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 32) as u8
                })
                .collect::<Vec<_>>();
            assert_eq!(lower(&bytes), old_lower(&bytes));
        }
    }
}
