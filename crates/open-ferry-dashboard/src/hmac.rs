//! HMAC-SHA256 (RFC 2104), which keys the ledger's hash of a client key
//! with a secret of the ledger's own, so that one ledger's IDs can't be
//! matched against another's, or against a table of hashes made in
//! advance. The secret is kept in the ledger, so whoever has the file can
//! still check it for a key they already know.

use sha2::{Digest, Sha256};

/// SHA-256's block size.
const BLOCK: usize = 64;

/// The HMAC-SHA256 of `message` under `key`.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest: [u8; 32] = Sha256::digest(key).into();
        for (slot, byte) in block.iter_mut().zip(digest) {
            *slot = byte;
        }
    } else {
        for (slot, byte) in block.iter_mut().zip(key) {
            *slot = *byte;
        }
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for ((inner, outer), byte) in inner_pad.iter_mut().zip(outer_pad.iter_mut()).zip(block) {
        *inner ^= byte;
        *outer ^= byte;
    }
    let inner: [u8; 32] = Sha256::new()
        .chain_update(inner_pad)
        .chain_update(message)
        .finalize()
        .into();
    Sha256::new()
        .chain_update(outer_pad)
        .chain_update(inner)
        .finalize()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Not upstream's: RFC 4231's test case 2, a key shorter than a block.
    #[test]
    fn short_key() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// Not upstream's: RFC 4231's test case 6, a key longer than a block,
    /// which is hashed first.
    #[test]
    fn long_key() {
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
