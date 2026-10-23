//! A run-level budget for uncertified config-save differences, never a proof.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub const REFRESH: &str = "cargo run --locked -p open-ferry-parity -- --write-baseline tools/parity/baseline/config-save.json --baseline-observation <run.json> ... (see tools/parity/README.md)";
type Result<T> = std::result::Result<T, Box<dyn Error>>;
pub type Fingerprint = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub fingerprint: Fingerprint,
    pub seed: u64,
    pub random: usize,
    pub cases: usize,
    /// Historical selected failures supply kind coverage, not a rate sample.
    pub selected: bool,
    pub different: usize,
    pub non_certification: usize,
    pub case_kinds: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    fingerprint: Fingerprint,
    kinds: BTreeSet<String>,
    max_different_per_1000: usize,
    pr_different_cases: BTreeSet<String>,
    calibration: Vec<Calibration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Calibration {
    selected: bool,
    seed: u64,
    random: usize,
    cases: usize,
    different: usize,
}

pub struct Assessment {
    pub passed: bool,
    pub markdown: String,
}

const GENERATOR_PATHS: &[&str] = &[
    "tools/parity/src/generate.rs",
    "tools/parity/src/generate/config_save.rs",
    "tools/parity/src/generate/config_diff.rs",
    "tools/parity/src/cases/config_save.rs",
    "tools/parity/src/cases/testdata",
];

/// Hash only the generator paths listed in `GENERATOR_PATHS`; other case inputs
/// are not fingerprinted yet. Product, certifier, harness and toolchain changes
/// are judged by the resulting verdict instead.
fn fingerprint_paths(root: &Path) -> Result<BTreeSet<PathBuf>> {
    fn collect(root: &Path, path: PathBuf, files: &mut BTreeSet<PathBuf>) -> Result<()> {
        if path.is_dir() {
            for entry in fs::read_dir(path)? {
                collect(root, entry?.path(), files)?;
            }
        } else {
            files.insert(path.strip_prefix(root)?.to_owned());
        }
        Ok(())
    }
    let mut files = BTreeSet::new();
    for path in GENERATOR_PATHS {
        collect(root, root.join(path), &mut files)?;
    }
    Ok(files)
}

fn fingerprint_at(root: &Path, upstream: &str) -> Result<Fingerprint> {
    // Sort normalized relative paths, not platform-specific separators.
    let mut normalized = BTreeMap::new();
    for path in fingerprint_paths(root)? {
        normalized.insert(
            path.to_string_lossy().replace('\\', "/"),
            fs::read_to_string(root.join(path))?.replace("\r\n", "\n"),
        );
    }
    let mut hash = Sha256::new();
    for (path, text) in normalized {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(text.as_bytes());
        hash.update([0]);
    }
    let generator_hash = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(BTreeMap::from([
        ("classifier_version".into(), "2".into()),
        ("upstream_pin".into(), upstream.trim().into()),
        ("generator_sha256".into(), generator_hash),
    ]))
}

/// Hash `GENERATOR_PATHS` with checkout-independent line endings and paths.
pub fn fingerprint(upstream: &str) -> Result<Fingerprint> {
    fingerprint_at(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap(),
        upstream,
    )
}

pub fn pinned_fingerprint() -> Result<Fingerprint> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let result = std::process::Command::new("git")
        .current_dir(root)
        .args(["ls-tree", "HEAD", "reference/cliproxyapi"])
        .output()?;
    if !result.status.success() {
        return Err("cannot read upstream gitlink".into());
    }
    let line = String::from_utf8(result.stdout)?;
    fingerprint(
        line.split_whitespace()
            .nth(2)
            .ok_or("missing upstream gitlink")?,
    )
}

pub fn config_output(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| matches!(key.as_str(), "files" | "error" | "unloadable"))
    }) && value["files"]
        .as_array()
        .is_some_and(|files| files.iter().all(Value::is_string))
        && ["error", "unloadable"]
            .iter()
            .all(|key| value.get(key).is_none_or(Value::is_string))
        && !(value.get("error").is_some() && value.get("unloadable").is_some())
}

