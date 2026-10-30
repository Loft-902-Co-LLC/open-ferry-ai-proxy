// Ported from CLIProxyAPI internal/config/config_yaml.go
// (SaveConfigPreserveComments, SaveConfigPreserveCommentsUpdateNestedScalar)
// and internal/api/handlers/management/config_basic.go (WriteConfig)
// (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Writing the config file back with its comments, its layout and the
//! settings this port doesn't type.
//!
//! - [`save_preserving_comments`] writes a [`Config`]'s settings into the
//!   file, as upstream's `SaveConfigPreserveComments` does: it merges the
//!   settings into the file's tree, so existing keys keep their comments,
//!   order and quoting, new keys are added unless they hold a default, and
//!   a file in the v8 layout stays in it (with `migrate_v8`, or when the
//!   file already uses the v8 layout, the whole file moves to it).
//! - [`update_nested_scalar`] sets one string at a path of mapping keys
//!   (`SaveConfigPreserveCommentsUpdateNestedScalar`). Upstream calls it
//!   when loading hashes a plain management key; loading never writes here,
//!   so only `tools/parity` calls it.
//! - [`write_file`] writes a whole file, as the management API's
//!   `WriteConfig` does: a file in the v8 layout is completed into it
//!   first, and comment lines are moved to the start of their line.
//! - [`write_as_is`] writes a whole file unchanged; it is this port's, for
//!   `open-ferry init`.
//! - [`undo`] puts back the file's previous contents from its backup, and
//!   [`defaults`] gives the settings an empty config holds; both are this
//!   port's, for `open-ferry config`.
//! - [`write_file_expecting`] is [`write_file`] made only while the file
//!   holds the bytes the caller read; it is this port's, for `open-ferry
//!   config`, which shows a change before it makes it.
//!
//! YAML is read and written with a port of gopkg.in/yaml.v3 (the
//! crate-private `config::yaml3`), so the bytes written are upstream's.
//! Settings, sections and keys the [`Config`] doesn't type stay as the file
//! has them, with their comments.
//!
//! Every write is checked to load as a config ([`Config::load`]'s rules
//! and wording) before anything is written; it is atomic, refuses a
//! symbolic link, keeps the file's previous contents as `<file name>.bak`,
//! and records the SHA-256 of what it wrote as `<file name>.sha256` (see
//! `write`). It holds an exclusive OS lock on `<file name>.lock`, which
//! stays beside the file, from the read it works from to that record, so
//! writes in this process and in others never interleave; it waits up to
//! five seconds for another to let go. A refused write leaves the file as
//! it was and returns a [`SaveError`] that says why.
//!
//! Deviations from upstream:
//! - Comment normalization leaves block scalar bodies alone in every section.
//!   Multiline line comments on block scalars are quoted safely, so emitting
//!   them cannot turn the comment into scalar data. Other block styles stay.
//!   A conflicting multiline key comment moves to its key's head only if
//!   its actual deferred recipient would damage scalar data or drop it.
//! - The check, the backup, the record, the lock, the atomic replacement
//!   and the symbolic link refusal are this port's; upstream writes the
//!   file in place.
//! - A file that isn't UTF-8 is refused (`yaml: input is not valid
//!   UTF-8`); yaml.v3 also reads UTF-16.
//! - After a save that moves the file to the v8 layout, upstream updates
//!   the config's `OAuthOnlyFields` from the written file; the `&Config`
//!   here isn't changed, so the caller reloads the file.
//! - Upstream decodes a migrated file once more and fails with `decode
//!   migrated config: ...`; the load check covers it, with its own wording.
//! - Each submodule lists its own deviations.

#![allow(
    clippy::mutable_key_type,
    reason = "CommentId lookup hints do not participate in equality or hashing"
)]

mod generate;
mod merge;
#[cfg(any(feature = "parity", test))]
#[doc(hidden)]
pub mod parity;
pub(crate) use merge::normalize_collection_node_styles;
pub(crate) mod tree;
mod v8;
mod write;
pub(crate) use write::{IntendedValues, check as check_values};

use std::path::{Path, PathBuf};
use std::{fmt, io};

use super::Config;
use super::layout::V8Document;
use super::v8 as loader_v8;
use super::yaml as loader_yaml;
use super::yaml3::compose::{self, YamlError};
use super::yaml3::encode;
use super::yaml3::{Kind, MAP_TAG, NULL_TAG, STR_TAG};

// The tree operations the v8 config edits ([`super::v8_edit`]) build on.
pub(crate) use super::yaml3::Node;
#[cfg(any(test, feature = "parity"))]
pub(crate) use tree::marshal_config;
#[cfg(test)]
pub(crate) use tree::normalize_comment_indentation;
pub(crate) use tree::{
    copy_yaml_path_value, delete_yaml_path, expand_config_aliases, find_map_key_index,
    get_or_create_map_value, legacy_path, marshal, remove_map_key, set_yaml_path_with_comments,
    yaml_path, yaml_path_mut,
};
pub(crate) use v8::{
    flatten_v8, is_v8_config_layout, normalize_config_layout, normalize_config_layout_doc,
    normalize_config_layout_receipted, normalize_config_layout_source, project_v8_config_aliases,
    v8_aliases,
};
pub(crate) use write::WriteLock;

/// Why a config write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SaveErrorKind {
    /// Reading or writing a file failed, or another write held the file's
    /// lock for too long.
    Io,
    /// The file isn't YAML yaml.v3 reads, or the tree can't be written.
    Yaml,
    /// The YAML doesn't have the shape of a config file.
    Invalid,
    /// The result wouldn't load as a config.
    Check,
    /// The path is a symbolic link.
    Symlink,
    /// The config holds a value the writer can't express.
    Unwritable,
    /// There is no backup to undo to (see [`undo`]).
    NoBackup,
    /// The file, or its backup, no longer holds what the caller read
    /// (see [`write_file_expecting`] and [`UndoCheck`]).
    Stale,
    /// The file was changed since the last write here, by hand or by a
    /// writer that keeps no record, so an undo would lose that change
    /// (see [`undo`]).
    ChangedSince,
}

/// A refused config write; nothing was written. The message is upstream's
/// where upstream has one.
#[derive(Debug)]
pub struct SaveError {
    kind: SaveErrorKind,
    message: String,
    source: Option<io::Error>,
}

