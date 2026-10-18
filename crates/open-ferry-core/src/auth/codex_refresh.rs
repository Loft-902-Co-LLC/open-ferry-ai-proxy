// Ported from CLIProxyAPI internal/auth/codex/refresh_patch.go and
// sdk/cliproxy/auth/codex_persist.go (v8.0.32, MIT).
//! Independent, version-checked Codex token partitions and save intents.

use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde_json::{Map, Value};
use uuid::Uuid;

use super::Auth;

pub(crate) const OUTER_KEYS: &[&str] = &[
    "id_token",
    "access_token",
    "refresh_token",
    "account_id",
    "last_refresh",
    "email",
    "expired",
    "plan_type",
];
pub(crate) const SIWC_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "client_id",
    "device_id",
    "expired",
    "last_refresh",
    "scope",
    "scopes",
];
const IDENTITY_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "account_id",
    "client_id",
    "device_id",
];

/// Generates the device UUID stored only in `siwc.device_id`. Never log it.
pub fn generate_device_id() -> String {
    Uuid::new_v4().hyphenated().to_string()
}

/// Validates an existing v4 UUID and returns lowercase hyphenated text.
/// Rebind callers reuse this value, or generate a new ID if absent/invalid.
pub fn validate_device_id(value: &str) -> Option<String> {
    let id = Uuid::parse_str(value.trim()).ok()?;
    (id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122)
        .then(|| id.hyphenated().to_string())
}

/// Parses upstream's boolean representations of the SIWC switch.
pub fn parse_use_siwc(value: Option<&Value>) -> Option<bool> {
    value.and_then(super::parse_bool_any)
}

/// Whether the record represents Codex, including a JSON-only record.
pub fn is_codex_auth(auth: &Auth) -> bool {
    auth.provider.trim().eq_ignore_ascii_case("codex")
        || auth
            .metadata_str("type")
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("codex"))
}

/// A transient save intent. Never insert it into credential metadata.
#[derive(Clone, Default, PartialEq)]
pub enum CodexPersistIntent {
    /// Saves settings, including deletions, while protecting disk tokens.
    #[default]
    Configuration,
    /// Explicit login replaces outer tokens and keeps SIWC and disk settings.
    OuterLogin,
    /// Explicit bind replaces only SIWC and enables it.
    SiwcBind(Map<String, Value>),
    /// Writes only the partitions whose baseline still matches.
    Refresh(CredentialRefreshPatch),
}

impl CodexPersistIntent {
    pub(crate) fn same_operation(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Refresh(a), Self::Refresh(b)) => a.same_operation(b),
            _ => self == other,
        }
    }

    /// The Refresh patch, if this intent is one.
    pub(crate) fn refresh_patch(&self) -> Option<&CredentialRefreshPatch> {
        match self {
            Self::Refresh(patch) => Some(patch),
            _ => None,
        }
    }
}

/// Which Codex partitions an outcome names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CodexPartitions {
    pub(crate) outer: bool,
    pub(crate) siwc: bool,
}

impl CodexPartitions {
    pub(crate) fn any(self) -> bool {
        self.outer || self.siwc
    }
}

impl fmt::Debug for CodexPersistIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Configuration",
            Self::OuterLogin => "OuterLogin",
            Self::SiwcBind(_) => "SiwcBind([redacted])",
            Self::Refresh(_) => "Refresh([redacted])",
        })
    }
}

#[derive(Clone, PartialEq)]
struct PartitionPatch {
    before: Map<String, Value>,
    disk_before: Option<Map<String, Value>>,
    accepted_baselines: Vec<Map<String, Value>>,
    intermediate: Vec<Map<String, Value>>,
    after: Map<String, Value>,
}

fn fields(map: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| map.get(*key).map(|v| ((*key).into(), v.clone())))
        .collect()
}

pub(crate) fn same_version(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    IDENTITY_KEYS
        .iter()
        .all(|key| a.get(*key).unwrap_or(&Value::Null) == b.get(*key).unwrap_or(&Value::Null))
}