/// `unloadable` includes the rejected file; `error` contains only successful
/// snapshots. Compare the same successful-prefix boundary on both sides.
fn successful_prefix(value: &Value) -> Option<usize> {
    let count = value["files"].as_array()?.len();
    if value.get("unloadable").is_some() {
        count.checked_sub(1)
    } else if value.get("error").is_some() {
        Some(count)
    } else {
        None
    }
}

/// Only an actual failed certifier attempt can supply a kind. Panics, malformed
/// wrappers, missing diagnostics and unavailable replays remain fatal.
pub fn difference_kinds(
    options: &Value,
    proof: &Value,
    go: &Value,
    protected: Option<&Value>,
) -> Option<BTreeSet<String>> {
    if proof["kind"] != "uncertified-protection-ablation"
        || !config_output(go)
        || protected.is_some_and(|value| !config_output(value))
    {
        return None;
    }
    let steps = options["steps"].as_array()?;
    if proof["trials"].as_array().is_some_and(|trials| {
        trials.iter().any(|trial| {
            trial["failures"].as_array().is_some_and(|failures| {
                failures.iter().any(|f| {
                    matches!(
                        f["reason"].as_str(),
                        Some(
                            "replay_unavailable"
                                | "comment_free_go_panicked"
                                | "comment_free_go_malformed"
                        )
                    )
                })
            })
        })
    }) {
        return None;
    }
    if let Some(protected) = protected
        && (protected.get("error").is_some() || protected.get("unloadable").is_some())
        && (!(go.get("error").is_some() || go.get("unloadable").is_some())
            || successful_prefix(go)? > successful_prefix(protected)?)
    {
        // A Rust-only refusal is not a comment-placement limitation. Only the
        // existing Unicode guard gets its own (deliberately unbaselined) kind.
        if !protected["error"]
            .as_str()
            .is_some_and(unicode_guard_refusal)
        {
            return None;
        }
        let op = steps.get(protected["files"].as_array()?.len())?["op"].as_str()?;
        return matches!(op, "save" | "nested" | "write")
            .then(|| BTreeSet::from([format!("rust-refusal/unicode-guard/{op}")]));
    }
    let mut kinds = BTreeSet::new();
    let mut add = |failure: &Value, prefix: &str| -> Option<()> {
        let reason = failure["reason"].as_str()?;
        if reason.is_empty()
            || !reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            || reason.contains("panicked")
            || matches!(reason, "replay_unavailable" | "comment_free_go_malformed")
        {
            return None;
        }
        let op = steps.get(failure["step"].as_u64()? as usize)?["op"].as_str()?;
        if !matches!(op, "save" | "nested" | "write") {
            return None;
        }
        let check = if prefix == "protected-replay" {
            prefix.to_owned()
        } else {
            let check = failure["check"].as_u64()?;
            if !(1..=5).contains(&check) {
                return None;
            }
            format!("check-{check}{prefix}")
        };
        kinds.insert(format!("{check}/{reason}/{op}"));
        Some(())
    };
    match proof["reason"].as_str() {
        Some("check 1: no reproducing subset") => {
            for trial in proof["trials"].as_array()? {
                for failure in trial["failures"].as_array()? {
                    add(failure, "-no-subset")?;
                }
            }
        }
        Some("protected replay mismatch") => {
            // The existing replay records attempted rejected bytes while the
            // ordinary writer returns its unchanged successful prefix.
            let protected = protected?;
            if !protected["error"]
                .as_str()?
                .starts_with("failed to parse config file: yaml:")
            {
                return None;
            }
            let failures = proof["failures"].as_array()?;
            if failures.len() != 1 {
                return None;
            }
            for failure in failures {
                if failure["stage"] != "protected_replay"
                    || failure["reason"] != "missing_snapshot"
                    || failure["step"].as_u64()? as usize != protected["files"].as_array()?.len()
                {
                    return None;
                }
                add(failure, "protected-replay")?;
            }
        }
        None
        | Some(
            "check 2: comment-free source changes values"
            | "check 2: unsupported non-string-keyed values"
            | "check 2: unsupported binary values"
            | "check 2: multiple YAML documents",
        ) => {
            for failure in proof["failures"].as_array()? {
                add(failure, "")?;
            }
        }
        _ => return None,
    }
    (!kinds.is_empty()).then_some(kinds)
}