impl SaveError {
    pub(crate) fn new(kind: SaveErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn io(message: impl Into<String>, source: io::Error) -> Self {
        Self {
            kind: SaveErrorKind::Io,
            message: message.into(),
            source: Some(source),
        }
    }

    /// What kind of failure this is.
    pub fn kind(&self) -> SaveErrorKind {
        self.kind
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

impl From<YamlError> for SaveError {
    fn from(error: YamlError) -> Self {
        Self::new(SaveErrorKind::Yaml, error.message())
    }
}

fn invalid(message: impl Into<String>) -> SaveError {
    SaveError::new(SaveErrorKind::Invalid, message)
}

/// `yaml.Unmarshal(data, &node)`: the document node of the first document
/// in `data`, with its comments, or an empty node when there is none.
pub(crate) fn unmarshal(data: &[u8]) -> Result<Node, SaveError> {
    Ok(compose::unmarshal(data)?)
}

/// The loader's tree of `data`, or `None` when it holds no document.
fn loader_root(data: &[u8]) -> Result<Option<loader_yaml::Node>, SaveError> {
    let text = std::str::from_utf8(data)
        .map_err(|_| SaveError::new(SaveErrorKind::Yaml, "yaml: input is not valid UTF-8"))?;
    loader_yaml::parse_document(text)
        .map_err(|error| SaveError::new(SaveErrorKind::Yaml, error.message()))
}

/// Upstream's `flattenV8` checks of a document, with its errors: the
/// loader's. Upstream runs them before it moves anything.
pub(crate) fn check_layout(data: &[u8]) -> Result<(), SaveError> {
    if let Some(root) = loader_root(data)? {
        loader_v8::flatten_v8(&root).map_err(|error| invalid(error.to_string()))?;
    }
    Ok(())
}

/// Writes `cfg`'s settings into the config file at `path`, keeping the
/// file's comments, order, layout and the settings `cfg` doesn't type
/// (upstream's `SaveConfigPreserveComments`). With `migrate_v8`, or when
/// the file uses the v8 layout, the file is written in the v8 layout. The
/// file's lock is held from the read to the write.
pub fn save_preserving_comments(
    path: &Path,
    cfg: &Config,
    migrate_v8: bool,
) -> Result<(), SaveError> {
    save_preserving_comments_expecting(path, cfg, migrate_v8, None).map(|_| ())
}

/// [`save_preserving_comments`], made only while the file at `path` has
/// the SHA-256 `expected_sha256` (in hex), when one is given: the one of
/// the contents the caller loaded `cfg` from, or last wrote. Else a
/// [`SaveErrorKind::Stale`] error, and nothing is written, since `cfg`
/// doesn't hold what changed in the file since, and writing it would undo
/// that. The check is made under the file's lock, with the read the write
/// starts from. Returns the SHA-256 of what it wrote, in lowercase hex.
/// Not upstream's: the management API saves with it, so it never writes
/// over a change made in the file since the server loaded it, as by
/// `open-ferry config`.
pub fn save_preserving_comments_expecting(
    path: &Path,
    cfg: &Config,
    migrate_v8: bool,
    expected_sha256: Option<&str>,
) -> Result<String, SaveError> {
    let lock = WriteLock::acquire(path)?;
    let data = write::read(path)?;
    if let Some(expected) = expected_sha256
        && !expected.trim().eq_ignore_ascii_case(&sha256_hex(&data))
    {
        return Err(SaveError::new(
            SaveErrorKind::Stale,
            format!(
                "{} changed since it was loaded; nothing was written",
                path.display()
            ),
        ));
    }
    let intended = IntendedValues::settings(&data, cfg, migrate_v8)?;
    let out = render_preserving_comments(&data, cfg, migrate_v8)?;
    let expected = expected_sha256.map(|_| data.as_slice());
    write::commit_locked_values(&lock, path, &out, expected, &intended)?;
    Ok(sha256_hex(&out))
}

/// Sets the string at the path of mapping `keys` in the config file at
/// `path`, creating the mappings on the way and keeping everything else
/// (upstream's `SaveConfigPreserveCommentsUpdateNestedScalar`).
pub fn update_nested_scalar(path: &Path, keys: &[&str], value: &str) -> Result<(), SaveError> {
    let lock = WriteLock::acquire(path)?;
    let data = write::read(path)?;
    let out = render_nested_scalar(&data, keys, value)?;
    write::commit_locked(&lock, path, &out, None)
}

/// Writes `bytes` as the config file at `path`, as the management API's
/// `WriteConfig` does: a file in the v8 layout is moved fully into it
/// (`NormalizeConfigLayout`), and comment lines are moved to the start of
/// their line.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    write_file_with_sha256(path, bytes).map(|_| ())
}

/// [`write_file`], returning the SHA-256 of what it wrote, in lowercase
/// hex. Not upstream's: the management API keeps it, to check that the
/// file still holds it before it next saves.
pub fn write_file_with_sha256(path: &Path, bytes: &[u8]) -> Result<String, SaveError> {
    write::refuse_link(path)?;
    let out = render_write_file(bytes)?;
    write::commit(path, &out)?;
    Ok(sha256_hex(&out))
}

/// [`write_file`], made only while the file at `path` holds `expected`,
/// the bytes the caller read and worked `bytes` out from: else a
/// [`SaveErrorKind::Stale`] error, and nothing is written. Not upstream's:
/// `open-ferry config` writes a change it has shown with it, so it never
/// writes over a change made since.
pub fn write_file_expecting(path: &Path, bytes: &[u8], expected: &[u8]) -> Result<(), SaveError> {
    write::refuse_link(path)?;
    let out = render_write_file(bytes)?;
    write::commit_expecting(path, &out, Some(expected))
}

/// [`write_file`] under `lock`, the caller's lock of the file at `path`,
/// taken before it read what it worked `bytes` out from. Returns the
/// SHA-256 of what it wrote, in lowercase hex.
pub(crate) fn write_file_locked(
    lock: &WriteLock,
    path: &Path,
    bytes: &[u8],
) -> Result<String, SaveError> {
    let out = render_write_file(bytes)?;
    write::commit_locked(lock, path, &out, None)?;
    Ok(sha256_hex(&out))
}

pub(crate) type RepeatWrite = Box<dyn FnOnce(&[u8]) -> Option<Vec<u8>>>;

/// A call-scoped edit result, before the callers' final write preparation.
pub(crate) struct PreparedWrite {
    pub(crate) candidate: tree::ReceiptedYaml,
    pub(crate) source: tree::CommentSource,
    pub(crate) repeat: RepeatWrite,
}

pub(crate) fn render_prepared_file(prepared: PreparedWrite) -> Result<Vec<u8>, SaveError> {
    let intended = IntendedValues::bytes(&prepared.candidate.bytes)?;
    let candidate = candidate_write_file_receipted(prepared.candidate)?;
    check_values(&candidate.bytes, &intended)?;
    if !tree::owner_protection_enabled() {
        return Ok(candidate.bytes);
    }
    let writer = candidate.bytes.clone();
    let proposal = tree::finish_comment_owners(&prepared.source, candidate, prepared.repeat);
    if proposal != writer && !repair_survives_following_saves(&writer, &proposal) {
        return Ok(writer);
    }
    Ok(proposal)
}

/// Commit the complete checked edit once, preserving the existing lock,
/// validation, backup and hash. Disabled protection follows the original path.
pub(crate) fn write_rendered_file_locked(
    lock: &WriteLock,
    path: &Path,
    prepared: PreparedWrite,
) -> Result<String, SaveError> {
    if !tree::owner_protection_enabled() {
        return write_file_locked(lock, path, &prepared.candidate.bytes);
    }
    let out = render_prepared_file(prepared)?;
    write::commit_locked(lock, path, &out, None)?;
    Ok(sha256_hex(&out))
}

/// Writes `bytes` as the config file at `path` unchanged, with the checks,
/// backup and atomic replacement every write here has. Not upstream's:
/// `open-ferry init` writes the config it makes with it.
pub fn write_as_is(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    write::commit(path, bytes)
}

/// Where every write here keeps the contents it replaced: `<file
/// name>.bak` beside the file at `path`.
pub fn backup_path(path: &Path) -> PathBuf {
    write::backup_path(path)
}

/// Where every write here records the SHA-256 of what it wrote:
/// `<file name>.sha256` beside the file at `path`, in `sha256sum`'s
/// format.
pub fn record_path(path: &Path) -> PathBuf {
    write::record_path(path)
}

/// The SHA-256 of `data`, in lowercase hex, as the record and
/// [`UndoCheck`] hold it.
pub fn sha256_hex(data: &[u8]) -> String {
    write::sha256_hex(data)
}

/// The SHA-256 of the last write here to the config file at `path`, as its
/// record keeps it, when there is a record that holds one. The file holds
/// what was last written here when its SHA-256 is this one.
pub fn recorded_sha256(path: &Path) -> Option<String> {
    write::recorded_sha256(path)
}

/// What an [`undo`] checks before it puts the backup back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UndoCheck {
    /// The SHA-256 the file must have, in hex: the one of the file the
    /// caller showed the undo of.
    pub config_sha256: Option<String>,
    /// The SHA-256 the backup must have, in hex.
    pub backup_sha256: Option<String>,
    /// Go ahead even when the file was changed since the last write here.
    pub force: bool,
}

/// Puts back the contents the last write replaced: writes the backup of
/// the config file at `path` (see [`backup_path`]) as the file, with the
/// checks every write here has. That write keeps what it replaces as the
/// new backup, so undoing again puts it back. Returns the config the file
/// now holds. Not upstream's: `open-ferry config undo` uses it.
///
/// The file's lock is held from the reads of the file and the backup to
/// the write, so another write never lands between them.
///
/// It is refused, and nothing is written:
/// - with [`SaveErrorKind::Stale`] when `check` gives a SHA-256 the file
///   or the backup doesn't have, or the file changes while it runs (by a
///   writer that takes no lock, as an editor);
/// - with [`SaveErrorKind::ChangedSince`] when the file isn't what the
///   last write here wrote (by its record, see [`recorded_sha256`]), as
///   after a hand edit, unless `check.force`: the backup is from before
///   that write, so putting it back would lose the edit too.
pub fn undo(path: &Path, check: &UndoCheck) -> Result<Config, SaveError> {
    undo_with_sha256(path, check).map(|(config, _)| config)
}

/// [`undo`], returning with the config the file now holds the SHA-256 of
/// what it wrote, in lowercase hex. Not upstream's: the management API
/// keeps it, to check that the file still holds it before it next saves.
pub fn undo_with_sha256(path: &Path, check: &UndoCheck) -> Result<(Config, String), SaveError> {
    let lock = WriteLock::acquire(path)?;
    let backup = write::backup_path(path);
    let data = match write::read(&backup) {
        Ok(data) => data,
        Err(error) if error.kind() == SaveErrorKind::Io && !backup.exists() => {
            return Err(SaveError::new(
                SaveErrorKind::NoBackup,
                format!("there is no backup to undo to: {}", backup.display()),
            ));
        }
        Err(error) => return Err(error),
    };
    let current = write::read(path)?;
    let current_sha256 = sha256_hex(&current);
    let differs = |expected: &Option<String>, actual: &str| {
        expected
            .as_deref()
            .is_some_and(|expected| !expected.trim().eq_ignore_ascii_case(actual))
    };
    if differs(&check.config_sha256, &current_sha256) {
        return Err(SaveError::new(
            SaveErrorKind::Stale,
            "the config file changed since the undo was worked out; nothing was undone",
        ));
    }
    if differs(&check.backup_sha256, &sha256_hex(&data)) {
        return Err(SaveError::new(
            SaveErrorKind::Stale,
            "the backup changed since the undo was worked out; nothing was undone",
        ));
    }
    if !check.force && recorded_sha256(path).as_deref() != Some(current_sha256.as_str()) {
        return Err(SaveError::new(
            SaveErrorKind::ChangedSince,
            "the config file was changed since the last change that kept a backup, so undoing would lose that change too; nothing was undone",
        ));
    }
    write::commit_locked(&lock, path, &data, Some(&current))?;
    let config = Config::load_bytes(&data)
        .map_err(|error| SaveError::new(SaveErrorKind::Check, error.to_string()))?;
    Ok((config, sha256_hex(&data)))
}

/// The settings a config that sets nothing holds, as the loader gives
/// them, in the v8 layout: what the writer would write for
/// [`Config::load_bytes`] of an empty file, with upstream's `omitempty`,
/// so some settings whose default is empty or zero aren't there. Not
/// upstream's: `open-ferry config get` shows a default from it.
pub fn defaults() -> Result<V8Document, SaveError> {
    let config = Config::load_bytes(b"")
        .map_err(|error| SaveError::new(SaveErrorKind::Check, error.to_string()))?;
    let value = generate::legacy_config(&config)
        .map_err(|error| SaveError::new(SaveErrorKind::Unwritable, error.0))?;
    let rendered = encode::marshal(&value)?;
    V8Document::migrate(&rendered).map_err(|error| invalid(error.to_string()))
}

/// The bytes [`write_file`] writes for `data`.
/// Raw owner repair is bounded independently of ordinary saves and edits.
const RAW_COMMENT_OWNER_BUDGET: usize = 64 * 1024;

pub(crate) fn render_write_file(data: &[u8]) -> Result<Vec<u8>, SaveError> {
    let (mut doc, source) = tree::read_comment_source(data)?;
    if let Some(root) = doc.content.first_mut()
        && omit_stable_update_channel(root)
    {
        // Removing a default must not turn duplicate keys into valid input.
        if let Some(original) = loader_root(data)? {
            loader_yaml::check_shape(&original)
                .map_err(|error| SaveError::new(SaveErrorKind::Yaml, error.message()))?;
        }
        let canonical = tree::marshal_config_for_source(&doc, &source)?;
        return render_write_file(&canonical.bytes);
    }
    if data.len() > RAW_COMMENT_OWNER_BUDGET || !tree::owner_protection_enabled() {
        return render_write_file_at_writer_placement(data);
    }
    let intended = IntendedValues::bytes(data)?;
    let (candidate, source, legacy_layout) = candidate_write_file_with_layout(data)?;
    let writer = candidate.bytes.clone();
    check_values(&writer, &intended)?;
    let protects_key_feet = source.key_feet_need_restoring(&candidate);
    let mut cached_previews = None;
    let proposal = tree::finish_comment_owners_proven(&source, candidate, |loaded| {
        let data = loaded.bytes();
        if data.len() > RAW_COMMENT_OWNER_BUDGET {
            if protects_key_feet {
                return None;
            }
            return render_write_file_at_writer_placement(data).ok();
        }
        if protects_key_feet {
            cached_previews = Some(raw_feet_survive_following_saves(data, legacy_layout)?);
        }
        let (candidate, source) = candidate_write_file(data).ok()?;
        Some(tree::finish_comment_owners_replay_loaded(
            &source, candidate, loaded,
        ))
    });
    if proposal != writer
        && !raw_comments_survive_following_saves(
            data,
            &writer,
            &proposal,
            legacy_layout,
            cached_previews,
        )
    {
        return Ok(writer);
    }
    Ok(proposal)
}

/// Main's writer path, without constructing owner receipts or preview saves.
fn render_write_file_at_writer_placement(data: &[u8]) -> Result<Vec<u8>, SaveError> {
    let intended = IntendedValues::bytes(data)?;
    let doc = unmarshal(data)?;
    let mut out = std::borrow::Cow::Borrowed(data);
    if let Some(root) = doc.content.first()
        && is_v8_config_layout(root)?
    {
        out = std::borrow::Cow::Owned(v8::normalize_config_layout_writer(data, true)?.0);
    }
    let out = tree::normalize_comment_indentation(&out);
    check_values(&out, &intended)?;
    Ok(out)
}

struct RawSavePreviews {
    config: Config,
    outputs: [Vec<u8>; 2],
}

fn raw_preview_keeps_comment_counts(
    source_counts: &std::collections::HashMap<&[u8], usize>,
    proposal_preview: &[u8],
    writer_preview: impl FnOnce() -> Result<Vec<u8>, SaveError>,
) -> bool {
    let Some(proposal_counts) = tree::physical_comment_counts(proposal_preview) else {
        return false;
    };
    // A complete source count already meets any source-capped writer count.
    if source_counts
        .iter()
        .all(|(text, count)| proposal_counts.get(*text).copied().unwrap_or(0) >= *count)
    {
        return true;
    }
    let Ok(writer_preview) = writer_preview() else {
        return false;
    };
    let Some(writer_counts) = tree::physical_comment_counts(&writer_preview) else {
        return false;
    };
    source_counts.iter().all(|(text, count)| {
        proposal_counts.get(*text).copied().unwrap_or(0)
            >= writer_counts.get(*text).copied().unwrap_or(0).min(*count)
    })
}

fn raw_comments_survive_following_saves(
    source: &[u8],
    writer: &[u8],
    proposal: &[u8],
    legacy_layout: bool,
    cached_previews: Option<RawSavePreviews>,
) -> bool {
    let Some(mut source_counts) = tree::physical_comment_counts(source) else {
        return false;
    };
    let Some(proposal_counts) = tree::physical_comment_counts(proposal) else {
        return false;
    };
    // The following save receives the expanded proposal, including every
    // physical alias copy. Its source count can exceed the raw input's count.
    for (text, count) in proposal_counts {
        let required = source_counts.entry(text).or_insert(0);
        *required = (*required).max(count);
    }
    // The whole-file gate proved identical values, scalar styles and tags.
    let previews = match cached_previews {
        Some(previews) => previews,
        None => {
            let Ok(config) = Config::parse(proposal) else {
                return false;
            };
            let outputs = if legacy_layout {
                let (Ok(save), Ok(migrate)) = (
                    render_preserving_comments(proposal, &config, false),
                    render_preserving_comments(proposal, &config, true),
                ) else {
                    return false;
                };
                [save, migrate]
            } else {
                // An explicit v8 marker makes both saves take the same
                // migration branch, including their repeat checks.
                let Ok(save) = render_preserving_comments(proposal, &config, true) else {
                    return false;
                };
                [save.clone(), save]
            };
            RawSavePreviews { config, outputs }
        }
    };
    // Every v8 edit first migrates the stored bytes themselves. A save's
    // migration instead sees its own generated output, so it is a separate
    // preview. Already-v8 raw results repeat this step in the owner gate.
    let layout_preview = if legacy_layout {
        let Ok((bytes, _)) = normalize_config_layout(proposal, true) else {
            return false;
        };
        Some(bytes)
    } else {
        None
    };
    for (index, output) in previews.outputs.iter().enumerate() {
        if !raw_preview_keeps_comment_counts(&source_counts, output, || {
            render_preserving_comments(writer, &previews.config, index == 1)
        }) {
            return false;
        }
    }
    layout_preview.is_none_or(|output| {
        raw_preview_keeps_comment_counts(&source_counts, &output, || {
            normalize_config_layout(writer, true).map(|(bytes, _)| bytes)
        })
    })
}

fn raw_feet_survive_following_saves(data: &[u8], legacy_layout: bool) -> Option<RawSavePreviews> {
    let config = Config::parse(data).ok()?;
    let mut outputs = [Vec::new(), Vec::new()];
    let count = if legacy_layout { 2 } else { 1 };
    for (index, output) in outputs.iter_mut().take(count).enumerate() {
        let migrate = index == 1 || !legacy_layout;
        let (candidate, source) = candidate_preserving_comments(data, &config, migrate).ok()?;
        // Keep this candidate for the source check instead of rendering it again.
        let bytes = tree::finish_comment_owners(&source, candidate.clone(), |data| {
            let (candidate, source) = candidate_preserving_comments(data, &config, migrate).ok()?;
            Some(tree::finish_comment_owners_replay(&source, candidate))
        });
        if !tree::output_holds_source(&source, candidate, &bytes) {
            return None;
        }
        *output = bytes;
    }
    if !legacy_layout {
        outputs[1] = outputs[0].clone();
    }
    Some(RawSavePreviews { config, outputs })
}

fn candidate_write_file(
    data: &[u8],
) -> Result<(tree::ReceiptedYaml, tree::CommentSource), SaveError> {
    candidate_write_file_with_layout(data).map(|(candidate, source, _)| (candidate, source))
}

fn candidate_write_file_with_layout(
    data: &[u8],
) -> Result<(tree::ReceiptedYaml, tree::CommentSource, bool), SaveError> {
    let (doc, source) = tree::read_comment_source(data)?;
    let legacy_layout = doc
        .content
        .first()
        .and_then(|root| yaml_path(root, "config-version"))
        .is_none_or(|version| version.tag != super::yaml3::INT_TAG || version.value != "8");
    let (out, source) = if let Some(root) = doc.content.first()
        && is_v8_config_layout(root)?
    {
        // Re-parsing loses receipts inside alias targets. Normalize the
        // annotated tree so expanded copies also enter this source catalog.
        let (out, source, _) = v8::layout_candidate_from_source(data, doc, source, true)?;
        (out, source)
    } else {
        (tree::source_yaml(data, &doc), source)
    };
    Ok((tree::normalize_receipted(out), source, legacy_layout))
}

/// The edit path also crosses this final write stage. Keep the original
/// occurrence bindings through it before choosing any repaired output.
pub(crate) fn candidate_write_file_receipted(
    mut out: tree::ReceiptedYaml,
) -> Result<tree::ReceiptedYaml, SaveError> {
    let doc = tree::read_receipted_for_render(&out)?;
    if let Some(root) = doc.content.first()
        && is_v8_config_layout(root)?
    {
        out = normalize_config_layout_receipted(out, true)?;
    }
    Ok(tree::normalize_receipted(out))
}

fn rebind_generated_comments(node: &mut Node, emitted: &[super::yaml3::CommentEmission]) {
    use std::collections::HashMap;
    let mut by_line: HashMap<usize, HashMap<&[u8], &super::yaml3::CommentId>> = HashMap::new();
    for origin in emitted {
        by_line
            .entry(origin.line)
            .or_default()
            .entry(&origin.text)
            .or_insert(&origin.id);
    }
    fn visit(node: &mut Node, by_line: &HashMap<usize, HashMap<&[u8], &super::yaml3::CommentId>>) {
        if let Some(id) = by_line
            .get(&node.line_comment_line)
            .and_then(|comments| comments.get(node.line_comment.as_bytes()))
        {
            node.line_comment_id = Some((*id).clone());
        }
        for child in &mut node.content {
            visit(child, by_line);
        }
    }
    visit(node, &by_line);
}

/// The generated tree: `cfg` as upstream marshals it, read back.
fn generated_root(cfg: &Config, plugin_configs: Option<&Node>) -> Result<Node, SaveError> {
    let mut value = generate::legacy_config(cfg)
        .map_err(|error| SaveError::new(SaveErrorKind::Unwritable, error.0))?;
    // Go marshals each plugin's decoded yaml.Node before merging. Besides
    // preserving its contents, this matters for yaml.v3's pending comments:
    // a line comment on a plugin mapping's key can survive its value's own
    // comment and attach to the following auth-dir scalar. Feed the same
    // nodes through our yaml.v3 encoder.
    if let Some(configs) = plugin_configs.filter(|node| node.kind == Kind::Mapping)
        && let encode::Value::Struct(fields) = &mut value
        && let Some(plugins) = fields.iter_mut().find(|field| field.key == "plugins")
        && let encode::Value::Struct(fields) = &mut plugins.value
        && let Some(field) = fields.iter_mut().find(|field| field.key == "configs")
    {
        field.value = generated_plugin_configs(configs)?;
    }
    let (rendered, mut identities, mut receipts) = encode::marshal_with_receipts(&value)?;
    let mut receipt_bytes = rendered.clone();
    let mut generated = match compose::unmarshal(&rendered) {
        Ok(generated) => generated,
        Err(error) => {
            // This intermediate only supplies generated settings. Its plugin
            // nodes can carry a pending key comment into a null plugin's {}
            // and make yaml.v3 emit invalid YAML. Retry those nodes without
            // comments and quote tab-leading block scalars, whose missing
            // indentation hint also makes this faithful intermediate invalid.
            // The merge retains every original plugin node/comment/style.
            let Some(plugins) = generated_plugins_field(&mut value) else {
                return Err(error.into());
            };
            repair_generated_plugin_nodes(plugins);
            let (rendered, retry_ids, retry_receipts) = encode::marshal_with_receipts(&value)?;
            identities = retry_ids;
            receipts = retry_receipts;
            receipt_bytes = rendered.clone();
            compose::unmarshal(&rendered)?
        }
    };
    // Re-parsing generated YAML changes ownership/positions. Rebind only an
    // actual emitted occurrence with the exact output line and full comment.
    rebind_generated_comments(&mut generated, &identities);
    if !receipts.owners.is_empty() || !receipts.comments.is_empty() {
        tree::rebind_comment_receipts(
            &mut generated,
            &tree::ReceiptedYaml {
                bytes: receipt_bytes,
                receipts,
            },
        );
    }
    if generated.kind != Kind::Document {
        return Err(invalid("invalid generated yaml structure"));
    }
    let Some(root) = generated.content.into_iter().next() else {
        return Err(invalid("invalid generated yaml structure"));
    };
    if root.kind != Kind::Mapping {
        return Err(invalid("expected generated root mapping node"));
    }
    Ok(root)
}

fn generated_plugins_field(value: &mut encode::Value) -> Option<&mut encode::Value> {
    let encode::Value::Struct(fields) = value else {
        return None;
    };
    let plugins = fields.iter_mut().find(|field| field.key == "plugins")?;
    let encode::Value::Struct(fields) = &mut plugins.value else {
        return None;
    };
    Some(
        &mut fields
            .iter_mut()
            .find(|field| field.key == "configs")?
            .value,
    )
}

fn repair_generated_plugin_nodes(value: &mut encode::Value) {
    fn clear(node: &mut Node) {
        node.head_comment.clear();
        node.line_comment.clear();
        node.foot_comment.clear();
        if node.kind == Kind::Scalar
            && node.value.starts_with('\t')
            && (node.style.has(super::yaml3::Style::LITERAL)
                || node.style.has(super::yaml3::Style::FOLDED))
        {
            node.style = super::yaml3::Style::DOUBLE_QUOTED;
        }
        for child in &mut node.content {
            clear(child);
        }
    }
    match value {
        encode::Value::Node(node) => clear(node),
        encode::Value::Map(entries) => {
            for (key, value) in entries {
                repair_generated_plugin_nodes(key);
                repair_generated_plugin_nodes(value);
            }
        }
        _ => {}
    }
}

/// PluginInstanceConfig.MarshalYAML writes a default empty mapping when
/// yaml.v3 skipped UnmarshalYAML for null; the null node's comments are lost.
fn generated_plugin_configs(configs: &Node) -> Result<encode::Value, YamlError> {
    Ok(encode::Value::Map(
        configs
            .pairs()
            .map(|(key, value)| {
                let mut value = if value.kind == Kind::Scalar && value.tag == NULL_TAG {
                    Node::mapping()
                } else {
                    value.clone()
                };
                // Multiline conflicts need protection in this intermediate too.
                // A dangling single comment can continue into generated auth-dir:
                // this subtree is not the final emitted document.
                encode::protect_conflicting_key_comments(&mut value, 4, false)?;
                Ok((encode::Value::str(&key.value), encode::Value::Node(value)))
            })
            .collect::<Result<_, YamlError>>()?,
    ))
}

/// The OAuth maps whose keys follow the settings exactly.
const OAUTH_MAPS: [&str; 4] = [
    "oauth-excluded-models",
    "oauth-model-alias",
    "oauth-request-scoped-errors",
    "oauth-settings",
];

/// The bytes [`save_preserving_comments`] writes for the file `data`.
pub(crate) fn render_preserving_comments(
    data: &[u8],
    cfg: &Config,
    migrate_v8: bool,
) -> Result<Vec<u8>, SaveError> {
    let (candidate, source) = candidate_preserving_comments(data, cfg, migrate_v8)?;
    let writer = candidate.bytes.clone();
    let proposal = finish_preserving_comments(candidate, &source, cfg, migrate_v8);
    if proposal != writer && !repair_survives_following_saves(&writer, &proposal) {
        return Ok(writer);
    }
    Ok(proposal)
}

fn finish_preserving_comments(
    candidate: tree::ReceiptedYaml,
    source: &tree::CommentSource,
    cfg: &Config,
    migrate_v8: bool,
) -> Vec<u8> {
    tree::finish_comment_owners(source, candidate, |data| {
        let (candidate, source) = candidate_preserving_comments(data, cfg, migrate_v8).ok()?;
        Some(tree::finish_comment_owners_replay(&source, candidate))
    })
}

/// The following saves and legacy-to-v8 layout step must retain the proposal's
/// comment copies up to the number that step keeps at the writer's placement.
/// Preview the unmodified writer candidate: an owner repair can only restore
/// its source copies, so this is a lower bound on the protected next step.
/// This also avoids depending on a repair the next step might itself reject.
fn repair_survives_following_saves(writer: &[u8], proposal: &[u8]) -> bool {
    let Some(counts) = tree::physical_comment_counts(proposal) else {
        return false;
    };
    let Ok(doc) = unmarshal(proposal) else {
        return false;
    };
    let legacy_layout = doc
        .content
        .first()
        .and_then(|root| yaml_path(root, "config-version"))
        .is_none_or(|version| version.tag != super::yaml3::INT_TAG || version.value != "8");
    let Ok(config) = Config::parse(proposal) else {
        return false;
    };
    if !following_saves_keep_comments(writer, proposal, &counts, &config, legacy_layout) {
        return false;
    }
    // Layout normalization acts on the stored file itself, before an edit.
    // Use its bare writer too: calling the protected entry point here would
    // make a layout repair recursively preview itself.
    !legacy_layout
        || v8::normalize_config_layout_writer(proposal, true).is_ok_and(|(output, _)| {
            raw_preview_keeps_comment_counts(&counts, &output, || {
                v8::normalize_config_layout_writer(writer, true).map(|(bytes, _)| bytes)
            })
        })
}

fn following_saves_keep_comments(
    writer: &[u8],
    proposal: &[u8],
    counts: &std::collections::HashMap<&[u8], usize>,
    config: &Config,
    legacy_layout: bool,
) -> bool {
    // With an explicit v8 marker both flags take the same migration branch.
    let flags: &[bool] = if legacy_layout {
        &[false, true]
    } else {
        &[true]
    };
    for &migrate in flags {
        let Ok((candidate, _)) = candidate_preserving_comments(proposal, config, migrate) else {
            return false;
        };
        if !raw_preview_keeps_comment_counts(counts, &candidate.bytes, || {
            candidate_preserving_comments(writer, config, migrate)
                .map(|(candidate, _)| candidate.bytes)
        }) {
            return false;
        }
    }
    true
}

fn candidate_preserving_comments(
    data: &[u8],
    cfg: &Config,
    migrate_v8: bool,
) -> Result<(tree::ReceiptedYaml, tree::CommentSource), SaveError> {
    let (mut original, mut source) = tree::read_comment_source(data)?;
    source.repair_key_feet = true;
    if original.kind != Kind::Document || original.content.is_empty() {
        return Err(invalid("invalid yaml document structure"));
    }
    let Some(first) = original.content.first_mut() else {
        return Err(invalid("invalid yaml document structure"));
    };
    if first.kind != Kind::Mapping {
        return Err(invalid("expected root mapping node"));
    }
    check_layout(data)?;
    let layout = expand_config_aliases(first)?;
    let flat = v8::flatten_v8(&layout)?;
    *first = flat;
    let root = first;
    let migrating = migrate_v8 || is_v8_config_layout(&layout)?;
    let generated = generated_root(cfg, yaml_path(&layout, "plugins.configs"))?;

    // Keep obsolete roots until a v8 migration can keep them as comments.
    if !migrating {
        merge::remove_legacy_auth_block(root);
        merge::remove_removed_integration_keys(root);
        merge::remove_legacy_generative_language_keys(root);
    }
    merge::remove_legacy_openai_compat_api_keys(root);
    for key in OAUTH_MAPS {
        merge::prune_mapping_to_generated_keys(root, &generated, key);
    }
    merge::prune_routing_quota(root, &generated);
    merge::prune_mapping_to_generated_keys(root, &generated, "self-update");
    merge::merge_mapping_preserve(root, &generated, &mut Vec::new());
    v8::restore_v8_layout(root, &layout, data, cfg)?;
    if !migrating {
        // Keep the earlier client paths where the file had them, so the next
        // save of a legacy file doesn't look like a v8 one.
        for &(old, current) in loader_v8::V8_CLIENT_PATHS {
            if yaml_path(&layout, old).is_none()
                || yaml_path(&layout, current).is_some()
                || yaml_path(root, current).is_none()
            {
                continue;
            }
            if let Some(copy) = copy_yaml_path_value(root, current) {
                delete_yaml_path(root, current);
                set_yaml_path_with_comments(root, old, &copy);
            }
        }
    }
    merge::normalize_collection_node_styles(root);
    if migrating {
        v8::retain_version_comment_owner(root)?;
    }

    source.include_copies(&original);
    let mut out = tree::normalize_receipted(tree::marshal_config_for_source(&original, &source)?);
    if migrating {
        out = normalize_config_layout_receipted(out, true)?;
    }
    Ok((out, source))
}

/// Stable is the default, so older binaries must not encounter this new key.
pub(crate) fn omit_stable_update_channel(root: &mut Node) -> bool {
    let stable = yaml_path(root, "self-update.channel").is_some_and(|node| {
        node.kind == Kind::Scalar
            && (node.tag == NULL_TAG
                || (node.tag == STR_TAG
                    && super::SelfUpdateChannel::parse(&node.value)
                        == Some(super::SelfUpdateChannel::Stable)))
    });
    stable && delete_yaml_path(root, "self-update.channel")
}

/// The bytes [`update_nested_scalar`] writes for the file `data`.
pub(crate) fn render_nested_scalar(
    data: &[u8],
    keys: &[&str],
    value: &str,
) -> Result<Vec<u8>, SaveError> {
    let (candidate, source) = candidate_nested_scalar(data, keys, value)?;
    let writer = candidate.bytes.clone();
    let proposal = tree::finish_comment_owners(&source, candidate, |data| {
        let (candidate, source) = candidate_nested_scalar(data, keys, value).ok()?;
        Some(tree::finish_comment_owners_replay(&source, candidate))
    });
    if proposal != writer && !repair_survives_following_saves(&writer, &proposal) {
        return Ok(writer);
    }
    Ok(proposal)
}

fn candidate_nested_scalar(
    data: &[u8],
    keys: &[&str],
    value: &str,
) -> Result<(tree::ReceiptedYaml, tree::CommentSource), SaveError> {
    let (mut doc, mut source) = tree::read_comment_source(data)?;
    if doc.kind != Kind::Document || doc.content.is_empty() {
        return Err(invalid("invalid yaml document structure"));
    }
    // `root.Decode(&map[string]any)`: duplicate keys, merge rules and a
    // root that isn't a mapping fail.
    if let Some(root) = loader_root(data)? {
        if root.is_mapping() {
            loader_yaml::check_shape(&root)
                .map_err(|error| SaveError::new(SaveErrorKind::Yaml, error.message()))?;
        } else if root.tag != NULL_TAG {
            return Err(SaveError::new(
                SaveErrorKind::Yaml,
                format!(
                    "yaml: unmarshal errors:\n  {}",
                    loader_yaml::type_error(&root, "map[string]interface {}")
                ),
            ));
        }
    }
    let Some(first) = doc.content.first_mut() else {
        return Err(invalid("invalid yaml document structure"));
    };
    *first = expand_config_aliases(first)?;
    let mut node = first;
    let last = keys.len().saturating_sub(1);
    for (index, key) in keys.iter().enumerate() {
        let next = get_or_create_map_value(node, key);
        if index == last {
            next.kind = Kind::Scalar;
            STR_TAG.clone_into(&mut next.tag);
            value.clone_into(&mut next.value);
        } else if next.kind != Kind::Mapping {
            next.kind = Kind::Mapping;
            MAP_TAG.clone_into(&mut next.tag);
        }
        node = next;
    }
    if let Some(root) = doc.content.first_mut() {
        omit_stable_update_channel(root);
    }
    let intended = IntendedValues::node(&doc, false)?;
    source.include_copies(&doc);
    let output = tree::normalize_receipted(tree::marshal_config_for_source(&doc, &source)?);
    check_values(&output.bytes, &intended)?;
    Ok((output, source))
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn multiline_values_in_every_section_keep_block_style_and_reload() {
        fn find<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
            if node.kind == Kind::Mapping
                && let Some(i) = find_map_key_index(node, key)
            {
                return node.content.get(i + 1);
            }
            node.content.iter().find_map(|child| find(child, key))
        }
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/block-scalar-every-section.json"))
                .unwrap();
        for case in fixtures.as_array().unwrap() {
            let source = case["Source"].as_str().unwrap();
            let op = case["Op"].as_str().unwrap();
            let actual = if op == "nested" {
                render_nested_scalar(source.as_bytes(), &["auth-dir"], "/x").unwrap()
            } else {
                render_preserving_comments(
                    source.as_bytes(),
                    &Config::parse(source).unwrap(),
                    op == "migrate",
                )
                .unwrap()
            };
            assert_eq!(
                std::str::from_utf8(&actual).unwrap(),
                case["File"].as_str().unwrap(),
                "{} {op}",
                case["Name"]
            );
            Config::load_bytes(&actual).unwrap();
            let saved = root(&actual);
            let key = match case["Name"].as_str().unwrap() {
                "typed-payload" => "prompt",
                "retained-pprof" => "note",
                _ => "x-note",
            };
            if matches!(key, "x-note" | "note") && op == "migrate" {
                // Go archives unknown keys, including pprof's unknown note.
                assert!(find(&saved, key).is_none());
                assert!(String::from_utf8_lossy(&actual).lines().any(|line| line.starts_with("# ") && line[2..].trim_start() == "# Heading"));
            } else {
                let value = find(&saved, key).unwrap();
                assert_eq!(value.value, "before\n# Heading\nafter");
                assert!(value.style.has(Style::LITERAL));
            }
        }
    }

