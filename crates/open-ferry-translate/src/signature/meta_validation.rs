// Ported from CLIProxyAPI internal/signature/meta_validation.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Observed Meta Muse envelopes. Shape validation cannot prove decryptability.

use super::{Error, first_invalid_base64_char};
use crate::go::base64::RAW_URL;

pub const MAX_META_REASONING_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
const HEADER: &[u8] = &[0x43, 0xe3, 0xda, 0x0e, 0x01];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaReasoningSignatureInfo {
    pub decoded_len: usize,
    pub payload_len: usize,
    pub bucket_exponent: usize,
    pub router_suffix: String,
}

pub fn is_valid_meta_reasoning_signature(raw: &str) -> bool {
    inspect_meta_reasoning_signature(raw).is_ok()
}

/// Checks canonical unpadded base64url, the observed header, and a decoded
/// length of 45 + 1024 * 2^k. No cache prefix is recognized for Meta.
pub fn inspect_meta_reasoning_signature(raw: &str) -> Result<MetaReasoningSignatureInfo, Error> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err(error!("empty Meta reasoning signature"));
    }
    if sig.len() > MAX_META_REASONING_SIGNATURE_LEN {
        return Err(error!(
            "Meta reasoning signature exceeds maximum length ({MAX_META_REASONING_SIGNATURE_LEN} bytes)"
        ));
    }
    if !sig.starts_with("Q-PaDg") {
        return Err(error!(
            "invalid Meta reasoning signature: expected Q-PaDg prefix"
        ));
    }
    let (sig, suffix) = match sig.split_once('.') {
        Some((head, tail)) => {
            if !valid_router_suffix(tail) {
                return Err(error!("invalid Meta reasoning signature: router suffix must be 1-128 alphanumeric characters"));
            }
            (head, tail)
        }
        None => (sig, ""),
    };
    if sig.len() % 4 == 1 {
        return Err(error!("invalid Meta reasoning signature: base64url length {} cannot decode", sig.len()));
    }
    let decoded_len = sig.len() / 4 * 3 + (sig.len() % 4).saturating_sub(1);
    let payload_len = decoded_len as isize - 45;
    if payload_len < 1024 || !(payload_len as usize).is_power_of_two() {
        return Err(error!("invalid Meta reasoning signature: payload length {payload_len} is not a power-of-two bucket"));
    }
    if let Some((index, c)) = first_invalid_base64_char(sig, b"-_") {
        return Err(error!("invalid Meta reasoning signature: contains non-base64url character U+{:04X} at byte {index}", u32::from(c)));
    }
    let decoded = RAW_URL
        .strict()
        .decode(sig)
        .map_err(|_| error!("invalid Meta reasoning signature: base64url decode failed"))?;
    if decoded.len() < 45 + 1024 {
        return Err(error!(
            "invalid Meta reasoning signature: decoded payload too short"
        ));
    }
    if !decoded.starts_with(HEADER) {
        let header: String = decoded[..HEADER.len()]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        return Err(error!(
            "invalid Meta reasoning signature: unexpected envelope header {header}"
        ));
    }
    let payload_len = decoded.len() - 45;
    if !payload_len.is_power_of_two() {
        return Err(error!(
            "invalid Meta reasoning signature: payload length {payload_len} is not a power-of-two bucket"
        ));
    }
    Ok(MetaReasoningSignatureInfo {
        router_suffix: suffix.to_owned(),
        decoded_len: decoded.len(),
        payload_len,
        bucket_exponent: payload_len.ilog2() as usize - 10,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    #[test]
    fn observed_buckets_and_replay_family() {
        for exponent in 0..5 {
            let mut bytes = vec![0; 45 + (1024 << exponent)];
            bytes[..HEADER.len()].copy_from_slice(HEADER);
            let sig = URL_SAFE_NO_PAD.encode(&bytes);
            let info = inspect_meta_reasoning_signature(&sig).unwrap();
            assert_eq!(info.bucket_exponent, exponent);
            assert_eq!(info.decoded_len, bytes.len());
            assert_eq!(
                super::super::detect_signature_provider(&sig),
                super::super::Provider::Meta
            );
            for target in [
                super::super::Provider::Gpt,
                super::super::Provider::Claude,
                super::super::Provider::Meta,
            ] {
                assert_eq!(
                    super::super::is_signature_compatible_with_provider(target, &sig),
                    target == super::super::Provider::Meta
                );
            }
            bytes[4] = 2;
            assert!(inspect_meta_reasoning_signature(&URL_SAFE_NO_PAD.encode(bytes)).is_err());
            assert!(inspect_meta_reasoning_signature(&format!("{sig}=")).is_err());
            assert!(inspect_meta_reasoning_signature(&format!("meta#{sig}")).is_err());
        }
    }

    #[test]
    fn router_suffix_is_bounded_and_native_shape_is_still_checked() {
        let native = format!("Q-PaDgE{}", "A".repeat(1419));
        for suffix in ["A", "Router123", &"x".repeat(128)] {
            assert_eq!(inspect_meta_reasoning_signature(&format!("{native}.{suffix}")).unwrap().router_suffix, suffix);
        }
        for suffix in ["", "a.b", "a-b", "a_b", "é", &"x".repeat(129)] {
            assert!(inspect_meta_reasoning_signature(&format!("{native}.{suffix}")).is_err());
        }
        assert!(inspect_meta_reasoning_signature("Q-PaDgE.Router").is_err());
    }

    #[test]
    fn rejects_noncanonical_and_nonbucket_values() {
        for len in [44, 45, 1068, 1070, 45 + 3072] {
            let mut bytes = vec![0; len];
            bytes[..HEADER.len()].copy_from_slice(HEADER);
            assert!(inspect_meta_reasoning_signature(&URL_SAFE_NO_PAD.encode(bytes)).is_err());
        }
        let mut bytes = vec![0; 1069];
        bytes[..HEADER.len()].copy_from_slice(HEADER);
        let mut sig = URL_SAFE_NO_PAD.encode(bytes);
        sig.pop();
        sig.push('B');
        assert!(inspect_meta_reasoning_signature(&sig).is_err());
    }
}

pub(super) fn valid_router_suffix(suffix: &str) -> bool {
    !suffix.is_empty() && suffix.len() <= 128 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
}