/// The portable wrapper has only text errors. `SaveErrorKind::Unwritable` is
/// shared with other refusals and the trace has no guard-specific event, so
/// recognize the complete guard message, never a substring or error prefix.
fn unicode_guard_refusal(message: &str) -> bool {
    let Some(message) = message.strip_suffix("; write refused, file unchanged") else {
        return false;
    };
    let Some((location, separators)) = message
        .rsplit_once(", is read differently by the saphyr loader and writer in input containing ")
    else {
        return false;
    };
    let Some(location) = location
        .strip_prefix("config scalar at line ")
        .or_else(|| location.strip_prefix("config value at line "))
    else {
        return false;
    };
    let Some((line, path)) = location.split_once(", path ") else {
        return false;
    };
    if line.is_empty()
        || !line.bytes().all(|b| b.is_ascii_digit())
        || !line.parse::<usize>().is_ok_and(|n| n > 0)
        || !path.starts_with('$')
    {
        return false;
    }
    let names = [
        "U+2028 (LINE SEPARATOR)",
        "U+2029 (PARAGRAPH SEPARATOR)",
        "U+0085 (NEXT LINE)",
    ];
    (1..8).any(|mask| {
        names
            .iter()
            .enumerate()
            .filter_map(|(i, name)| (mask & (1 << i) != 0).then_some(*name))
            .collect::<Vec<_>>()
            .join(", ")
            == separators
    })
}

pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    // serde_json uses preserve_order elsewhere: explicitly sort every object.
    fn sorted(value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(k, v)| (k, sorted(v)))
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(sorted).collect()),
            other => other,
        }
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&sorted(serde_json::to_value(value)?))?
        ),
    )?;
    Ok(())
}

impl Observation {
    fn validate(&self, fingerprint: &Fingerprint) -> Result<()> {
        if &self.fingerprint != fingerprint {
            return Err(
                format!("config-save baseline fingerprint mismatch; refresh: {REFRESH}").into(),
            );
        }
        if self.cases == 0
            || self.different > self.cases
            || self.non_certification + self.case_kinds.len() != self.different
            || self.case_kinds.values().any(BTreeSet::is_empty)
        {
            return Err("invalid config-save observation counts".into());
        }
        Ok(())
    }
}

impl Baseline {
    pub fn validate(&self, fingerprint: &Fingerprint) -> Result<()> {
        if &self.fingerprint != fingerprint {
            return Err(
                format!("config-save baseline fingerprint mismatch; refresh: {REFRESH}").into(),
            );
        }
        if self.max_different_per_1000 > 1000 || self.calibration.is_empty() {
            return Err("invalid config-save baseline".into());
        }
        Ok(())
    }