    use super::tree::set_yaml_path;
    use super::*;
    use crate::config::AnyValue;
    use crate::config::testing::TempDir;
    use crate::config::yaml3::Style;

    fn root(data: &[u8]) -> Node {
        let doc = unmarshal(data).expect("yaml");
        doc.content.first().cloned().expect("root")
    }

    fn scalar<'a>(root: &'a Node, path: &str) -> Option<&'a str> {
        yaml_path(root, path).map(|node| node.value.as_str())
    }

    #[test]
    fn raw_write_preserves_plugin_keep_chomped_text() {
        for style in [">+", "|+"] {
            let source = format!(
                "config-version: 8\nplugins:\n  configs:\n    example:\n      text: {style}\n        fictional note\n\n\n      # fictional footer\n"
            );
            let dir = TempDir::new();
            let path = dir.path().join("config.yaml");
            write_file(&path, source.as_bytes()).expect("write without changing the plugin text");
            for step in 0..=3 {
                let bytes = fs::read(&path).unwrap();
                Config::load_bytes(&bytes).expect("written config loads");
                assert_eq!(
                    scalar(&root(&bytes), "plugins.configs.example.text"),
                    Some("fictional note\n\n\n")
                );
                assert!(String::from_utf8_lossy(&bytes).contains("# fictional footer"));
                if step < 3 {
                    let cfg = Config::parse(&bytes).unwrap();
                    save_preserving_comments(&path, &cfg, false).expect("save preserves the text");
                }
            }
        }
    }

    #[test]
    fn raw_write_preserves_keep_chomped_text_before_typed_normalization() {
        // The old guard accepted this setting because its loader trims it.
        // The shared emitter also changes those previously accepted bytes.
        let source = b"config-version: 8\nmanagement:\n  panel-github-repository: >+\n    https://panel.example/fictional\n\n\n  # fictional footer\n";
        let dir = TempDir::new();
        let path = dir.path().join("config.yaml");
        write_file(&path, source).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(
            scalar(&root(&bytes), "management.panel-github-repository"),
            Some("https://panel.example/fictional\n\n\n")
        );
        assert_eq!(bytes, b"config-version: 8\nmanagement:\n    panel-github-repository: >+\n        https://panel.example/fictional\n\n\n# fictional footer\n");
    }

    #[test]
    fn plugin_mapping_line_comment_reaches_generated_auth_dir() {
        // The exact final save from config-save/random-6-2628, including
        // upstream's complete expected file. A reduced document can flush
        // the pending comment differently and miss this regression.
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/plugin-anchor-comment-save.json"))
                .unwrap();
        let cfg = Config::parse(fixture["config"].as_str().unwrap()).expect("config");
        let output = render_preserving_comments(
            fixture["file"].as_str().unwrap().as_bytes(),
            &cfg,
            fixture["migrate"].as_bool().unwrap(),
        )
        .expect("save");
        assert_eq!(
            String::from_utf8(output).unwrap(),
            fixture["expected"].as_str().unwrap()
        );
    }

    #[test]
    fn retained_plugin_scalars_and_comments_survive_saving() {
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../testdata/plugin-lossy-reencode-save.json"))
                .unwrap();
        fn data(node: &Node) -> serde_json::Value {
            serde_json::json!([
                format!("{:?}", node.kind),
                node.tag,
                node.value,
                node.content.iter().map(data).collect::<Vec<_>>()
            ])
        }
        fn comments(node: &Node, out: &mut Vec<String>) {
            for comment in [&node.head_comment, &node.line_comment, &node.foot_comment] {
                out.extend(comment.lines().map(str::to_owned));
            }
            for child in &node.content {
                comments(child, out);
            }
        }
        fn settings(mut cfg: Config) -> Config {
            // A migration records OAuth scope on reload; it also sorts map
            // entries. Neither changes the configured values being checked.
            cfg.oauth_only_fields.clear();
            for rules in [
                &mut cfg.payload.default,
                &mut cfg.payload.default_raw,
                &mut cfg.payload.r#override,
                &mut cfg.payload.override_raw,
            ] {
                for rule in rules {
                    rule.params.sort_by(|a, b| a.0.cmp(&b.0));
                }
            }
            cfg
        }
        for case in &fixtures {
            let dir = TempDir::new();
            let path = dir.join("config.yaml");
            let source = case["options"]["file"].as_str().unwrap();
            fs::write(&path, source).unwrap();
            let original = expand_config_aliases(&root(source.as_bytes())).unwrap();
            let plugin = yaml_path(&original, "plugins.configs").unwrap();
            let mut plugin_comments = Vec::new();
            comments(plugin, &mut plugin_comments);
            let mut outputs = Vec::new();
            for step in case["options"]["steps"].as_array().unwrap() {
                let before = fs::read_to_string(&path).unwrap();
                let mut cfg = Config::parse(step["config"].as_str().unwrap_or(&before)).unwrap();
                match step["op"].as_str().unwrap() {
                    "save" => save_preserving_comments(
                        &path,
                        &cfg,
                        step["migrate"].as_bool().unwrap_or(false),
                    )
                    .unwrap(),
                    "nested" => {
                        assert_eq!(step["keys"], serde_json::json!(["models", "catalog"]));
                        let value = step["value"].as_str().unwrap();
                        update_nested_scalar(&path, &["models", "catalog"], value).unwrap();
                        cfg.models.catalog = value.to_owned();
                    }
                    op => panic!("unexpected fixture step {op}"),
                }
                let actual = fs::read_to_string(&path).unwrap();
                assert_eq!(settings(Config::parse(&actual).unwrap()), settings(cfg));
                let saved = root(actual.as_bytes());
                assert_eq!(
                    data(yaml_path(&saved, "plugins.configs").unwrap()),
                    data(plugin)
                );
                for comment in &plugin_comments {
                    let occurrences = actual
                        .match_indices(comment)
                        .filter(|(at, _)| {
                            actual
                                .get(at + comment.len()..)
                                .is_some_and(|rest| rest.is_empty() || rest.starts_with('\n'))
                        })
                        .count();
                    assert_eq!(occurrences, 1, "{}: {comment}", case["case"]);
                }
                outputs.push(actual);
            }
            assert_eq!(
                serde_json::json!({"files":outputs}),
                case["open_ferry_output"]
            );
        }
    }

    #[test]
    fn retained_plugin_block_scalar_encoding_preserves_data_and_comments() {
        for style in [">-", "|-"] {
            let source = format!("tags: &s # moved\n  - {style} # own\n    a\n  - b\n");
            let mut doc = unmarshal(source.as_bytes()).unwrap();
            doc.content[0] = expand_config_aliases(&doc.content[0]).unwrap();
            assert_eq!(
                normalize_comment_indentation(&marshal_config(&doc).unwrap()),
                b"tags:\n  - \"a\" # moved\n# own\n  - b\n"
            );
        }
        let doc = unmarshal(b"tags:\n  - |- # own\n    # data\n    a\n").unwrap();
        assert_eq!(
            normalize_comment_indentation(&marshal_config(&doc).unwrap()),
            b"tags:\n  - |- # own\n    # data\n    a\n"
        );
        let doc = unmarshal(b"tags:\n  - >- # own\n    a\n").unwrap();
        assert_eq!(
            normalize_comment_indentation(&marshal_config(&doc).unwrap()),
            b"tags:\n  - >- # own\n    a\n"
        );
    }

    #[test]
    fn nested_updates_preserve_retained_plugin_block_scalars() {
        let source = b"plugins:\n  configs:\n    alpha:\n      tags: &s # moved\n        - >- # own\n          a\ndebug: false\n";
        let actual = tree::without_owner_protection(|| {
            render_nested_scalar(
                source,
                &["models", "catalog"],
                "https://catalog.example/models.json",
            )
        })
        .unwrap();
        assert_eq!(actual, b"plugins:\n  configs:\n    alpha:\n      tags:\n        - \"a\" # moved\n# own\ndebug: false\nmodels:\n  catalog: https://catalog.example/models.json\n");
        assert_eq!(
            Config::parse(&actual).unwrap().models.catalog,
            "https://catalog.example/models.json"
        );
    }

    #[test]
    fn pending_plugin_comments_match_go_across_layouts_and_following_keys() {
        // The review's S1 example is also a double write in Go: restoring
        // source key comments reintroduces the comment after replacement.
        // These complete outputs include legacy, v8 and migrating saves.
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../testdata/plugin-pending-comment-neighbors.json"
        ))
        .unwrap();
        for case in cases {
            let cfg = Config::parse(case["config"].as_str().unwrap()).unwrap();
            let run = || {
                render_preserving_comments(
                    case["file"].as_str().unwrap().as_bytes(),
                    &cfg,
                    case["migrate"].as_bool().unwrap(),
                )
            };
            let actual = if super::owner_legacy_tests::has_frozen_main_control(
                "pending",
                case["file"].as_str().unwrap(),
                if case["migrate"] == true {
                    "migrate"
                } else {
                    "save"
                },
            ) {
                tree::without_owner_protection(run)
            } else {
                run()
            }
            .unwrap();
            assert_eq!(
                String::from_utf8(actual).unwrap(),
                case["expected"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn null_plugins_follow_go_mapping_and_pending_comment_encoding() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../testdata/plugin-null-generated-comments.json"
        ))
        .unwrap();
        for case in cases {
            let layout =
                expand_config_aliases(&root(case["file"].as_str().unwrap().as_bytes())).unwrap();
            let configs =
                generated_plugin_configs(yaml_path(&layout, "plugins.configs").unwrap()).unwrap();
            let value = encode::Value::Struct(vec![
                encode::Field::new(
                    "plugins",
                    encode::Value::Struct(vec![encode::Field::new("configs", configs)]),
                ),
                encode::Field::new("auth-dir", encode::Value::str("/x")),
            ]);
            let actual = String::from_utf8(encode::marshal(&value).unwrap()).unwrap();
            assert_eq!(actual, case["expected"].as_str().unwrap());
            // Go defers the pending comment onto zeta's key before the empty
            // mapping; retaining the null scalar would consume it there.
            assert!(actual.contains("zeta: # moved\n{}\nauth-dir: /x\n"));
            assert!(!actual.contains("null comment"));
        }
    }

    #[test]
    fn plugin_alias_comments_match_go_config_decoder_expansion() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../testdata/plugin-alias-comment-save.json"))
                .unwrap();
        for case in cases {
            let cfg = Config::parse(case["config"].as_str().unwrap()).unwrap();
            let run = || {
                render_preserving_comments(case["file"].as_str().unwrap().as_bytes(), &cfg, false)
            };
            let actual = if super::owner_legacy_tests::has_frozen_main_control(
                "aliases",
                case["file"].as_str().unwrap(),
                "save",
            ) {
                tree::without_owner_protection(run)
            } else {
                run()
            }
            .unwrap();
            assert_eq!(
                String::from_utf8(actual).unwrap(),
                case["expected"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    /// Checks that `data` is a valid v8 config (upstream's
    /// `ValidateV8Config`).
    fn assert_valid_v8(data: &[u8]) {
        if let Err(error) = crate::config::v8_edit::validate_v8_config(data) {
            panic!("{error}\n{}", String::from_utf8_lossy(data));
        }
    }

    #[test]
    fn routing_keys_follow_upstreams_field_order() {
        let mut cfg = Config::default();
        cfg.routing.strategy = "round-robin".into();
        cfg.routing.session_affinity = true;
        cfg.routing.session_affinity_ttl = "1h".into();
        cfg.routing.session_affinity_subagents = Some(false);
        cfg.routing.session_affinity_rebind_on = "quota".into();
        let generated = generated_root(&cfg, None).expect("generate");
        let routing = yaml_path(&generated, "routing").expect("routing");
        let keys: Vec<_> = routing
            .content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|entry| entry[0].value.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "strategy",
                "session-affinity",
                "session-affinity-ttl",
                "session-affinity-subagents",
                "session-affinity-rebind-on"
            ]
        );
    }

    #[test]
    fn legacy_save_unquotes_mapping_keys_and_keeps_value_quotes() {
        let raw = br#""port": 8317
"custom": {"label": "quoted value", "true": "quoted boolean"}
"#;
        let cfg = Config::parse(raw).expect("parse");
        let dir = TempDir::new();
        let path = dir.write("config.yaml", raw);
        save_preserving_comments(&path, &cfg, false).expect("save");
        let saved = fs::read_to_string(path).expect("read");
        assert!(saved.contains("port: 8317\n"), "{saved}");
        assert!(
            saved.contains("custom:\n  label: \"quoted value\"\n  \"true\": \"quoted boolean\"\n"),
            "{saved}"
        );
        assert!(!saved.contains("config-version:"), "{saved}");
        assert!(!saved.contains('{'), "{saved}");
    }

    #[test]
    fn new_keys_are_inserted_at_their_v8_paths() {
        let raw = b"config-version: 8\nserver: {port: 8317}\napi-keys:\n  claude: [{keys: [{api-key: test}]}]\n  vertex: [{keys: [{api-key: test}]}]\n";
        let mut cfg = Config::parse(raw).expect("parse");
        cfg.max_request_body_bytes = 1048576;
        cfg.routing.session_affinity_rebind_on = "quota".into();
        cfg.streaming.send_keepalive_before_ft = true;
        cfg.claude.tool_search = true;
        cfg.claude_api_key[0].native_tool_search = true;
        cfg.vertex_api_key[0].interactions = Some(false);
        let dir = TempDir::new();
        let path = dir.write("config.yaml", raw);
        save_preserving_comments(&path, &cfg, false).expect("save");
        let saved = fs::read(path).expect("read");
        assert_valid_v8(&saved);
        let root = root(&saved);
        for (path, value) in [
            ("server.max-request-body-bytes", "1048576"),
            ("routing.session-affinity-rebind-on", "quota"),
            ("requests.streaming.send-keepalive-before-ft", "true"),
            ("upstream.claude.tool-search", "true"),
        ] {
            assert_eq!(scalar(&root, path), Some(value), "{path}");
        }
        for (provider, key, value) in [
            ("claude", "native-tool-search", "true"),
            ("vertex", "interactions", "false"),
        ] {
            let group = &yaml_path(&root, &format!("api-keys.{provider}"))
                .expect("provider")
                .content[0];
            let key_node = &yaml_path(group, "keys").expect("keys").content[0];
            assert_eq!(scalar(key_node, key), Some(value), "{provider}.{key}");
        }
        for legacy in [
            "max-request-body-bytes",
            "streaming",
            "claude",
            "claude-api-key",
            "vertex-api-key",
        ] {
            assert!(yaml_path(&root, legacy).is_none(), "{legacy}");
        }
        let mut reloaded = Config::parse(&saved).expect("reload");
        // Completing the v8 layout moves ws-auth into oauth.providers,
        // recomputing layout metadata without changing setting values.
        reloaded.oauth_only_fields.clear();
        cfg.oauth_only_fields.clear();
        assert_eq!(reloaded, cfg);
    }

    #[test]
    fn thinking_disable_type_survives_both_layouts_and_migration() {
        for raw in [
            "claude-api-key: [{api-key: dummy, models: [{name: claude-sonnet-5-5, alias: sonnet, thinking: {levels: [low, none], disable-type: between_tools}}]}]\n",
            "config-version: 8\napi-keys:\n  claude:\n    - models: [{name: claude-sonnet-5-5, alias: sonnet, thinking: {levels: [low, none], disable-type: between_tools}}]\n      keys: [{api-key: dummy}]\n",
        ] {
            let dir = TempDir::new();
            let file = dir.path().join("config.yaml");
            fs::write(&file, raw).unwrap();
            let mut cfg = Config::load(&file).unwrap();
            assert_eq!(
                cfg.claude_api_key[0].models[0]
                    .thinking
                    .as_ref()
                    .unwrap()
                    .disable_type,
                "between_tools"
            );
            for migrate in [false, true] {
                save_preserving_comments(&file, &cfg, migrate).unwrap();
                cfg = Config::load(&file).unwrap();
                assert_eq!(
                    cfg.claude_api_key[0].models[0]
                        .thinking
                        .as_ref()
                        .unwrap()
                        .disable_type,
                    "between_tools"
                );
            }
        }
    }

    #[test]
    fn web_search_cache_saves_defaults_updates_and_migrates_both_layouts() {
        for layout in ["port: 0\n", "config-version: 8\n"] {
            let dir = TempDir::new();
            let file = dir.path().join("config.yaml");
            fs::write(&file, layout).unwrap();
            let mut cfg = Config::load(&file).unwrap();
            for (enabled, ttl, limit) in [(true, "30m", 128), (false, "24h", 1024), (false, "", 0)]
            {
                cfg.client.codex.web_search_result_cache =
                    crate::config::CodexWebSearchResultCacheConfig {
                        enabled,
                        ttl: ttl.into(),
                        limit,
                    };
                for migrate in [false, true] {
                    save_preserving_comments(&file, &cfg, migrate).unwrap();
                    let loaded = Config::load(&file).unwrap();
                    assert_eq!(
                        loaded.client.codex.web_search_result_cache,
                        cfg.client.codex.web_search_result_cache
                    );
                }
            }
        }
    }

    // Ports TestV8MovedCommentsSurviveV0Saves (config_v8_comments_test.go).
    #[test]
    fn v8_moved_comments_survive_v0_saves() {
        let raw = "# DOCUMENT HEAD\nconfig-version: 8\noauth: # OAUTH INLINE\n  providers: # PROVIDERS INLINE\n    xai: # PROVIDER INLINE\n      # FIELD HEAD\n      inject-x-search: true # FIELD INLINE\n\n      # FIELD FOOT\n\n    # PROVIDER FOOT\nserver:\n  port: 8317 # UNRELATED INLINE\n# DOCUMENT FOOT\n";
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, raw).expect("seed");
        let mut cfg = Config::load(&file).expect("load");
        for port in [8318, 8319, 8320] {
            cfg.port = port;
            save_preserving_comments(&file, &cfg, false).expect("save");
            let data = fs::read_to_string(&file).expect("read");
            assert_valid_v8(data.as_bytes());
            for marker in [
                "DOCUMENT HEAD",
                "OAUTH INLINE",
                "PROVIDERS INLINE",
                "PROVIDER INLINE",
                "FIELD HEAD",
                "FIELD INLINE",
                "FIELD FOOT",
                "PROVIDER FOOT",
                "UNRELATED INLINE",
                "DOCUMENT FOOT",
            ] {
                assert_eq!(data.matches(marker).count(), 1, "{marker}:\n{data}");
            }
            let port_text = port.to_string();
            assert_eq!(
                scalar(&root(data.as_bytes()), "server.port"),
                Some(port_text.as_str())
            );
        }
    }

    // Ports TestV8MigrationCarriesProviderKeyFootComments
    // (config_v8_comments_test.go).
    #[test]
    fn v8_migration_carries_provider_key_foot_comments() {
        let mut doc =
            unmarshal(b"oauth: {providers: {xai: {inject-x-search: true}}}\n").expect("yaml");
        let root = doc.content.first_mut().expect("root");
        let provider = yaml_path_mut(root, "oauth.providers").expect("providers");
        let index = find_map_key_index(provider, "xai").expect("xai");
        provider.content[index].foot_comment = "# PROVIDER KEY FOOT".to_owned();
        let field = yaml_path_mut(root, "oauth.providers.xai").expect("xai");
        let index = find_map_key_index(field, "inject-x-search").expect("field");
        field.content[index].foot_comment = "# FIELD KEY FOOT".to_owned();
        let raw = marshal(&doc).expect("marshal");
        let raw_text = String::from_utf8_lossy(&raw).into_owned();
        for marker in ["PROVIDER KEY FOOT", "FIELD KEY FOOT"] {
            assert!(raw_text.contains(marker), "fixture lacks {marker}");
        }
        let (data, _) = normalize_config_layout(&raw, true).expect("normalize");
        let data = String::from_utf8(data).expect("utf-8");
        for marker in ["PROVIDER KEY FOOT", "FIELD KEY FOOT"] {
            assert_eq!(data.matches(marker).count(), 1, "{marker}:\n{data}");
        }
    }

    // Ports TestIsV8ConfigLayout (config_v8_save_layout_test.go).
    #[test]
    fn is_v8_config_layout_table() {
        let cases = [
            (
                "legacy",
                "request-retry: 3\ncodex: {response-steering: true}\n",
                false,
            ),
            (
                "legacy common roots",
                "routing: {strategy: fill-first}\nplugins: {configs: {example: {enabled: true}}}\nquota-exceeded: {switch-project: true}\nclient: {codex: {enable-apply-patch: true}}\napi-keys: [client-key]\n",
                false,
            ),
            (
                "legacy optimize alias",
                "codex: {optimize-multi-agent-v2: true}\n",
                false,
            ),
            ("declared v8", "config-version: 8\nrequest-retry: 3\n", true),
            (
                "historical v8",
                "oauth: {providers: {codex: {response-steering: true}}}\n",
                true,
            ),
            ("empty v8", "server: {}\n", true),
            (
                "latest v8",
                "upstream: {xai: {inject-x-search: false}}\n",
                true,
            ),
            ("grouped credentials", "api-keys: {}\n", true),
            (
                "partial v8 retry",
                "request-retry: 3\nrouting: {retry: {max-retry-credentials: 2}}\n",
                true,
            ),
            ("empty v8 retry", "routing: {retry: {}}\n", true),
            (
                "historical client",
                "providers: {codex: {optimize-multi-agent-v2: true}}\n",
                true,
            ),
            (
                "latest client",
                "client: {codex: {optimize-multi-agent-v2: false}}\n",
                true,
            ),
        ];
        for (name, raw, v8) in cases {
            let got = is_v8_config_layout(&root(raw.as_bytes())).expect("layout");
            assert_eq!(got, v8, "{name}");
        }
    }

    // Ports TestV0SaveUpgradesHistoricalV8Layout
    // (config_v8_save_layout_test.go), but for its Home mode part, which
    // isn't ported, and its check of the OAuth scope, which the loader's
    // tests cover.
    #[test]
    fn v0_save_upgrades_historical_v8_layout() {
        for version in ["", "config-version: 8\n"] {
            let dir = TempDir::new();
            let file = dir.path().join("config.yaml");
            let raw = format!(
                "{version}# Preserve provider settings\noauth: {{providers: {{codex: {{response-steering: true, header-defaults: {{user-agent: oauth-agent}}}}, xai: {{inject-x-search: true}}}}}}\n"
            );
            fs::write(&file, &raw).expect("seed");
            let mut cfg = Config::load(&file).expect("load");
            cfg.port = 8318;
            save_preserving_comments(&file, &cfg, false).expect("save");
            let data = fs::read(&file).expect("read");
            assert_valid_v8(&data);
            let root = root(&data);
            assert!(
                yaml_path(&root, "upstream.codex.response-steering").is_some(),
                "{version}"
            );
            assert!(
                yaml_path(&root, "upstream.xai.inject-x-search").is_some(),
                "{version}"
            );
            assert_eq!(scalar(&root, "server.port"), Some("8318"), "{version}");
            let text = String::from_utf8_lossy(&data);
            assert!(text.contains("# Preserve provider settings"), "{text}");
        }
    }

    fn bool_node(value: &str, line_comment: &str) -> Node {
        Node {
            kind: Kind::Scalar,
            tag: "!!bool".to_owned(),
            value: value.to_owned(),
            line_comment: line_comment.to_owned(),
            ..Node::default()
        }
    }

    // Ports TestSaveConfigPreserveComments_OAuthSettings
    // (oauth_settings_test.go).
    #[test]
    fn oauth_settings_save_keeps_comments() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(
            &file,
            "config-version: 8\noauth:\n  # Provider model settings\n  settings:\n    codex:\n      - name: \"gpt-6-sol\"\n        max-context-length: 524288\n",
        )
        .expect("seed");
        let mut cfg = Config::load(&file).expect("load");
        let setting = cfg
            .oauth_settings
            .get_mut("codex")
            .and_then(|settings| settings.first_mut())
            .expect("codex setting");
        assert_eq!(setting.max_context_length, 524_288);
        setting.max_context_length = 1_048_576;
        save_preserving_comments(&file, &cfg, false).expect("save");
        let text = fs::read_to_string(&file).expect("read");
        assert!(text.contains("1048576"), "{text}");
        assert!(text.contains("# Provider model settings"), "{text}");
    }

    // Ports TestClientCodexEnableApplyPatchSave (client_test.go).
    #[test]
    fn client_codex_enable_apply_patch_save() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, "config-version: 8\n").expect("seed");
        let mut cfg = Config::default();
        for enabled in [true, false] {
            cfg.client.codex.enable_apply_patch = enabled;
            save_preserving_comments(&file, &cfg, true).expect("save");
            assert_valid_v8(&fs::read(&file).expect("read"));
            let loaded = Config::load(&file).expect("load");
            assert_eq!(loaded.client.codex.enable_apply_patch, enabled);
        }
    }

    // Ports TestClientCodexOptimizeMultiAgentV2Save
    // (client_optimize_test.go).
    #[test]
    fn client_codex_optimize_multi_agent_v2_save() {
        for &(old, _) in loader_v8::V8_CLIENT_PATHS {
            let mut root = Node {
                kind: Kind::Mapping,
                tag: MAP_TAG.to_owned(),
                ..Node::default()
            };
            set_yaml_path(&mut root, old, &bool_node("true", "keep optimize comment"));
            let (parent, last) = old.rsplit_once('.').expect("nested path");
            let parent = yaml_path_mut(&mut root, parent).expect("parent");
            let index = find_map_key_index(parent, last).expect("key");
            parent.content[index].head_comment = "keep optimize heading".to_owned();
            set_yaml_path(
                &mut root,
                "client.codex.enable-apply-patch",
                &bool_node("true", ""),
            );
            let raw = marshal(&root).expect("marshal");
            let dir = TempDir::new();
            let file = dir.path().join("config.yaml");
            fs::write(&file, &raw).expect("seed");
            let mut cfg = Config::load(&file).expect("load");
            for enabled in [true, false, true] {
                cfg.client.codex.optimize_multi_agent_v2 = enabled;
                save_preserving_comments(&file, &cfg, true).expect("save");
                let data = fs::read_to_string(&file).expect("read");
                assert_valid_v8(data.as_bytes());
                let loaded = Config::load(&file).expect("reload");
                assert_eq!(
                    loaded.client.codex.optimize_multi_agent_v2, enabled,
                    "{old}"
                );
                assert!(loaded.client.codex.enable_apply_patch, "{old}");
                assert!(
                    data.contains("keep optimize comment")
                        && data.contains("keep optimize heading"),
                    "{old} ({enabled}): {data}"
                );
            }
        }
    }

    // Ports TestV8SaveCommentsObsoleteSections (config_v8_test.go).
    #[test]
    fn v8_save_comments_obsolete_sections() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(
            &file,
            "auth: {old: true}\nampcode: {old: true}\namp-upstream-url: https://old.example\namp-upstream-api-key: old-secret\ngenerative-language-api-key: old-key\nhome: {enabled: true}\nproxy-url: old\n",
        )
        .expect("seed");
        let cfg = Config::load(&file).expect("load");
        save_preserving_comments(&file, &cfg, true).expect("save");
        let saved = fs::read_to_string(&file).expect("read");
        assert_valid_v8(saved.as_bytes());
        for key in [
            "auth",
            "ampcode",
            "amp-upstream-url",
            "amp-upstream-api-key",
            "generative-language-api-key",
            "home",
        ] {
            assert!(saved.contains(&format!("# {key}:")), "{key}: {saved}");
        }
        assert!(
            saved.contains("# amp-upstream-api-key: old-secret"),
            "{saved}"
        );
    }

    // Ports TestV8MigrationCommentsUnknownLegacySections
    // (config_v8_test.go), but for its Home mode setting, which isn't
    // ported.
    #[test]
    fn v8_migration_comments_unknown_legacy_sections() {
        let raw: &[u8] = b"home:\n  enabled: true\n  host: ignored.example\nenable-gemini-cli-endpoint: true\nforgotten-setting:\n  items: [first, second]\nproxy-url: old\n";
        let (unchanged, changed) = normalize_config_layout(raw, false).expect("read-only");
        assert!(!changed);
        assert_eq!(unchanged, raw);
        let (migrated, changed) = normalize_config_layout(raw, true).expect("migrate");
        assert!(changed);
        assert_valid_v8(&migrated);
        let migrated_root = root(&migrated);
        for key in [
            "home",
            "enable-gemini-cli-endpoint",
            "forgotten-setting",
            "proxy-url",
        ] {
            assert!(yaml_path(&migrated_root, key).is_none(), "{key}");
        }
        let text = String::from_utf8_lossy(&migrated).into_owned();
        for marker in [
            "# home:",
            "#     enabled: true",
            "#     host: ignored.example",
            "# enable-gemini-cli-endpoint: true",
            "# forgotten-setting:",
            "#     items: [first, second]",
        ] {
            assert!(text.contains(marker), "{marker}: {text}");
        }
        assert_eq!(Config::parse(&migrated).expect("parse").proxy_url, "old");
        let (remigrated, _) = normalize_config_layout(&migrated, true).expect("remigrate");
        let text = String::from_utf8_lossy(&remigrated);
        assert_eq!(text.matches("# home:").count(), 1, "{text}");
        assert_eq!(text.matches("# forgotten-setting:").count(), 1, "{text}");
    }

    // Ports TestV8MigrationCommentsUnknownNestedFields (config_v8_test.go),
    // but for its check of `disable-codex-cloaking`, which the config
    // doesn't type.
    #[test]
    fn v8_migration_comments_unknown_nested_fields() {
        let raw: &[u8] = b"server: {port: 8317}\nrouting: {strategy: fill-first, session-affinity: true}\noauth:\n  providers:\n    codex:\n      disable-codex-cloaking: true\n      retired-setting: {mode: old}\n";
        let (unchanged, changed) = normalize_config_layout(raw, false).expect("read-only");
        assert!(!changed);
        assert_eq!(unchanged, raw);
        let (migrated, _) = normalize_config_layout(raw, true).expect("migrate");
        assert_valid_v8(&migrated);
        let text = String::from_utf8_lossy(&migrated).into_owned();
        assert!(
            yaml_path(&root(&migrated), "oauth.providers.codex.retired-setting").is_none(),
            "{text}"
        );
        assert!(
            text.contains("# oauth.providers.codex.retired-setting:"),
            "{text}"
        );
        let cfg = Config::parse(&migrated).expect("parse");
        assert_eq!(cfg.routing.strategy, "fill-first");
        assert!(cfg.routing.session_affinity);
        let (remigrated, _) = normalize_config_layout(&migrated, true).expect("remigrate");
        let text = String::from_utf8_lossy(&remigrated);
        assert_eq!(
            text.matches("# oauth.providers.codex.retired-setting:")
                .count(),
            1,
            "{text}"
        );
    }

    // Ports TestV8MigrationPreservesEmptyLegacyContainers
    // (config_v8_test.go), with the configs compared whole where upstream
    // compares their legacy encodings.
    #[test]
    fn v8_migration_preserves_empty_legacy_containers() {
        let sections = [
            ("tls", "server.tls"),
            ("remote-management", "management"),
            ("pprof", "observability.pprof"),
            ("discovery", "server.discovery"),
            ("discovery.interfaces", "server.discovery.interfaces"),
            ("credential-concurrency", "credentials.concurrency"),
            ("credential-in-flight", "credentials.in-flight"),
            ("streaming", "requests.streaming"),
            ("payload", "requests.payload"),
            ("codex", "oauth.providers.codex"),
            (
                "codex.live-media-relay",
                "oauth.providers.codex.live-media-relay",
            ),
            (
                "codex-header-defaults",
                "oauth.providers.codex.header-defaults",
            ),
            ("claude", "upstream.claude"),
            ("claude-code", "upstream.claude"),
            ("claude-header-defaults", "upstream.claude.header-defaults"),
            ("antigravity", "oauth.providers.antigravity"),
            (
                "antigravity.connection-pool",
                "oauth.providers.antigravity.connection-pool",
            ),
            ("xai", "upstream.xai"),
            ("devin", "oauth.providers.devin"),
        ];
        for (old, current) in sections {
            for empty in ["{}", "null"] {
                let name = format!("{old}/{empty}");
                let mut doc = unmarshal(
                    b"port: 8317\nplugins: {configs: {sample: {enabled: false, options: {}}}}\n",
                )
                .expect("yaml");
                let mut value = root(empty.as_bytes());
                value.line_comment = "Keep this empty block comment".to_owned();
                set_yaml_path(doc.content.first_mut().expect("root"), old, &value);
                let raw = marshal(&doc).expect("marshal");
                let before = Config::parse(&raw).expect("parse");
                let (unchanged, changed) = normalize_config_layout(&raw, false).expect("read-only");
                assert!(!changed && unchanged == raw, "{name}");
                let (migrated, _) = normalize_config_layout(&raw, true).expect("migrate");
                assert_valid_v8(&migrated);
                let after = Config::parse(&migrated).expect("parse migrated");
                assert!(before == after, "{name}: settings changed");
                let migrated_root = root(&migrated);
                assert!(yaml_path(&migrated_root, old).is_none(), "{name}");
                let moved = yaml_path(&migrated_root, current).expect("moved");
                assert!(
                    moved.kind == Kind::Mapping && moved.content.is_empty(),
                    "{name}"
                );
                let text = String::from_utf8_lossy(&migrated);
                assert!(
                    text.contains("Keep this empty block comment"),
                    "{name}: {text}"
                );
            }
        }
    }

    // Ports TestV8EmptyLegacyContainersKeepNewValues (config_v8_test.go),
    // but for its checks of the Codex live media relay and
    // `disable-codex-cloaking`, which the config doesn't type.
    #[test]
    fn v8_empty_legacy_containers_keep_new_values() {
        let raw: &[u8] = b"port: 8317\ntls: null\ncodex: {disable-codex-cloaking: true, live-media-relay: {}}\nserver: {tls: {enable: true, cert: server.crt, key: server.key}}\noauth: {providers: {codex: {live-media-relay: {max-sessions: 12}}}}\n";
        for migrate in [false, true] {
            let (data, _) = normalize_config_layout(raw, migrate).expect("normalize");
            let cfg = Config::parse(&data).expect("parse");
            assert!(cfg.tls.enable, "{migrate}");
            assert_eq!(cfg.tls.cert, "server.crt", "{migrate}");
            assert_eq!(cfg.tls.key, "server.key", "{migrate}");
            let data_root = root(&data);
            assert!(yaml_path(&data_root, "tls").is_none(), "{migrate}");
            assert!(
                yaml_path(&data_root, "codex.live-media-relay").is_none(),
                "{migrate}"
            );
            if !migrate {
                assert!(
                    yaml_path(&data_root, "codex.disable-codex-cloaking").is_some(),
                    "a non-conflicting legacy sibling moved"
                );
            }
            let text = String::from_utf8_lossy(&data);
            assert!(text.contains("max-sessions: 12"), "{text}");
        }
    }

    // Ports TestV8SaveLegacyAdapterAndManualFallback (config_v8_test.go).
    #[test]
    fn v8_save_legacy_adapter_and_manual_fallback() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(
            &file,
            "config-version: 8\nrouting: {retry: {request-retry: 1}}\noauth: {providers: {aistudio: {ws-auth: true}}}\n",
        )
        .expect("seed");
        let mut cfg = Config::load(&file).expect("load");
        cfg.request_retry = 0;
        cfg.ws_auth = false;
        save_preserving_comments(&file, &cfg, false).expect("save");
        let reloaded = Config::load(&file).expect("reload");
        assert_eq!(reloaded.request_retry, 0, "shadowed by stale v8 values");
        assert!(!reloaded.ws_auth, "shadowed by stale v8 values");
        fs::write(
            &file,
            "config-version: 8\nrequest-retry: 5\nws-auth: true\n",
        )
        .expect("seed");
        let reloaded = Config::load(&file).expect("reload");
        assert_eq!(reloaded.request_retry, 5, "manual legacy fallback");
        assert!(reloaded.ws_auth, "manual legacy fallback");
    }

    // Ports TestAPIKeyWeightParsingAndZeroPersistence (weight_test.go).
    #[test]
    fn api_key_weight_zero_persists() {
        let cfg = Config::parse(
            "xai-api-key:\n  - api-key: key\n    base-url: https://api.x.ai/v1\n    weight: 0\n",
        )
        .expect("parse");
        assert_eq!(cfg.xai_api_key.first().and_then(|key| key.weight), Some(0));
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(
            &file,
            "xai-api-key:\n  - api-key: key\n    base-url: https://api.x.ai/v1\n",
        )
        .expect("seed");
        save_preserving_comments(&file, &cfg, false).expect("save");
        let saved = fs::read_to_string(&file).expect("read");
        assert!(saved.contains("weight: 0"), "{saved}");
    }

    // Ports TestSaveConfigPreserveComments_PrunesDefaultPluginsDir
    // (plugin_config_test.go), but with no plugin settings in the config,
    // which doesn't type them.
    #[test]
    fn save_adds_no_default_plugins_dir() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, "debug: true\n").expect("seed");
        let cfg = Config {
            debug: true,
            ..Config::default()
        };
        save_preserving_comments(&file, &cfg, false).expect("save");
        let text = fs::read_to_string(&file).expect("read");
        assert!(!text.contains("plugins:"), "{text}");
        assert!(!text.contains("dir: plugins"), "{text}");
    }

    // Not upstream's: a save keeps the sections the config doesn't type and
    // writes what it changes, with upstream's flow collections in block
    // style and the settings it adds (Go's answer, recorded).
    #[test]
    fn save_keeps_untyped_sections() {
        let raw = "# head\nport: 8317 # the port\nplugins:\n  enabled: false # off\n  configs: {example: {enabled: true}}\npprof:\n  enable: true\n  addr: \"127.0.0.1:6060\"\nunknown: [a, b]\n";
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, raw).expect("seed");
        let mut cfg = Config::load(&file).expect("load");
        cfg.port = 8318;
        save_preserving_comments(&file, &cfg, false).expect("save");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "# head\nport: 8318 # the port\nplugins:\n  enabled: false # off\n  configs:\n    example:\n      enabled: true\npprof:\n  enable: true\n  addr: \"127.0.0.1:6060\"\nunknown:\n  - a\n  - b\ncredential-concurrency:\n  cpa-heartbeat-timeout: 3s\n  cpa-cancel-bound: 5s\n  reclaim-grace: 5s\n  cleanup-interval: 5s\n  release-flush-interval: 250ms\n  release-max-backoff: 2s\n  busy-retry-min: 250ms\n  busy-retry-max: 1s\n  max-limit: 1000000\ncredential-in-flight:\n  snapshot-interval: 2s\n  stale-after: 10s\n  max-part-bytes: 262144\n  max-part-count: 64\n  max-revision-bytes: 16777216\n  max-aggregate-groups: 100000\n  max-details: 10000\n  max-string-bytes: 256\n  staging-retention: 1m\ndiscovery:\n  service-type: _ai-gateway._tcp\n  subtypes:\n    - _chat-completions\n    - _responses\n    - _messages\n    - _generate-content\n    - _interactions\nredis-usage-queue-retention-seconds: 60\ndisable-cooling: false\nrequest-retry: 0\nws-auth: true\n"
        );
        assert_eq!(
            fs::read_to_string(write::backup_path(&file)).expect("backup"),
            raw
        );
    }

    // Ports TestModelCatalogConfigRoundTrip (model_catalogs_test.go): the
    // `models` sources survive a save, with or without migrating to the v8
    // layout, and clearing them saves them cleared.
    #[test]
    fn model_catalog_config_round_trip() {
        for migrate in [false, true] {
            let dir = TempDir::new();
            let file = dir.path().join("config.yaml");
            let catalog = dir.path().join("models.json");
            let sources = crate::config::CatalogSources {
                catalog: catalog.to_string_lossy().into_owned(),
                codex_catalog: "https://example.com/codex.json".to_owned(),
                devin_catalog: "http://localhost/devin.json".to_owned(),
            };
            // yaml.Marshal(map[string]any{"config-version": 8, "models": sources}).
            let data = format!(
                "config-version: 8\nmodels:\n    catalog: {:?}\n    codex-catalog: {}\n    devin-catalog: {}\n",
                sources.catalog, sources.codex_catalog, sources.devin_catalog
            );
            assert_valid_v8(data.as_bytes());
            fs::write(&file, &data).expect("seed");
            let mut cfg = Config::load(&file).expect("load");
            assert_eq!(cfg.models, sources, "migrate {migrate}");
            save_preserving_comments(&file, &cfg, migrate).expect("save");
            let saved = fs::read(&file).expect("read");
            assert_valid_v8(&saved);
            let reloaded = Config::load(&file).expect("reload");
            assert_eq!(reloaded.models, sources, "migrate {migrate}");
            cfg.models = crate::config::CatalogSources::default();
            save_preserving_comments(&file, &cfg, migrate).expect("save cleared");
            let cleared = Config::load(&file).expect("load cleared");
            assert_eq!(cleared.models, crate::config::CatalogSources::default());
        }
    }

    // Not upstream's: a nested update sets the string and keeps the rest,
    // as Go's SaveConfigPreserveCommentsUpdateNestedScalar does (Go's
    // answer, recorded).
    #[test]
    fn nested_scalar_is_set() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(
            &file,
            "# head\nport: 8317 # the port\nremote-management:\n  allow-remote: false\n",
        )
        .expect("seed");
        update_nested_scalar(
            &file,
            &["remote-management", "secret-key"],
            "$2a$10$abcdefghijklmnopqrstuv",
        )
        .expect("update");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "# head\nport: 8317 # the port\nremote-management:\n  allow-remote: false\n  secret-key: $2a$10$abcdefghijklmnopqrstuv\n"
        );
    }

    // Not upstream's: Go writes `port: {inner: value}` here, which its own
    // LoadConfig then refuses ("cannot unmarshal !!map into int"); the
    // load check refuses it before anything is written.
    #[test]
    fn nested_scalar_that_does_not_load_is_refused() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        let raw = "# head\nport: 8317 # the port\ndebug: false\n";
        fs::write(&file, raw).expect("seed");
        let error = update_nested_scalar(&file, &["port", "inner"], "value").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Check);
        assert_eq!(
            error.to_string(),
            "failed to parse config file: yaml: unmarshal errors:\n  line 3: cannot unmarshal !!map into int"
        );
        assert_eq!(fs::read_to_string(&file).expect("read"), raw);
    }

    // Ports TestGitHubTokenConfigRoundTrip (github_token_test.go); that the
    // token stays out of the config JSON is checked by the management API's
    // tests. Not upstream's: the legacy key, and `Debug` hiding the token.
    #[test]
    fn github_token_round_trips() {
        let cfg = Config::parse("server:\n  github-token: test-github-secret\n").expect("parse");
        assert_eq!(cfg.github_token, "test-github-secret");
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        fs::write(&file, "server:\n  github-token: old\n").expect("seed");
        save_preserving_comments(&file, &cfg, false).expect("save");
        let data = fs::read(&file).expect("read");
        assert_eq!(
            scalar(&root(&data), "server.github-token"),
            Some("test-github-secret")
        );
        let reloaded = Config::parse(&data).expect("reparse");
        assert_eq!(reloaded.github_token, cfg.github_token);
        assert_eq!(cfg.clone().github_token, cfg.github_token);
        assert!(!format!("{cfg:?}").contains("test-github-secret"));

        let legacy = Config::parse("github-token: legacy-token\n").expect("legacy");
        assert_eq!(legacy.github_token, "legacy-token");
    }

    // Not upstream's: a management write of a v8 file completes the v8
    // layout and moves comment lines to the start of their line, as
    // WriteConfig does.
    #[test]
    fn write_file_normalizes_a_v8_file() {
        let dir = TempDir::new();
        let file = dir.path().join("config.yaml");
        write_file(&file, b"config-version: 8\nserver:\n  port: 8318\n").expect("write");
        let data = fs::read(&file).expect("read");
        let root = root(&data);
        assert_eq!(scalar(&root, "server.port"), Some("8318"));
        assert_eq!(scalar(&root, "config-version"), Some("8"));
        write_file(&file, b"port: 8319\n    # indented\ndebug: true\n").expect("write");
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "port: 8319\n# indented\ndebug: true\n"
        );
        let error = write_file(&file, b"port: [\n").expect_err("refused");
        assert_eq!(error.kind(), SaveErrorKind::Yaml);
        assert_eq!(
            error.to_string(),
            "yaml: line 1: did not find expected node content"
        );
    }

    // Not upstream's: undo puts back the backup and keeps what it replaces
    // as the new one, so undoing twice is no change.
    #[test]
    fn undo_swaps_the_file_and_its_backup() {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        assert_eq!(
            undo(&path, &UndoCheck::default()).unwrap_err().kind(),
            SaveErrorKind::NoBackup
        );
        write_as_is(
            &path,
            b"port: 1
",
        )
        .unwrap();
        assert_eq!(
            undo(&path, &UndoCheck::default()).unwrap_err().kind(),
            SaveErrorKind::NoBackup
        );
        write_as_is(
            &path,
            b"port: 2
",
        )
        .unwrap();
        assert_eq!(undo(&path, &UndoCheck::default()).unwrap().port, 1);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"port: 1
"
        );
        assert_eq!(
            fs::read(backup_path(&path)).unwrap(),
            b"port: 2
"
        );
        assert_eq!(undo(&path, &UndoCheck::default()).unwrap().port, 2);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"port: 2