/// Retain every possible disk identity without eviction while marked, folding
/// its variants to their agreement. Each set has one entry per distinct identity;
/// clearing the marker empties it. Patches keep snapshot copies, so aggregate
/// history storage across failed saves can still be quadratic.
pub(crate) fn record_durable_baseline(
    baselines: &mut Vec<Map<String, Value>>,
    baseline: &Map<String, Value>,
) {
    if let Some(known) = baselines
        .iter_mut()
        .find(|known| same_version(known, baseline))
    {
        known.retain(|key, value| baseline.get(key) == Some(value));
        return;
    }
    baselines.push(baseline.clone());
}

fn nested(map: &Map<String, Value>) -> Map<String, Value> {
    map.get("siwc")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Per partition: the token identity on `disk` differs from `live`. Identity
/// inequality only: predecessor protection comes from the pending and undurable
/// overlays that build the disk the caller passes.
pub(crate) fn disk_rotated_partitions(
    live: &Map<String, Value>,
    disk: &Map<String, Value>,
) -> CodexPartitions {
    CodexPartitions {
        outer: !same_version(live, disk),
        siwc: !same_version(&nested(live), &nested(disk)),
    }
}

impl PartitionPatch {
    fn new(
        before: &Map<String, Value>,
        disk: Option<&Map<String, Value>>,
        after: &Map<String, Value>,
        keys: &[&str],
    ) -> Self {
        Self {
            before: fields(before, keys),
            disk_before: disk.map(|d| fields(d, keys)).filter(|d| !d.is_empty()),
            accepted_baselines: Vec::new(),
            intermediate: Vec::new(),
            after: fields(after, keys),
        }
    }

    fn matches_after(&self, target: &Map<String, Value>) -> bool {
        !self.after.is_empty()
            && (same_version(target, &self.after)
                || self
                    .disk_before
                    .iter()
                    .chain(self.accepted_baselines.iter())
                    .chain(std::iter::once(&self.before))
                    .chain(self.intermediate.iter())
                    .any(|baseline| {
                        IDENTITY_KEYS.iter().all(|key| {
                            target.get(*key).unwrap_or(&Value::Null)
                                == self
                                    .after
                                    .get(*key)
                                    .or_else(|| baseline.get(*key))
                                    .unwrap_or(&Value::Null)
                        })
                    }))
    }

    /// `target` shows this patch's result: its identity, and nothing `apply`
    /// would still write. An operation that kept its tokens and changed only,
    /// say, the expiry has the unwritten file's identity, so identity alone
    /// cannot tell. Nor can the values it changed from its own baseline:
    /// `apply` also writes a value it kept where the locked file differs.
    fn shows_result(&self, target: &Map<String, Value>) -> bool {
        let mut applied = target.clone();
        self.matches_after(target) && self.apply(&mut applied) && applied == *target
    }

    fn baseline<'a>(
        &'a self,
        target: &Map<String, Value>,
    ) -> Option<(&'a Map<String, Value>, usize, bool)> {
        // Sparse writes retain omitted baseline fields. Recognize the effective
        // result only as a fallback, as upstream checks after last.
        self.disk_before
            .as_ref()
            .filter(|d| same_version(target, d))
            .or_else(|| {
                self.accepted_baselines
                    .iter()
                    .find(|baseline| same_version(target, baseline))
            })
            .or_else(|| same_version(target, &self.before).then_some(&self.before))
            .map(|baseline| (baseline, 0, false))
            .or_else(|| {
                self.intermediate
                    .iter()
                    .enumerate()
                    .find(|(_, baseline)| same_version(target, baseline))
                    .map(|(i, baseline)| (baseline, i + 1, false))
            })
            .or_else(|| {
                self.matches_after(target)
                    .then_some((&self.after, self.intermediate.len(), true))
            })
    }

    fn apply(&self, target: &mut Map<String, Value>) -> bool {
        let Some((before, later, landed)) = self.baseline(target) else {
            return false;
        };
        // Only the final after-only fallback proves this patch already landed.
        // Earlier baseline matches still apply F6, even for equal identities.
        if landed {
            return true;
        }
        // Absent fields are not resurrected. Leaving a field untouched requires
        // agreement across the matched version's variants, own before and every
        // later intermediate; identity acceptance alone cannot hide a change.
        for (key, value) in &self.after {
            if before.get(key) != Some(value)
                || self.before.get(key) != Some(value)
                || self
                    .accepted_baselines
                    .iter()
                    .chain(self.disk_before.iter())
                    .filter(|baseline| same_version(baseline, before))
                    .any(|baseline| baseline.get(key) != Some(value))
                || self.intermediate[later..]
                    .iter()
                    .any(|baseline| baseline.get(key) != Some(value))
            {
                target.insert(key.clone(), value.clone());
            }
        }
        true
    }
}