    pub fn assess(&self, observation: &Observation, path: &Path) -> Result<Assessment> {
        self.validate(&observation.fingerprint)?;
        observation.validate(&self.fingerprint)?;
        let mut errors = Vec::new();
        if observation.non_certification != 0 {
            errors.push(format!(
                "{} non-certification failures (always fatal)",
                observation.non_certification
            ));
        }
        let mut counts = BTreeMap::new();
        for kinds in observation.case_kinds.values() {
            for kind in kinds {
                *counts.entry(kind).or_insert(0usize) += 1;
            }
        }
        for kind in counts.keys() {
            if !self.kinds.contains(*kind) {
                errors.push(format!("new difference kind: {kind}"));
            }
        }
        if !observation.selected
            && observation.different as u128 * 1000
                > self.max_different_per_1000 as u128 * observation.cases as u128
        {
            errors.push("config-save Different rate exceeds baseline".into());
        }
        let pr = !observation.selected && observation.seed == 1 && observation.random == 25;
        if pr {
            for id in observation.case_kinds.keys() {
                if !self.pr_different_cases.contains(id) {
                    errors.push(format!("new PR-run Different case: {id}"));
                }
            }
        }
        let mut markdown = format!(
            "\n### Config-save baseline ratchet\n\nBaseline: `{}`. Different remains Different; this budget grants no certification.\n\n",
            path.display()
        );
        let _ = writeln!(
            markdown,
            "| Kind | Cases | Per 1,000 | Baseline |\n|---|---:|---:|---|"
        );
        for kind in self.kinds.iter().chain(
            counts
                .keys()
                .filter(|kind| !self.kinds.contains(**kind))
                .copied(),
        ) {
            let count = counts.get(kind).copied().unwrap_or(0);
            let _ = writeln!(
                markdown,
                "| `{kind}` | {count} | {} | {} |",
                if observation.selected {
                    "n/a (selected)".into()
                } else {
                    format!("{:.2}", count as f64 * 1000.0 / observation.cases as f64)
                },
                if self.kinds.contains(kind) {
                    "known kind; total bound below"
                } else {
                    "NEW KIND"
                }
            );
        }
        if !observation.selected {
            let _ = writeln!(
                markdown,
                "\nTotal Different: **{}/{} ({:.2} per 1,000)**; maximum **{} per 1,000**. Kind counts can overlap.",
                observation.different,
                observation.cases,
                observation.different as f64 * 1000.0 / observation.cases as f64,
                self.max_different_per_1000
            );
        }
        if observation.selected {
            let _ = writeln!(
                markdown,
                "Selected historical failure replay: {}/{} Different, kind coverage only; no rate verdict or rate estimate.",
                observation.different, observation.cases
            );
        }
        let reference = self
            .calibration
            .iter()
            .filter(|c| !c.selected && c.random == observation.random && c.seed == observation.seed)
            .map(|c| c.different as u128 * observation.cases as u128 / c.cases as u128)
            .min()
            .or_else(|| {
                self.calibration
                    .iter()
                    .filter(|c| !c.selected && c.random == observation.random)
                    .map(|c| c.different as u128 * observation.cases as u128 / c.cases as u128)
                    .min()
            });
        if errors.is_empty()
            && ((pr && observation.case_kinds.len() < self.pr_different_cases.len())
                || (!observation.selected
                    && reference.is_some_and(|n| (observation.different as u128) < n)))
        {
            let _ = writeln!(markdown, "Baseline can be lowered; refresh: `{REFRESH}`.");
        }
        for error in &errors {
            let _ = writeln!(markdown, "- FAIL: {error}");
        }
        let passed = errors.is_empty();
        let _ = writeln!(
            markdown,
            "\n{} verdict: **{}**.",
            if observation.selected {
                "Kind coverage"
            } else {
                "Ratchet"
            },
            if passed { "PASS" } else { "FAIL" }
        );
        Ok(Assessment { passed, markdown })
    }
}

/// Refresh requires ten independent nightly-sized config samples and the exact
/// PR profile. Selected historical failures may add kinds, never inflate rates.
pub fn refresh(observations: &[Observation], fingerprint: &Fingerprint) -> Result<Baseline> {
    refresh_with_existing(observations, fingerprint, None)
}

/// Reviewed additions need only their own observations. Incremental refresh
/// preserves the PR set and cannot lower the bound; a full refresh can lower it.
pub fn refresh_incremental(
    existing: &Baseline,
    observations: &[Observation],
    fingerprint: &Fingerprint,
) -> Result<Baseline> {
    existing.validate(fingerprint)?;
    refresh_with_existing(observations, fingerprint, Some(existing))
}

