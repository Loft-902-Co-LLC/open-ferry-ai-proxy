//! Compact, deterministic exports preserve IDs and every random-case input.
//! JSON framing has no platform line endings; embedded input bytes, including
//! deliberate CR/LF characters, are preserved rather than normalized away.
use super::*;
use serde_json::json;
use std::sync::OnceLock;

pub type Digests = BTreeMap<String, String>;
pub const PROFILES: [(u64, usize); 2] = [(1, 25), (38, 5000)];

pub fn export(seed: u64, random: usize) -> Result<Vec<u8>> {
    let rows: Vec<_> = crate::generate::config_save::step_cases(seed, random)
        .into_iter()
        .map(|case| {
            // Deliberately exhaustive: adding a Case field must update this
            // export rather than silently omitting a new generator input.
            let crate::cases::Case {
                name,
                model,
                request,
                translated_request,
                events,
                options,
                known_difference,
            } = case;
            json!({
                "id": name,
                "model": model,
                "request": request,
                "translated_request": translated_request,
                "events": events,
                "options": options,
                "known_difference": known_difference,
            })
        })
        .collect();
    Ok(serde_json::to_vec(&rows)?)
}

pub fn digests() -> &'static Digests {
    static DIGESTS: OnceLock<Digests> = OnceLock::new();
    DIGESTS.get_or_init(|| {
        PROFILES
            .into_iter()
            .map(|(seed, random)| {
                let bytes = export(seed, random).expect("random cases serialize");
                let digest = Sha256::digest(bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                (format!("seed-{seed}-random-{random}"), digest)
            })
            .collect()
    })
}

pub(super) fn valid(digests: &Digests) -> bool {
    digests.len() == PROFILES.len()
        && PROFILES.into_iter().all(|(seed, random)| {
            digests
                .get(&format!("seed-{seed}-random-{random}"))
                .is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                })
        })
}