/// Immutable successful rotations with independent token-identity baselines.
/// Timestamps are not versions. `Debug` never exposes tokens or device IDs.
#[derive(Clone, Default)]
pub struct CredentialRefreshPatch {
    written: Arc<AtomicBool>,
    outer: Option<PartitionPatch>,
    siwc: Option<PartitionPatch>,
}

// Value equality is for patch-content tests, never operation consumption.
impl PartialEq for CredentialRefreshPatch {
    fn eq(&self, other: &Self) -> bool {
        self.outer == other.outer && self.siwc == other.siwc
    }
}

impl fmt::Debug for CredentialRefreshPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialRefreshPatch")
            .field("outer", &self.outer.is_some())
            .field("siwc", &self.siwc.is_some())
            .finish()
    }
}

/// Which requested partitions still matched the locked disk snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchApplied {
    /// `None` when outer was not requested, `false` when disk was newer.
    pub outer: Option<bool>,
    /// `None` when SIWC was not requested, `false` when disk was newer.
    pub siwc: Option<bool>,
}

impl CredentialRefreshPatch {
    pub(crate) fn same_operation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.written, &other.written)
    }

    pub(crate) fn is_written(&self) -> bool {
        self.written.load(Ordering::Acquire)
    }

    pub(crate) fn mark_written(&self) {
        self.written.store(true, Ordering::Release);
    }

    /// Compose only the save clone, oldest pending operation first.
    /// An unchanged-metadata Replace can leave the live Refresh intent in
    /// pending too, so exclude our own operation. Consumers walking pending
    /// plus the intent must count each operation only once.
    pub(crate) fn with_pending_operations(&self, pending: &[Self]) -> Self {
        let older = pending
            .iter()
            .filter(|patch| !patch.same_operation(self))
            .cloned()
            .reduce(|older, newer| newer.with_pending(Some(&older)));
        self.with_pending(older.as_ref())
    }

    /// Captures only partitions actually refreshed successfully.
    pub fn new(
        before: &Map<String, Value>,
        after: &Map<String, Value>,
        outer: bool,
        siwc: bool,
    ) -> Self {
        Self::with_disk_baseline(before, None, after, outer, siwc)
    }

    /// Also captures the disk baseline read under the credential lock.
    pub fn with_disk_baseline(
        before: &Map<String, Value>,
        disk: Option<&Map<String, Value>>,
        after: &Map<String, Value>,
        outer: bool,
        siwc: bool,
    ) -> Self {
        Self {
            written: Arc::new(AtomicBool::new(false)),
            outer: outer.then(|| PartitionPatch::new(before, disk, after, OUTER_KEYS)),
            siwc: siwc.then(|| {
                PartitionPatch::new(
                    &nested(before),
                    disk.map(nested).as_ref(),
                    &nested(after),
                    SIWC_KEYS,
                )
            }),
        }
    }

    /// Whether this patch includes a successful refresh of the partition.
    pub(crate) fn includes_partition(&self, partition: super::CodexCredentialPartition) -> bool {
        match partition {
            super::CodexCredentialPartition::Outer => self.outer.is_some(),
            super::CodexCredentialPartition::Siwc => self.siwc.is_some(),
        }
    }

    /// The partitions this operation rotated.
    pub(crate) fn requested(&self) -> CodexPartitions {
        CodexPartitions {
            outer: self.outer.is_some(),
            siwc: self.siwc.is_some(),
        }
    }

    /// Per requested partition: `disk` shows this patch's result, its identity
    /// and the values it changed. Outer is read on the top level, SIWC on the
    /// nested object. A partition not requested is false.
    pub(crate) fn landed_in(&self, disk: &Map<String, Value>) -> CodexPartitions {
        CodexPartitions {
            outer: self.outer.as_ref().is_some_and(|p| p.shows_result(disk)),
            siwc: self
                .siwc
                .as_ref()
                .is_some_and(|p| p.shows_result(&nested(disk))),
        }
    }

    /// All proven baselines of the partition's chain, oldest attached first.
    pub(crate) fn durable_baselines(
        &self,
        partition: super::CodexCredentialPartition,
    ) -> Vec<Map<String, Value>> {
        let patch = match partition {
            super::CodexCredentialPartition::Outer => self.outer.as_ref(),
            super::CodexCredentialPartition::Siwc => self.siwc.as_ref(),
        };
        let Some(patch) = patch else {
            return Vec::new();
        };
        let mut baselines = Vec::new();
        for baseline in patch
            .accepted_baselines
            .iter()
            .chain(patch.disk_before.iter())
            .chain(std::iter::once(&patch.before))
            .chain(patch.intermediate.iter())
            .chain(std::iter::once(&patch.after))
        {
            record_durable_baseline(&mut baselines, baseline);
        }
        // An admitted write may still land after discard, including sparse
        // effective results retaining baseline fields omitted from after.
        for baseline in baselines.clone() {
            let mut written = baseline;
            if patch.apply(&mut written) {
                record_durable_baseline(&mut baselines, &written);
            }
        }
        baselines
    }

    /// Manager-owned discard baselines supplement provider-created patches.
    /// Keep the operation identity and intermediate versions shared with clones.
    pub(crate) fn with_durable_baselines(
        &self,
        outer: &[Map<String, Value>],
        siwc: &[Map<String, Value>],
    ) -> Self {
        let mut patch = self.clone();
        for (partition, baselines) in [(&mut patch.outer, outer), (&mut patch.siwc, siwc)] {
            if let Some(partition) = partition {
                for baseline in baselines {
                    record_durable_baseline(&mut partition.accepted_baselines, baseline);
                }
            }
        }
        patch
    }

    /// Retain each partition whose latest following result still matches the
    /// live identity. Pruning never changes the clone-shared operation identity.
    pub(crate) fn pruned_after_following(
        &self,
        following: &[Self],
        current: &Map<String, Value>,
    ) -> Option<Self> {
        let mut retained = self.clone();
        if let Some(candidate) = &self.outer {
            let latest = following
                .iter()
                .rev()
                .find_map(|patch| patch.outer.as_ref())
                .unwrap_or(candidate);
            if !latest.matches_after(current) {
                retained.outer = None;
            }
        }
        if let Some(candidate) = &self.siwc {
            let latest = following
                .iter()
                .rev()
                .find_map(|patch| patch.siwc.as_ref())
                .unwrap_or(candidate);
            if !latest.matches_after(&nested(current)) {
                retained.siwc = None;
            }
        }
        (retained.outer.is_some() || retained.siwc.is_some()).then_some(retained)
    }

    /// Whether every captured rotation is still the live token identity.
    /// Settings-only edits keep pending rotations; explicit token changes drop them.
    pub fn matches_after(&self, current: &Map<String, Value>) -> bool {
        self.outer.as_ref().is_none_or(|p| p.matches_after(current))
            && self
                .siwc
                .as_ref()
                .is_none_or(|p| p.matches_after(&nested(current)))
    }

    /// Carries pending rotations into a later refresh. Sequential rotations of
    /// one partition keep its original disk baseline and the composed result.
    pub fn with_pending(&self, pending: Option<&Self>) -> Self {
        fn combine(
            newer: &Option<PartitionPatch>,
            older: &Option<PartitionPatch>,
        ) -> Option<PartitionPatch> {
            match (newer, older) {
                (Some(newer), Some(older)) if older.matches_after(&newer.before) => {
                    let mut intermediate = older.intermediate.clone();
                    for baseline in std::iter::once(&newer.before)
                        .chain(newer.disk_before.iter())
                        .chain(newer.intermediate.iter())
                    {
                        if !intermediate.contains(baseline) {
                            intermediate.push(baseline.clone());
                        }
                    }
                    let mut after = older.after.clone();
                    after.retain(|key, value| {
                        newer.after.contains_key(key)
                            || if IDENTITY_KEYS.contains(&key.as_str()) {
                                newer.before.get(key).unwrap_or(&Value::Null) == value
                            } else {
                                newer.before.get(key) == Some(value)
                            }
                    });
                    after.extend(newer.after.clone());
                    let mut accepted_baselines = older.accepted_baselines.clone();
                    for baseline in &newer.accepted_baselines {
                        record_durable_baseline(&mut accepted_baselines, baseline);
                    }
                    Some(PartitionPatch {
                        before: older.before.clone(),
                        disk_before: older.disk_before.clone(),
                        accepted_baselines,
                        intermediate,
                        after,
                    })
                }
                (Some(newer), _) => Some(newer.clone()),
                (None, older) => older.clone(),
            }
        }
        let Some(pending) = pending else {
            return self.clone();
        };
        Self {
            written: self.written.clone(),
            outer: combine(&self.outer, &pending.outer),
            siwc: combine(&self.siwc, &pending.siwc),
        }
    }

    /// Whether at least one requested partition can still apply.
    pub fn can_apply_to(&self, disk: &Map<String, Value>) -> bool {
        self.outer
            .as_ref()
            .is_some_and(|p| p.baseline(disk).is_some())
            || self
                .siwc
                .as_ref()
                .is_some_and(|p| p.baseline(&nested(disk)).is_some())
    }

    /// Applies to a fresh disk/live snapshot, preserving settings and the switch.
    /// Chained application is idempotent unless a token identity returns to an
    /// earlier value, allowing a later application to match an earlier baseline.
    pub fn apply(&self, disk: &Map<String, Value>) -> (Map<String, Value>, PatchApplied) {
        let mut merged = disk.clone();
        let outer = self.outer.as_ref().map(|p| p.apply(&mut merged));
        let siwc = self.siwc.as_ref().map(|p| {
            let mut object = nested(disk);
            let applied = p.apply(&mut object);
            if applied {
                merged.insert("siwc".into(), Value::Object(object));
            }
            applied
        });
        (merged, PatchApplied { outer, siwc })
    }
}