fn refresh_with_existing(
    observations: &[Observation],
    fingerprint: &Fingerprint,
    existing: Option<&Baseline>,
) -> Result<Baseline> {
    let mut seeds = BTreeSet::new();
    let mut calibration: BTreeSet<_> = existing
        .into_iter()
        .flat_map(|b| b.calibration.iter().cloned())
        .collect();
    let mut kinds = existing.map(|b| b.kinds.clone()).unwrap_or_default();
    let mut max_rate = existing.map(|b| b.max_different_per_1000).unwrap_or(0);
    let mut pr = existing.map(|b| b.pr_different_cases.clone());
    for observation in observations {
        observation.validate(fingerprint)?;
        if observation.non_certification != 0 {
            return Err("cannot baseline non-certification failures".into());
        }
        if !observation.selected {
            if observation.random >= 5000 {
                seeds.insert(observation.seed);
            }
            // 20% margin above the maximum observed rate, rounded up to 5.
            max_rate = max_rate.max(
                (observation.different * 1200)
                    .div_ceil(observation.cases)
                    .next_multiple_of(5),
            );
            if observation.seed == 1 && observation.random == 25 {
                let ids: BTreeSet<_> = observation.case_kinds.keys().cloned().collect();
                if pr.as_ref().is_some_and(|old| old != &ids) {
                    if existing.is_some() {
                        return Err(
                            "PR observation conflicts with existing baseline's Different case set"
                                .into(),
                        );
                    }
                    return Err("inconsistent PR observations".into());
                }
                pr = Some(ids);
            }
        }
        for set in observation.case_kinds.values() {
            kinds.extend(set.iter().cloned());
        }
        calibration.insert(Calibration {
            selected: observation.selected,
            seed: observation.seed,
            random: observation.random,
            cases: observation.cases,
            different: observation.different,
        });
    }
    if existing.is_none() && seeds.len() < 10 {
        return Err(
            "baseline refresh needs at least ten distinct nightly-sized config seeds".into(),
        );
    }
    Ok(Baseline {
        fingerprint: fingerprint.clone(),
        kinds,
        max_different_per_1000: max_rate,
        pr_different_cases: pr.ok_or("baseline refresh needs seed 1 / random 25")?,
        calibration: calibration.into_iter().collect(),
    })
}

pub fn observations(paths: &[PathBuf]) -> Result<Vec<Observation>> {
    paths.iter().map(|path| read(path)).collect()
}

pub fn replay_cases(path: &Path) -> Result<Vec<crate::cases::Case>> {
    let mut paths = BTreeSet::new();
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            paths.insert(path);
        }
    }
    let mut cases = Vec::new();
    for path in paths {
        let saved: Value = read(&path)?;
        let name = saved["case"]
            .as_str()
            .ok_or("saved fixture without case ID")?;
        if !crate::safe_case_name(name)
            || !saved["options"]["file"].is_string()
            || !saved["options"]["steps"].is_array()
        {
            return Err("invalid saved config-save fixture".into());
        }
        cases.push(
            crate::cases::Case::new(
                name,
                saved["model"].as_str().unwrap_or(""),
                saved["request_text"].as_str().unwrap_or(""),
            )
            .with_options(saved["options"].clone()),
        );
    }
    if cases.is_empty() {
        return Err("no saved config-save fixtures".into());
    }
    Ok(cases)
}

/// Import complete saved failure fixtures using the very same classifier as a
/// live offline sweep. `cases` is its original suite denominator; selected
/// replays are marked explicitly and are excluded from rate calibration.
pub fn import(failures: &Path, proofs: &Path, mut observation: Observation) -> Result<Observation> {
    let mut files = BTreeSet::new();
    for entry in fs::read_dir(failures)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            files.insert(path);
        }
    }
    for path in files {
        let case: Value = read(&path)?;
        let id = case["case"].as_str().ok_or("fixture without case ID")?;
        if !crate::safe_case_name(id) {
            return Err("unsafe imported case ID".into());
        }
        let proof: Value = match fs::read(proofs.join(path.file_name().unwrap())) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(error) => return Err(error.into()),
        };
        let go = case["upstream_output_raw"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        let bound = proof["case"] == id
            && go.as_ref().is_some_and(config_output)
            && config_output(&case["open_ferry_output"])
            && proof
                .get("original_go")
                .is_none_or(|p| Some(p) == go.as_ref())
            && proof
                .get("protected_rust")
                .is_none_or(|p| p == &case["open_ferry_output"]);
        let kinds = bound
            .then(|| {
                difference_kinds(
                    &case["options"],
                    &proof,
                    go.as_ref().unwrap(),
                    Some(&case["open_ferry_output"]),
                )
            })
            .flatten();
        observation.different += 1;
        if let Some(kinds) = kinds {
            if observation
                .case_kinds
                .insert(id.to_owned(), kinds)
                .is_some()
            {
                return Err("duplicate imported case ID".into());
            }
        } else {
            observation.non_certification += 1;
        }
    }
    observation.validate(&observation.fingerprint)?;
    Ok(observation)
}

#[cfg(test)]
mod tests;