"
        );
        // A backup that doesn't load is refused, and nothing changes.
        fs::write(
            backup_path(&path),
            b"port: [
",
        )
        .unwrap();
        assert_eq!(
            undo(&path, &UndoCheck::default()).unwrap_err().kind(),
            SaveErrorKind::Check
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            b"port: 2
"
        );
    }

    // Not upstream's: an undo after a hand edit is refused unless forced,
    // as is one whose file or backup isn't the one the caller saw; a
    // refused undo writes nothing.
    #[test]
    fn undo_checks_what_it_puts_back() {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        write_as_is(&path, b"port: 1\n").unwrap();
        write_as_is(&path, b"port: 2\n").unwrap();
        let edited = b"port: 3\n";
        fs::write(&path, edited).unwrap();
        let refused = undo(&path, &UndoCheck::default()).unwrap_err();
        assert_eq!(refused.kind(), SaveErrorKind::ChangedSince);
        assert_eq!(fs::read(&path).unwrap(), edited);

        // A SHA-256 that isn't the file's or the backup's is refused.
        let wrong = UndoCheck {
            config_sha256: Some(sha256_hex(b"port: 2\n")),
            force: true,
            ..UndoCheck::default()
        };
        assert_eq!(
            undo(&path, &wrong).unwrap_err().kind(),
            SaveErrorKind::Stale
        );
        let wrong = UndoCheck {
            backup_sha256: Some(sha256_hex(b"port: 2\n")),
            force: true,
            ..UndoCheck::default()
        };
        assert_eq!(
            undo(&path, &wrong).unwrap_err().kind(),
            SaveErrorKind::Stale
        );
        assert_eq!(fs::read(&path).unwrap(), edited);

        // Forced, with the right SHA-256s, it goes ahead and keeps the
        // edit as the new backup.
        let check = UndoCheck {
            config_sha256: Some(sha256_hex(edited).to_ascii_uppercase()),
            backup_sha256: Some(sha256_hex(b"port: 1\n")),
            force: true,
        };
        assert_eq!(undo(&path, &check).unwrap().port, 1);
        assert_eq!(fs::read(backup_path(&path)).unwrap(), edited);
        assert_eq!(recorded_sha256(&path), Some(sha256_hex(b"port: 1\n")));

        // With no record, as a file an older writer wrote has, it asks.
        fs::remove_file(record_path(&path)).unwrap();
        assert_eq!(
            undo(&path, &UndoCheck::default()).unwrap_err().kind(),
            SaveErrorKind::ChangedSince
        );
    }

    // Not upstream's: every writer of the config takes the file's lock,
    // the server's (the management API's saves, its v8 edits and its undo)
    // and the commands' alike, before it reads the file. While another
    // holds the lock none writes; each waits, then works from the file as
    // the holder left it: a change made from the file keeps a comment the
    // holder added, an undo puts back what the holder replaced, and a
    // write that expected the bytes from before is refused as stale.
    #[test]
    fn every_writer_takes_the_lock() {
        use crate::config::v8_edit::{V8Edit, V8Method, edit_v8};
        type Writer = Box<dyn FnOnce(&Path) -> Result<(), SaveError> + Send>;
        fn boxed(writer: impl FnOnce(&Path) -> Result<(), SaveError> + Send + 'static) -> Writer {
            Box::new(writer)
        }
        /// What the file holds after a writer, once the holder added a
        /// comment of its own while it waited.
        enum Then {
            Replaced,
            Kept,
            Stale,
            PutBack,
        }
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        write_as_is(&path, b"port: 1\n").unwrap();
        write_as_is(&path, b"port: 2\n").unwrap();
        let writers: Vec<(&str, Writer, Then)> = vec![
            (
                "write_as_is",
                boxed(|path| write_as_is(path, b"port: 3\n")),
                Then::Replaced,
            ),
            (
                "write_file",
                boxed(|path| write_file(path, b"port: 4\n")),
                Then::Replaced,
            ),
            (
                "write_file_expecting",
                boxed(|path| {
                    let read = fs::read(path).unwrap();
                    write_file_expecting(path, b"port: 5\n", &read)
                }),
                Then::Stale,
            ),
            (
                "save_preserving_comments",
                boxed(|path| {
                    let config = Config::load_bytes(b"port: 6\n").unwrap();
                    save_preserving_comments(path, &config, false)
                }),
                Then::Kept,
            ),
            (
                "update_nested_scalar",
                boxed(|path| update_nested_scalar(path, &["remote-management", "secret-key"], "x")),
                Then::Kept,
            ),
            (
                "edit_v8",
                boxed(|path| {
                    let edit = V8Edit {
                        method: V8Method::Put,
                        path: vec!["server".to_owned(), "port".to_owned()],
                        body: b"7".to_vec(),
                        yaml: false,
                    };
                    edit_v8(path, &edit)
                        .map(|_| ())
                        .map_err(|error| SaveError::new(SaveErrorKind::Io, error.to_string()))
                }),
                Then::Kept,
            ),
            (
                "undo",
                boxed(|path| undo(path, &UndoCheck::default()).map(|_| ())),
                Then::PutBack,
            ),
        ];
        for (name, writer, then) in writers {
            let before = fs::read(&path).unwrap();
            let held = WriteLock::acquire(&path).unwrap();
            // The writer before this one noted a wait on every retry, so
            // notes can be left over. Drop them, or `waited` says this
            // writer waited before it has even read the file.
            write::waited(&path);
            let handle = {
                let path = path.clone();
                std::thread::spawn(move || writer(&path))
            };
            let start = std::time::Instant::now();
            while !write::waited(&path) {
                assert!(!handle.is_finished(), "{name} didn't wait for the lock");
                assert!(start.elapsed() < write::LOCK_WAIT, "{name} never waited");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert_eq!(fs::read(&path).unwrap(), before, "{name}");
            // Another write, while it waits.
            let mark = format!("# added while {name} waited");
            let marked = [before.as_slice(), mark.as_bytes(), b"\n"].concat();
            write::commit_locked(&held, &path, &marked, None).unwrap();
            drop(held);
            let result = handle.join().unwrap();
            let now = fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&now).into_owned();
            match then {
                Then::Stale => {
                    let error = result.expect_err(name);
                    assert_eq!(error.kind(), SaveErrorKind::Stale, "{name}: {error}");
                    assert_eq!(now, marked, "{name}");
                    continue;
                }
                _ => {
                    result.unwrap_or_else(|error| panic!("{name}: {error}"));
                }
            }
            match then {
                Then::Replaced => assert!(!text.contains(&mark), "{name}: {text}"),
                Then::Kept => {
                    assert!(text.contains(&mark), "{name}: {text}");
                    assert_ne!(now, marked, "{name}");
                }
                Then::PutBack => assert_eq!(now, before, "{name}"),
                Then::Stale => {}
            }
        }
    }

    // Not upstream's: a write that expects the bytes it was worked out
    // from writes nothing when the file holds others.
    #[test]
    fn write_file_expecting_refuses_a_changed_file() {
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        fs::write(&path, "port: 1\n").unwrap();
        write_file_expecting(&path, b"port: 2\n", b"port: 1\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"port: 2\n");
        let error = write_file_expecting(&path, b"port: 3\n", b"port: 1\n").unwrap_err();
        assert_eq!(error.kind(), SaveErrorKind::Stale);
        assert_eq!(fs::read(&path).unwrap(), b"port: 2\n");
    }

    // Not upstream's: a save that expects the SHA-256 of the contents its
    // config was loaded from writes nothing when the file holds others,
    // since its config doesn't hold what changed there; else it writes and
    // returns the SHA-256 of what it wrote, as the other writes that
    // return one do, and the loader does of what it loaded.
    #[test]
    fn a_save_expecting_a_sha256_refuses_a_changed_file() {
        use crate::config::v8_edit::{V8Edit, V8Method, edit_v8_with_sha256};
        let dir = TempDir::new();
        let path = dir.join("config.yaml");
        fs::write(&path, "port: 1\n").unwrap();
        let (config, loaded) = Config::load_with_sha256(&path).unwrap();
        assert_eq!(loaded, sha256_hex(b"port: 1\n"));
        fs::write(&path, "port: 2\napi-keys: [kept-key]\n").unwrap();
        let error =
            save_preserving_comments_expecting(&path, &config, false, Some(&loaded)).unwrap_err();
        assert_eq!(error.kind(), SaveErrorKind::Stale, "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"port: 2\napi-keys: [kept-key]\n");

        let (mut config, loaded) = Config::load_with_sha256(&path).unwrap();
        config.debug = true;
        let upper = loaded.to_ascii_uppercase();
        let wrote =
            save_preserving_comments_expecting(&path, &config, false, Some(&upper)).unwrap();
        let data = fs::read(&path).unwrap();
        assert_eq!(wrote, sha256_hex(&data));
        let saved = Config::load_bytes(&data).unwrap();
        assert!(saved.debug);
        assert_eq!(saved.api_keys, ["kept-key"]);
        // Without a SHA-256 to expect, it writes whatever the file holds.
        fs::write(&path, "port: 3\n").unwrap();
        let wrote = save_preserving_comments_expecting(&path, &config, false, None).unwrap();
        assert_eq!(wrote, sha256_hex(&fs::read(&path).unwrap()));

        let wrote = write_file_with_sha256(&path, b"port: 4\n").unwrap();
        assert_eq!(wrote, sha256_hex(&fs::read(&path).unwrap()));
        let (config, wrote) = undo_with_sha256(&path, &UndoCheck::default()).unwrap();
        assert!(config.debug);
        assert_eq!(wrote, sha256_hex(&fs::read(&path).unwrap()));
        let edit = V8Edit {
            method: V8Method::Put,
            path: vec!["server".to_owned(), "port".to_owned()],
            body: b"5".to_vec(),
            yaml: false,
        };
        let (config, wrote) = edit_v8_with_sha256(&path, &edit).unwrap();
        assert_eq!(config.port, 5);
        assert_eq!(wrote, sha256_hex(&fs::read(&path).unwrap()));
    }

    // Not upstream's: the defaults are an empty config's settings, in the
    // v8 layout, as the writer writes them.
    #[test]
    fn defaults_are_an_empty_configs_settings() {
        let defaults = defaults().unwrap();
        let value = |path: &str| {
            let parts: Vec<&str> = path.split('.').collect();
            defaults.value(&parts).map(Result::unwrap)
        };
        assert_eq!(value("server.port"), Some(AnyValue::Int(0)));
        assert_eq!(
            value("management.allow-remote"),
            Some(AnyValue::Bool(false))
        );
        assert_eq!(value("routing.strategy"), None);
        assert_eq!(value("config-version"), Some(AnyValue::Int(8)));
    }
}

#[cfg(test)]
mod block_comment_tests;

#[cfg(test)]
mod null_plugin_tests;

#[cfg(test)]
mod archive_tests;
#[cfg(test)]
mod folded_tests;

#[cfg(test)]
mod pending_map_tests;

#[cfg(test)]
mod comment_transfer_tests;

#[cfg(test)]
mod review_r7_tests;

#[cfg(test)]
mod review_r8_tests;
#[cfg(test)]
mod review_r9_foot_tests;
#[cfg(test)]
mod review_r9_unicode_tests;

#[cfg(test)]
mod review_r10_unicode_tests;

#[cfg(test)]
mod review_r10_tests;

#[cfg(test)]
mod review_r10_version_tests;

#[cfg(test)]
mod review_r10_steer_tests;

#[cfg(test)]
mod review_r11_tests;

#[cfg(test)]
mod review_r12_unicode_value_tests;

#[cfg(test)]
mod review_r13_unicode_value_tests;

#[cfg(test)]
mod review_r14_sequence_context_tests;

#[cfg(test)]
mod review_r15_value_tests;

#[cfg(test)]
mod owner_tests;

#[cfg(test)]
mod owner_matrix_tests;

#[cfg(all(test, feature = "parity"))]
mod owner_evidence_tests;

#[cfg(test)]
mod owner_legacy_ops;
#[cfg(test)]
mod owner_legacy_tests;

#[cfg(test)]
mod owner_caller_tests;

#[cfg(test)]
mod owner_writer_controls_tests;