pub(crate) fn is_credential_key(key: &str) -> bool {
    matches!(key, "siwc" | "use_siwc") || OUTER_KEYS.contains(&key)
}

/// Makes each withheld partition of `merged` exactly `disk`'s: for outer, every
/// `OUTER_KEYS` key takes disk's value, and a key disk lacks is removed; for
/// SIWC, the `siwc` object is disk's, and removed when disk has none. No other
/// key is touched (`use_siwc` stays the merge's). Pure; no I/O.
pub(crate) fn constrain_withheld(
    merged: &mut Map<String, Value>,
    disk: &Map<String, Value>,
    withheld: super::WithheldPartitions,
) {
    if withheld.outer {
        for key in OUTER_KEYS {
            match disk.get(*key) {
                Some(value) => {
                    merged.insert((*key).into(), value.clone());
                }
                None => {
                    merged.shift_remove(*key);
                }
            }
        }
    }
    if withheld.siwc {
        match disk.get("siwc") {
            Some(siwc) => {
                merged.insert("siwc".into(), siwc.clone());
            }
            None => {
                merged.shift_remove("siwc");
            }
        }
    }
}

/// Whether `map` (top level, or the nested `siwc` object for SIWC) holds the
/// partition: some `IDENTITY_KEYS` key is a non-empty string.
pub(crate) fn holds_partition(
    map: &Map<String, Value>,
    partition: super::CodexCredentialPartition,
) -> bool {
    let nested_map;
    let map = match partition {
        super::CodexCredentialPartition::Outer => map,
        super::CodexCredentialPartition::Siwc => {
            nested_map = nested(map);
            &nested_map
        }
    };
    IDENTITY_KEYS.iter().any(|key| {
        map.get(*key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
}

/// Composes a save of `incoming` over `disk` for `intent`. Not upstream's for a
/// sign-in (`OuterLogin`): it never writes an incoming `siwc` or `use_siwc`, so a
/// file keeps its own and a new file has neither, where upstream's login save
/// writes them when the file lacks them.
pub(crate) fn merge_persist(
    disk: Option<&Map<String, Value>>,
    incoming: &Map<String, Value>,
    intent: &CodexPersistIntent,
) -> (Map<String, Value>, PatchApplied) {
    let Some(disk) = disk else {
        let mut merged = incoming.clone();
        // A sign-in never writes SIWC: the destination decides, and with no
        // file it holds none.
        if matches!(intent, CodexPersistIntent::OuterLogin) {
            merged.shift_remove("siwc");
            merged.shift_remove("use_siwc");
        }
        return (merged, PatchApplied::default());
    };
    let mut merged = match intent {
        CodexPersistIntent::Configuration => {
            let mut merged = incoming.clone();
            let oauth = ["refresh_token", "last_refresh"].iter().any(|k| {
                disk.get(*k)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            });
            for key in OUTER_KEYS {
                if oauth
                    || merged.get(*key).is_none_or(|v| {
                        v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty())
                    })
                {
                    match disk.get(*key) {
                        Some(value) => {
                            merged.insert((*key).into(), value.clone());
                        }
                        None if oauth => {
                            merged.remove(*key);
                        }
                        None => {}
                    }
                }
            }
            if let Some(siwc) = disk.get("siwc") {
                merged.insert("siwc".into(), siwc.clone());
            }
            if !incoming.contains_key("use_siwc")
                && let Some(value) = disk.get("use_siwc")
            {
                merged.insert("use_siwc".into(), value.clone());
            }
            merged
        }
        CodexPersistIntent::OuterLogin => {
            let mut merged = disk.clone();
            for (key, value) in incoming {
                if key != "siwc"
                    && key != "use_siwc"
                    && (OUTER_KEYS.contains(&key.as_str())
                        || (!value.is_null()
                            && (!value.as_str().is_some_and(|s| s.trim().is_empty())
                                || !disk.contains_key(key))))
                {
                    merged.insert(key.clone(), value.clone());
                }
            }
            merged
        }
        CodexPersistIntent::SiwcBind(siwc) => {
            let mut merged = disk.clone();
            merged.insert("siwc".into(), Value::Object(siwc.clone()));
            merged.insert("use_siwc".into(), Value::Bool(true));
            merged
        }
        CodexPersistIntent::Refresh(patch) => {
            let (mut merged, applied) = patch.apply(disk);
            super::metadata::normalize_credential_metadata(&mut merged);
            return (merged, applied);
        }
    };
    super::metadata::normalize_credential_metadata(&mut merged);
    (merged, PatchApplied::default())
}

#[cfg(test)]
#[path = "codex_refresh_tests.rs"]
mod tests;
