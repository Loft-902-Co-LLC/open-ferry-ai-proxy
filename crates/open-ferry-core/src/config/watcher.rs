// Ported from CLIProxyAPI internal/watcher (watcher.go, events.go,
// config_reload.go, and the auth-file bookkeeping of clients.go;
// dispatcher.go is replaced by the event channel) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Watching the config file and the auth directory.
//!
//! [`ConfigWatcher::start`] watches both on a background thread and sends a
//! [`WatchEvent`] on a bounded channel for each change, following upstream's
//! rules:
//! - Writes to the config file are debounced: it's reloaded once it has been
//!   quiet for 150 ms, and only if its SHA-256 differs from the last config
//!   that loaded. A config that doesn't load is reported and the caller keeps
//!   the one it has; the next change is tried again.
//! - A `.json` file directly inside the auth directory that is created or
//!   written is reported when its contents are a JSON object and differ from
//!   the last ones seen. Empty files are skipped.
//! - A removed or renamed auth file is checked again after 50 ms (and, if it
//!   was known, up to three more times 25 ms apart), so an atomic replace
//!   counts as a change. Otherwise a known file is reported removed and an
//!   unknown one ignored. Removes of one path are debounced for a second.
//!
//! At start the auth directory is scanned, as upstream's first client load
//! does: each `.json` file that parses is reported as added, and every
//! non-empty one is remembered, so later events with the same contents are
//! skipped.
//!
//! [`ConfigWatcher::reload_config`] reloads the config file at once, as the
//! management API has it reloaded after a save (upstream's
//! `ReloadConfigIfChanged`): on the watcher thread, with the same hash
//! check, so the debounced reload that the save's own write events bring
//! finds nothing new. Its [`WatchEvent::ConfigChanged`], if any, comes
//! before the [`WatchEvent::Reloaded`] that answers it, on the channel that
//! carries every other event, so the consumer never applies a config read
//! before one it already applied. [`ConfigWatcher::force_reload_config`]
//! reloads it the same way without the hash check, so contents loaded before
//! are loaded again (not upstream's): the management API has it reloaded so
//! after a save that found the file changed, which may since hold again
//! what the watcher last loaded. An empty file is still skipped.
//!
//! Each auth event carries a revision from [`next_revision`], taken before
//! the file was read or found gone. The service takes its revisions for the
//! management API's changes from the same counter once they are saved, and
//! skips an event older than a change it has applied to the same
//! credential, as upstream's service skips a stale revision.
//!
//! The watcher stops when the [`ConfigWatcher`] is dropped or the receiver
//! is closed. When the channel is full it waits for the consumer, and still
//! stops if the [`ConfigWatcher`] is dropped meanwhile.
//!
//! The `Debug` of an [`AuthFile`], and of an event carrying one, shows the
//! file's path and the length of its contents, never the credential.
//!
//! Deviations from upstream:
//! - Events go out on a channel; upstream calls a reload callback and builds
//!   and dispatches auth records itself. An auth event carries the contents
//!   the watcher read, hashed and checked, as upstream builds its records
//!   from those bytes, so a write that doesn't parse, made before the
//!   consumer gets to the event, doesn't change what is loaded.
//! - Auth files are read with the credential store's size cap
//!   ([`read_capped`]); a larger file is skipped as one that can't be read.
//! - The directory holding the config file is watched rather than the file,
//!   so an editor that saves by replacing the file is still followed. Events
//!   for other files there are ignored, as upstream ignores them.
//! - The auth directory is fixed at start. When a reloaded config names a
//!   different `auth-dir`, upstream rescans the new directory but goes on
//!   watching the old one; here the consumer restarts the watcher.
//! - A reloaded config is decoded from the bytes that were hashed; upstream
//!   reads the file again to load it, and again afterwards because loading
//!   may rewrite it, which this port never does.
//! - An auth file counts as parseable when it's a JSON object or `null`;
//!   upstream also checks field types against its auth record.
//! - Upstream's mirrored auth directory (from a token store) isn't ported.
//! - Paths are made absolute before watching, as the file watcher reports
//!   them that way, and events reported under a watched directory's canonical
//!   path (macOS does this) are mapped back to the directory as given.
//! - Revisions come from one counter for every credential, and an event's is
//!   taken before the file is read or found gone. Upstream counts each
//!   credential's revisions apart and stamps an update once it is built from
//!   what was read, so a read made just before a management change was saved
//!   could carry the newer revision, and undo the change.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fmt, fs, thread};

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use open_ferry_translate::go::to_lower;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::ConfigError;
use super::load::load_bytes;
use super::paths::{self, Os};
use super::types::Config;
use crate::auth::file_store::read_capped;

/// How long a removed or renamed auth file gets to reappear before it
/// counts as removed.
const REPLACE_CHECK_DELAY: Duration = Duration::from_millis(50);
/// The wait between further checks for a known auth file.
const REPLACE_RETRY_DELAY: Duration = Duration::from_millis(25);
/// How many further checks a known auth file gets.
const REPLACE_RETRIES: usize = 3;
/// How long the config file must be quiet before it's reloaded.
const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
/// Removes of one auth file closer together than this are dropped.
const AUTH_REMOVE_DEBOUNCE_WINDOW: Duration = Duration::from_secs(1);
/// Past this many remembered removes, the stale ones are forgotten.
const REMOVE_TIMES_LIMIT: usize = 128;
/// Events waiting for the consumer.
const EVENT_CAPACITY: usize = 64;
/// How often a send to a full channel checks whether the watcher stopped.
const FULL_CHANNEL_POLL: Duration = Duration::from_millis(10);

/// The last revision handed out by [`next_revision`].
static LAST_REVISION: AtomicU64 = AtomicU64::new(0);

/// A revision for a credential change: above every one handed out before,
/// and never zero (upstream's `authRevisions` counters, made one counter for
/// every credential). The watcher takes one for each auth event before it
/// reads the file or finds it gone; take one for any other change once it
/// is saved, so that a change read before the save has the lower revision.
pub fn next_revision() -> u64 {
    LAST_REVISION
        .fetch_add(1, Ordering::SeqCst)
        .saturating_add(1)
}

/// A change to the config file or the auth directory. An auth event's
/// number is its revision (see [`next_revision`]).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WatchEvent {
    /// The config file changed and loaded: the config, with its
    /// `auth_dir` resolved (`~` expanded, cleaned), as upstream stores it
    /// after a reload, and the SHA-256 of the contents it loaded from, in
    /// lowercase hex (not upstream's).
    ConfigChanged(Arc<Config>, String),
    /// The config file changed but didn't load. Keep the current config.
    ConfigInvalid(ConfigError),
    /// An auth file appeared, or was there at start. Load it.
    AuthAdded(AuthFile, u64),
    /// A known auth file has new contents. Load it again.
    AuthChanged(AuthFile, u64),
    /// A known auth file was removed. Drop what was loaded from it. Files
    /// that never parsed are known too, so this may name a file that was
    /// never added.
    AuthRemoved(PathBuf, u64),
    /// The reload asked for with [`ConfigWatcher::reload_config`] and this
    /// ticket was made: the config change it found, if any, was sent
    /// before.
    Reloaded(u64),
}

/// An auth file and the contents the watcher read and checked. Its `Debug`
/// shows the contents' length, not the credential they hold.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthFile {
    /// The file, in the auth directory.
    pub path: PathBuf,
    /// Its contents when the watcher read them: a JSON object or `null`.
    pub data: Arc<[u8]>,
}

impl fmt::Debug for AuthFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthFile")
            .field("path", &self.path)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Why a watcher couldn't start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchError {
    message: String,
}

impl WatchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WatchError {}

/// Watches the config file and the auth directory until dropped.
pub struct ConfigWatcher {
    config_path: PathBuf,
    auth_dir: PathBuf,
    messages: std_mpsc::Sender<Message>,
    stopped: Arc<AtomicBool>,
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    /// Starts watching the config file at `config_path` and the auth
    /// directory `config` names.
    ///
    /// The receiver first gets a [`WatchEvent::AuthAdded`] for each auth file
    /// already there, then the changes. Like upstream's watcher, this fails
    /// when the config file or the auth directory doesn't exist.
    pub fn start(
        config_path: impl AsRef<Path>,
        config: &Config,
    ) -> Result<(Self, mpsc::Receiver<WatchEvent>), WatchError> {
        let config_path = absolute(config_path.as_ref(), "config file")?;
        if let Err(error) = fs::metadata(&config_path) {
            return Err(WatchError::new(format!(
                "watch config file {}: {error}",
                config_path.display()
            )));
        }
        let Some(config_dir) = config_path.parent().map(Path::to_path_buf) else {
            return Err(WatchError::new(format!(
                "watch config file {}: no parent directory",
                config_path.display()
            )));
        };
        let auth_dir = config
            .resolve_auth_dir()
            .map_err(|error| WatchError::new(error.to_string()))?;
        let auth_dir = absolute(&auth_dir, "auth directory")?;
        if !auth_dir.is_dir() {
            return Err(WatchError::new(format!(
                "watch auth directory {}: not an existing directory",
                auth_dir.display()
            )));
        }

        let (sender, messages) = std_mpsc::channel();
        let notify_sender = sender.clone();
        let mut watcher = notify::recommended_watcher(move |result| {
            let _ = notify_sender.send(Message::Fs(result));
        })
        .map_err(|error| WatchError::new(format!("start file watcher: {error}")))?;
        let mut dirs = vec![config_dir];
        if dirs.iter().all(|dir| path_key(dir) != path_key(&auth_dir)) {
            dirs.push(auth_dir.clone());
        }
        for dir in &dirs {
            watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|error| {
                    WatchError::new(format!("watch directory {}: {error}", dir.display()))
                })?;
        }

        let aliases = Aliases::new(&dirs);
        let state = WatchState::new(config_path.clone(), auth_dir.clone());
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        thread::Builder::new()
            .name("open-ferry-config-watcher".to_owned())
            .spawn(move || run(state, &aliases, &messages, &events, &thread_stopped))
            .map_err(|error| WatchError::new(format!("start watcher thread: {error}")))?;
        debug!(
            config = %config_path.display(),
            auth_dir = %auth_dir.display(),
            "watching config file and auth directory"
        );
        let watcher = Self {
            config_path,
            auth_dir,
            messages: sender,
            stopped,
            _watcher: watcher,
        };
        Ok((watcher, receiver))
    }

    /// The config file being watched, made absolute.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// The auth directory being watched, resolved and made absolute.
    pub fn auth_dir(&self) -> &Path {
        &self.auth_dir
    }

    /// Reloads the config file now rather than once it has been quiet,
    /// then sends [`WatchEvent::Reloaded`] with `ticket` (upstream's
    /// `ReloadConfigIfChanged`). As for a change the watcher sees, the file
    /// is reloaded only when its SHA-256 differs from the last config that
    /// loaded, and the [`WatchEvent::ConfigChanged`] or
    /// [`WatchEvent::ConfigInvalid`] it makes comes first. Nothing is sent
    /// once the watcher has stopped.
    pub fn reload_config(&self, ticket: u64) {
        let _ = self.messages.send(Message::Reload {
            ticket,
            force: false,
        });
    }

    /// [`reload_config`](Self::reload_config), loading the file even when
    /// its SHA-256 is that of the last config that loaded (not upstream's).
    /// An empty file is still skipped, and one that doesn't load reported
    /// with [`WatchEvent::ConfigInvalid`].
    pub fn force_reload_config(&self, ticket: u64) {
        let _ = self.messages.send(Message::Reload {
            ticket,
            force: true,
        });
    }
}

impl fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigWatcher")
            .field("config_path", &self.config_path)
            .field("auth_dir", &self.auth_dir)
            .finish_non_exhaustive()
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.messages.send(Message::Stop);
    }
}

/// What the watcher thread receives.
enum Message {
    Fs(notify::Result<notify::Event>),
    /// [`ConfigWatcher::reload_config`] with its ticket, or
    /// [`ConfigWatcher::force_reload_config`] with `force`.
    Reload {
        ticket: u64,
        force: bool,
    },
    Stop,
}

/// fsnotify's operations, which upstream's rules are written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Create,
    Write,
    Remove,
    Rename,
}

/// The fsnotify operation a notify event stands for, as fsnotify reports
/// the same change. Metadata and access events are dropped, as upstream
/// drops `Chmod`.
fn op_of(kind: &EventKind) -> Option<Op> {
    match kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            Some(Op::Create)
        }
        // Sent after the `From` and `To` halves it pairs up.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => None,
        EventKind::Modify(ModifyKind::Name(_)) => Some(Op::Rename),
        EventKind::Modify(ModifyKind::Metadata(_)) | EventKind::Access(_) => None,
        EventKind::Modify(_) | EventKind::Any | EventKind::Other => Some(Op::Write),
        EventKind::Remove(_) => Some(Op::Remove),
    }
}

/// What handling one event calls for.
#[derive(Debug, PartialEq)]
enum Step {
    Nothing,
    ScheduleConfigReload,
    Send(WatchEvent),
}

type Hash = [u8; 32];

fn sha256(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}

/// The watcher's bookkeeping. Upstream keeps it on its `Watcher` behind
/// mutexes; here one thread owns it.
struct WatchState {
    config_path: PathBuf,
    config_key: String,
    auth_dir: PathBuf,
    auth_dir_key: String,
    /// The hash of the last config that loaded; nothing before the first
    /// reload, so the first config event always reloads, as upstream's does.
    last_config_hash: Option<Hash>,
    /// The last contents seen of each known auth file, by [`path_key`].
    auth_hashes: HashMap<String, Hash>,
    /// When each auth file was last seen removed, by [`path_key`].
    remove_times: HashMap<String, Instant>,
}

impl WatchState {
    fn new(config_path: PathBuf, auth_dir: PathBuf) -> Self {
        Self {
            config_key: path_key(&config_path),
            config_path,
            auth_dir_key: path_key(&auth_dir),
            auth_dir,
            last_config_hash: None,
            auth_hashes: HashMap::new(),
            remove_times: HashMap::new(),
        }
    }

    /// Upstream's `handleEvent`.
    fn handle_event(&mut self, path: &Path, op: Op, now: Instant) -> Step {
        let key = path_key(path);
        if key == self.config_key && op != Op::Remove {
            debug!(?op, "config file event");
            return Step::ScheduleConfigReload;
        }
        let is_auth_json =
            paths::dir(Os::HOST, &key) == self.auth_dir_key && key.ends_with(".json");
        if !is_auth_json {
            return Step::Nothing;
        }
        debug!(?op, file = %file_name(path), "auth file event");
        match op {
            Op::Create | Op::Write => self.add_or_update(path, key),
            Op::Remove | Op::Rename => self.remove_or_replace(path, key, now),
        }
    }

    /// A removed or renamed auth file: upstream's check for an atomic
    /// replace, then `removeClient`.
    fn remove_or_replace(&mut self, path: &Path, key: String, now: Instant) -> Step {
        if self.should_debounce_remove(&key, now) {
            debug!(file = %file_name(path), "debouncing remove event");
            return Step::Nothing;
        }
        thread::sleep(REPLACE_CHECK_DELAY);
        let revision = next_revision();
        let mut exists = fs::metadata(path).is_ok();
        if !exists && self.auth_hashes.contains_key(&key) {
            for _ in 0..REPLACE_RETRIES {
                thread::sleep(REPLACE_RETRY_DELAY);
                if fs::metadata(path).is_ok() {
                    exists = true;
                    break;
                }
            }
        }
        if exists {
            return self.add_or_update(path, key);
        }
        if self.auth_hashes.remove(&key).is_none() {
            debug!(file = %file_name(path), "ignoring remove for unknown auth file");
            return Step::Nothing;
        }
        info!(file = %file_name(path), "auth file removed");
        Step::Send(WatchEvent::AuthRemoved(path.to_path_buf(), revision))
    }

    /// Upstream's `addOrUpdateClient`, with its `authFileUnchanged` check
    /// folded in so the file is read once.
    fn add_or_update(&mut self, path: &Path, key: String) -> Step {
        let revision = next_revision();
        let data = match read_capped(path) {
            Ok(data) => data,
            Err(error) => {
                error!(file = %file_name(path), %error, "failed to read auth file");
                return Step::Nothing;
            }
        };
        if data.is_empty() {
            debug!(file = %file_name(path), "ignoring empty auth file");
            return Step::Nothing;
        }
        let hash = sha256(&data);
        if self.auth_hashes.get(&key) == Some(&hash) {
            debug!(file = %file_name(path), "auth file unchanged (hash match)");
            return Step::Nothing;
        }
        if let Err(reason) = check_auth_json(&data) {
            error!(file = %file_name(path), %reason, "failed to parse auth file");
            return Step::Nothing;
        }
        let known = self.auth_hashes.insert(key, hash).is_some();
        info!(file = %file_name(path), "auth file changed");
        let file = AuthFile {
            path: path.to_path_buf(),
            data: data.into(),
        };
        Step::Send(if known {
            WatchEvent::AuthChanged(file, revision)
        } else {
            WatchEvent::AuthAdded(file, revision)
        })
    }

    /// Upstream's `shouldDebounceRemove`: whether `key` was removed less
    /// than a second ago. Records `now` otherwise.
    fn should_debounce_remove(&mut self, key: &str, now: Instant) -> bool {
        if key.is_empty() {
            return false;
        }
        if let Some(last) = self.remove_times.get(key)
            && now.saturating_duration_since(*last) < AUTH_REMOVE_DEBOUNCE_WINDOW
        {
            return true;
        }
        self.remove_times.insert(key.to_owned(), now);
        if self.remove_times.len() > REMOVE_TIMES_LIMIT {
            self.remove_times.retain(|_, at| {
                now.saturating_duration_since(*at) <= 2 * AUTH_REMOVE_DEBOUNCE_WINDOW
            });
        }
        false
    }

    /// Upstream's `reloadConfigIfChanged` and `reloadConfig`.
    fn reload_config_if_changed(&mut self) -> Option<WatchEvent> {
        self.reload_config_file(false)
    }

    /// [`reload_config_if_changed`](Self::reload_config_if_changed), or with
    /// `force` without its hash check (not upstream's).
    fn reload_config_file(&mut self, force: bool) -> Option<WatchEvent> {
        let data = match fs::read(&self.config_path) {
            Ok(data) => data,
            Err(error) => {
                error!(%error, "failed to read config file for hash check");
                return None;
            }
        };
        if data.is_empty() {
            debug!("ignoring empty config file write event");
            return None;
        }
        let hash = sha256(&data);
        if !force && self.last_config_hash == Some(hash) {
            debug!("config file content unchanged (hash match), skipping reload");
            return None;
        }
        info!(path = %self.config_path.display(), "config file changed, reloading");
        match load_bytes(&data) {
            Ok(mut config) => {
                match paths::resolve_auth_dir(Os::HOST, &config.auth_dir, paths::user_home_dir) {
                    Ok(auth_dir) => config.auth_dir = auth_dir,
                    Err(error) => error!(%error, "failed to resolve auth directory from config"),
                }
                self.last_config_hash = Some(hash);
                let sha256 = super::save::sha256_hex(&data);
                Some(WatchEvent::ConfigChanged(Arc::new(config), sha256))
            }
            Err(error) => {
                error!(%error, "failed to reload config");
                Some(WatchEvent::ConfigInvalid(error))
            }
        }
    }

    /// The auth-file part of upstream's first `reloadClients`: remembers
    /// every non-empty `.json` file in the auth directory and reports those
    /// that parse, in name order.
    fn initial_scan(&mut self) -> Vec<WatchEvent> {
        let entries = match fs::read_dir(&self.auth_dir) {
            Ok(entries) => entries,
            Err(error) => {
                error!(dir = %self.auth_dir.display(), %error, "failed to read auth directory");
                return Vec::new();
            }
        };
        let mut names: Vec<OsString> = entries
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name())
            .filter(|name| to_lower(&name.to_string_lossy()).ends_with(".json"))
            .collect();
        names.sort();
        let mut events = Vec::new();
        for name in names {
            let path = self.auth_dir.join(&name);
            let revision = next_revision();
            let data = match read_capped(&path) {
                Ok(data) if !data.is_empty() => data,
                _ => continue,
            };
            self.auth_hashes.insert(path_key(&path), sha256(&data));
            match check_auth_json(&data) {
                Ok(()) => events.push(WatchEvent::AuthAdded(
                    AuthFile {
                        path,
                        data: data.into(),
                    },
                    revision,
                )),
                Err(reason) => {
                    warn!(file = %name.to_string_lossy(), %reason, "skipping auth file");
                }
            }
        }
        debug!(
            files = self.auth_hashes.len(),
            loaded = events.len(),
            "auth directory scanned"
        );
        events
    }
}

/// Whether an auth file's contents parse, invalid UTF-8 reading as U+FFFD
/// as Go's decoder reads it. The reason never quotes them.
fn check_auth_json(data: &[u8]) -> Result<(), String> {
    match serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(data)) {
        Ok(value) if value.is_object() || value.is_null() => Ok(()),
        Ok(_) => Err("not a JSON object".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// The file name of `path`, for logs, as upstream logs `filepath.Base`.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `path` as upstream's `normalizeAuthPath` keys it on this platform.
fn path_key(path: &Path) -> String {
    normalize_path(Os::HOST, &path.to_string_lossy())
}

/// Upstream's `normalizeAuthPath`: trimmed and cleaned, and on Windows
/// without a `\\?\` prefix and lower-cased.
fn normalize_path(os: Os, path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let cleaned = paths::clean(os, trimmed);
    match os {
        Os::Unix => cleaned,
        Os::Windows => to_lower(cleaned.strip_prefix(r"\\?\").unwrap_or(&cleaned)),
    }
}

/// `path` made absolute and cleaned, as Go's `filepath.Abs` makes it: on
/// Unix, [`std::path::absolute`] alone keeps `..`.
fn absolute(path: &Path, what: &str) -> Result<PathBuf, WatchError> {
    std::path::absolute(path)
        .map(|path| crate::auth::path::clean(&path))
        .map_err(|error| WatchError::new(format!("{what} {}: {error}", path.display())))
}

/// Maps events under a watched directory's canonical path back to the
/// directory as it was given.
#[derive(Default)]
struct Aliases(Vec<(String, PathBuf)>);

impl Aliases {
    fn new(dirs: &[PathBuf]) -> Self {
        let mut aliases = Vec::new();
        for dir in dirs {
            if let Ok(canonical) = fs::canonicalize(dir) {
                let key = path_key(&canonical);
                if key != path_key(dir) {
                    aliases.push((key, dir.clone()));
                }
            }
        }
        Self(aliases)
    }

    fn translate(&self, path: &Path) -> PathBuf {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            let key = path_key(parent);
            if let Some((_, dir)) = self.0.iter().find(|(alias, _)| *alias == key) {
                return dir.join(name);
            }
        }
        path.to_path_buf()
    }
}

/// The watcher thread: reports the auth files found at start, then handles
/// events until stopped, reloading the config once it has been quiet for
/// [`CONFIG_RELOAD_DEBOUNCE`].
fn run(
    mut state: WatchState,
    aliases: &Aliases,
    messages: &std_mpsc::Receiver<Message>,
    events: &mpsc::Sender<WatchEvent>,
    stopped: &AtomicBool,
) {
    for event in state.initial_scan() {
        if !send(events, stopped, event) {
            return;
        }
    }
    let mut reload_at: Option<Instant> = None;
    loop {
        if let Some(at) = reload_at
            && at <= Instant::now()
        {
            reload_at = None;
            if let Some(event) = state.reload_config_if_changed()
                && !send(events, stopped, event)
            {
                return;
            }
        }
        let message = match reload_at {
            Some(at) => match messages.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match messages.recv() {
                Ok(message) => message,
                Err(_) => return,
            },
        };
        let event = match message {
            Message::Stop => return,
            Message::Reload { ticket, force } => {
                // A debounced reload still waiting finds the hash this one
                // leaves, as upstream's timer does.
                if let Some(event) = state.reload_config_file(force)
                    && !send(events, stopped, event)
                {
                    return;
                }
                if !send(events, stopped, WatchEvent::Reloaded(ticket)) {
                    return;
                }
                continue;
            }
            Message::Fs(Err(error)) => {
                warn!(%error, "file watcher error");
                continue;
            }
            Message::Fs(Ok(event)) => event,
        };
        let Some(op) = op_of(&event.kind) else {
            continue;
        };
        for path in &event.paths {
            let path = aliases.translate(path);
            match state.handle_event(&path, op, Instant::now()) {
                Step::Nothing => {}
                Step::ScheduleConfigReload => {
                    reload_at = Some(Instant::now() + CONFIG_RELOAD_DEBOUNCE);
                }
                Step::Send(event) => {
                    if !send(events, stopped, event) {
                        return;
                    }
                }
            }
        }
        if events.is_closed() {
            return;
        }
    }
}

/// Sends `event`, waiting while the channel is full. Returns `false` when
/// the receiver is gone or the watcher was stopped meanwhile.
fn send(events: &mpsc::Sender<WatchEvent>, stopped: &AtomicBool, mut event: WatchEvent) -> bool {
    loop {
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        match events.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(back)) => {
                event = back;
                thread::sleep(FULL_CHANNEL_POLL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, RemoveKind};

    use super::super::testing::TempDir;
    use super::*;

    const DEMO: &str = r#"{"type":"demo"}"#;

    /// A temp dir holding `config.yaml` and an `auth` directory.
    struct Fixture {
        dir: TempDir,
        config_path: PathBuf,
        auth_dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new();
            let auth_dir = dir.mkdir("auth");
            let config_path = dir.join("config.yaml");
            let fixture = Self {
                dir,
                config_path,
                auth_dir,
            };
            fixture.write_config("port: 8080\n");
            fixture
        }

        fn write_config(&self, rest: &str) {
            let body = format!("auth-dir: '{}'\n{rest}", self.auth_dir.display());
            fs::write(&self.config_path, body).expect("write config");
        }

        fn state(&self) -> WatchState {
            WatchState::new(self.config_path.clone(), self.auth_dir.clone())
        }

        fn auth(&self, name: &str) -> PathBuf {
            self.auth_dir.join(name)
        }

        fn write_auth(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.auth(name);
            fs::write(&path, contents).expect("write auth file");
            path
        }
    }

    fn known(state: &WatchState, path: &Path) -> Option<Hash> {
        state.auth_hashes.get(&path_key(path)).copied()
    }

    fn remember(state: &mut WatchState, path: &Path, contents: &str) {
        state
            .auth_hashes
            .insert(path_key(path), sha256(contents.as_bytes()));
    }

    fn auth_file(path: &Path) -> AuthFile {
        AuthFile {
            path: path.to_path_buf(),
            data: fs::read(path).expect("read auth file").into(),
        }
    }

    /// The added event for `path` with its current contents, unstamped.
    fn added(path: &Path) -> WatchEvent {
        WatchEvent::AuthAdded(auth_file(path), 0)
    }

    /// The changed event for `path` with its current contents, unstamped.
    fn changed(path: &Path) -> WatchEvent {
        WatchEvent::AuthChanged(auth_file(path), 0)
    }

    /// `event` with its revision, if it has one, set to zero.
    fn unstamped(event: WatchEvent) -> WatchEvent {
        match event {
            WatchEvent::AuthAdded(file, _) => WatchEvent::AuthAdded(file, 0),
            WatchEvent::AuthChanged(file, _) => WatchEvent::AuthChanged(file, 0),
            WatchEvent::AuthRemoved(path, _) => WatchEvent::AuthRemoved(path, 0),
            other => other,
        }
    }

    /// `step` with its event unstamped.
    fn sent(step: Step) -> Step {
        match step {
            Step::Send(event) => Step::Send(unstamped(event)),
            other => other,
        }
    }

    /// The revision of an auth event.
    fn revision_of(step: &Step) -> u64 {
        match step {
            Step::Send(
                WatchEvent::AuthAdded(_, revision)
                | WatchEvent::AuthChanged(_, revision)
                | WatchEvent::AuthRemoved(_, revision),
            ) => *revision,
            other => panic!("expected an auth event, got {other:?}"),
        }
    }

    fn config_of(event: Option<WatchEvent>) -> Arc<Config> {
        match event {
            Some(WatchEvent::ConfigChanged(config, _)) => config,
            other => panic!("expected a config change, got {other:?}"),
        }
    }

    #[test]
    fn reload_config_if_changed_triggers_on_change_and_skips_unchanged() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        assert_eq!(config_of(state.reload_config_if_changed()).port, 8080);
        assert_eq!(state.reload_config_if_changed(), None);

        fixture.write_config("port: 9090\nremote-management:\n  allow-remote: true\n");
        let config = config_of(state.reload_config_if_changed());
        assert_eq!(config.port, 9090);
        assert!(config.remote_management.allow_remote);
    }

    // Not upstream's: the event carries the SHA-256 of the contents the
    // config loaded from, in lowercase hex.
    #[test]
    fn a_config_change_carries_the_sha256_of_what_loaded() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        fixture.write_config("port: 9191\n");
        let data = fs::read(&fixture.config_path).unwrap();
        match state.reload_config_if_changed() {
            Some(WatchEvent::ConfigChanged(config, sha256)) => {
                assert_eq!(config.port, 9191);
                assert_eq!(sha256, crate::config::save::sha256_hex(&data));
            }
            other => panic!("expected a config change, got {other:?}"),
        }
    }

    // Not upstream's: a forced reload loads contents that loaded before,
    // and still skips an empty file and reports one that doesn't load.
    #[test]
    fn a_forced_reload_loads_contents_seen_before() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        fixture.write_config("port: 9292\n");
        assert_eq!(config_of(state.reload_config_if_changed()).port, 9292);
        assert_eq!(state.reload_config_if_changed(), None);
        let data = fs::read(&fixture.config_path).unwrap();
        match state.reload_config_file(true) {
            Some(WatchEvent::ConfigChanged(config, sha256)) => {
                assert_eq!(config.port, 9292);
                assert_eq!(sha256, crate::config::save::sha256_hex(&data));
            }
            other => panic!("expected a config change, got {other:?}"),
        }
        assert_eq!(state.reload_config_if_changed(), None);

        fs::write(&fixture.config_path, "").unwrap();
        assert_eq!(state.reload_config_file(true), None);
        fixture.write_config("port: [\n");
        assert!(matches!(
            state.reload_config_file(true),
            Some(WatchEvent::ConfigInvalid(_))
        ));
    }

    #[test]
    fn reload_config_if_changed_handles_missing_and_empty() {
        let fixture = Fixture::new();
        let mut state = WatchState::new(fixture.dir.join("missing.yaml"), fixture.auth_dir.clone());
        assert_eq!(state.reload_config_if_changed(), None);
        let empty = fixture.dir.write("empty.yaml", "");
        let mut state = WatchState::new(empty, fixture.auth_dir.clone());
        assert_eq!(state.reload_config_if_changed(), None);
        assert_eq!(state.last_config_hash, None);
    }

    #[test]
    fn invalid_config_is_reported_and_retried() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        fixture.write_config("port: [\n");
        for _ in 0..2 {
            match state.reload_config_if_changed() {
                Some(WatchEvent::ConfigInvalid(error)) => {
                    assert!(
                        error
                            .to_string()
                            .starts_with("failed to parse config file: ")
                    );
                }
                other => panic!("expected an invalid config, got {other:?}"),
            }
        }
        assert_eq!(state.last_config_hash, None);
        fixture.write_config("port: 1\n");
        assert_eq!(config_of(state.reload_config_if_changed()).port, 1);
    }

    #[test]
    fn reloaded_config_has_its_auth_dir_resolved() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let messy = fixture.dir.join("x").join("..").join("auth");
        fs::write(
            &fixture.config_path,
            format!("auth-dir: '{}'\n", messy.display()),
        )
        .expect("write config");
        let config = config_of(state.reload_config_if_changed());
        assert_eq!(
            path_key(Path::new(&config.auth_dir)),
            path_key(&fixture.auth_dir)
        );
        assert!(!config.auth_dir.contains(".."));
    }

    #[test]
    fn add_or_update_skips_unchanged() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.write_auth("sample.json", DEMO);
        remember(&mut state, &path, DEMO);
        assert_eq!(state.add_or_update(&path, path_key(&path)), Step::Nothing);
    }

    #[test]
    fn add_or_update_reports_and_hashes() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let contents = r#"{"type":"demo","api_key":"k"}"#;
        let path = fixture.write_auth("sample.json", contents);
        assert_eq!(
            sent(state.add_or_update(&path, path_key(&path))),
            Step::Send(added(&path))
        );
        assert_eq!(known(&state, &path), Some(sha256(contents.as_bytes())));

        // The same contents again are skipped; new ones are a change.
        assert_eq!(state.add_or_update(&path, path_key(&path)), Step::Nothing);
        fs::write(&path, r#"{"type":"demo","api_key":"k2"}"#).expect("rewrite");
        assert_eq!(
            sent(state.add_or_update(&path, path_key(&path))),
            Step::Send(changed(&path))
        );
    }

    /// Not upstream's: each auth event takes the next revision, in the
    /// order the watcher handled them, from the counter the service's
    /// credential sync shares.
    #[test]
    fn auth_events_take_rising_revisions() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.write_auth("a.json", DEMO);
        let before = next_revision();
        let add = state.handle_event(&path, Op::Write, Instant::now());
        let between = next_revision();
        fs::write(&path, r#"{"type":"demo","v":2}"#).expect("rewrite");
        let change = state.handle_event(&path, Op::Write, Instant::now());
        fs::remove_file(&path).expect("remove");
        let remove = state.handle_event(&path, Op::Remove, Instant::now());
        let after = next_revision();
        let revisions = [&add, &change, &remove].map(revision_of);
        assert!(before < revisions[0], "{before} {revisions:?}");
        assert!(revisions[0] < between && between < revisions[1]);
        assert!(revisions[1] < revisions[2] && revisions[2] < after);

        let other = fixture.write_auth("b.json", DEMO);
        let events = fixture.state().initial_scan();
        assert_eq!(events.len(), 1);
        let Some(WatchEvent::AuthAdded(file, revision)) = events.first() else {
            panic!("expected an added event, got {events:?}");
        };
        assert_eq!(file.path, other);
        assert!(after < *revision);
    }

    #[test]
    fn add_or_update_edge_cases() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let missing = fixture.auth("missing.json");
        let empty = fixture.write_auth("empty.json", "");
        let broken = fixture.write_auth("broken.json", "{\"type\":");
        let list = fixture.write_auth("list.json", "[1]");
        for path in [&missing, &empty, &broken, &list] {
            assert_eq!(
                state.add_or_update(path, path_key(path)),
                Step::Nothing,
                "{path:?}"
            );
        }
        assert!(state.auth_hashes.is_empty());

        // `null` decodes into upstream's auth record, so it counts.
        let null = fixture.write_auth("null.json", "null");
        assert_eq!(
            sent(state.add_or_update(&null, path_key(&null))),
            Step::Send(added(&null))
        );
    }

    #[test]
    fn auth_json_errors_never_quote_contents() {
        for contents in [
            r#""secret-token""#,
            "[\"secret-token\"]",
            "secret-token",
            "12",
        ] {
            let reason = check_auth_json(contents.as_bytes()).expect_err(contents);
            assert!(!reason.contains("secret"), "{reason}");
        }
    }

    #[test]
    fn should_debounce_remove() {
        let mut state = Fixture::new().state();
        let start = Instant::now();
        assert!(!state.should_debounce_remove("test.json", start));
        assert!(state.should_debounce_remove("test.json", start));
        assert!(
            !state.should_debounce_remove("test.json", start + 2 * AUTH_REMOVE_DEBOUNCE_WINDOW)
        );
        assert!(!state.should_debounce_remove("", start));
        assert!(!state.should_debounce_remove("", start));
    }

    #[test]
    fn normalize_path_and_debounce_cleanup() {
        assert_eq!(normalize_path(Os::HOST, "   "), "");
        assert_eq!(
            normalize_path(Os::HOST, "  a/../b  "),
            paths::clean(Os::HOST, "a/../b")
        );
        assert_eq!(
            normalize_path(Os::Windows, r"\\?\C:\Auth\X.JSON"),
            r"c:\auth\x.json"
        );
        assert_eq!(
            normalize_path(Os::Windows, "C:/Auth/./x.json"),
            r"c:\auth\x.json"
        );
        assert_eq!(normalize_path(Os::Unix, "/Auth/./X.JSON"), "/Auth/X.JSON");

        let mut state = Fixture::new().state();
        let start = Instant::now();
        for i in 0..129 {
            state.remove_times.insert(format!("old-{i}"), start);
        }
        state.should_debounce_remove("new-path", start + 3 * AUTH_REMOVE_DEBOUNCE_WINDOW);
        assert_eq!(state.remove_times.len(), 1);
    }

    #[test]
    fn initial_scan_caches_auth_hashes() {
        let fixture = Fixture::new();
        let one = fixture.write_auth("one.json", DEMO);
        let upper = fixture.write_auth("two.JSON", DEMO);
        let broken = fixture.write_auth("bad.json", "not json");
        fixture.write_auth("empty.json", "");
        fixture.write_auth("note.txt", DEMO);
        fs::create_dir(fixture.auth("sub.json")).expect("create dir");

        let mut state = fixture.state();
        let events: Vec<_> = state.initial_scan().into_iter().map(unstamped).collect();
        assert_eq!(events, [added(&one), added(&upper)]);
        assert_eq!(state.auth_hashes.len(), 3);
        assert!(known(&state, &broken).is_some());

        // A known file that didn't parse is a change once it does.
        assert_eq!(
            state.handle_event(&broken, Op::Write, Instant::now()),
            Step::Nothing
        );
        fs::write(&broken, DEMO).expect("fix auth file");
        assert_eq!(
            sent(state.handle_event(&broken, Op::Write, Instant::now())),
            Step::Send(changed(&broken))
        );
    }

    #[test]
    fn initial_scan_of_a_missing_dir_finds_nothing() {
        let fixture = Fixture::new();
        let mut state = WatchState::new(fixture.config_path.clone(), fixture.dir.join("gone"));
        assert!(state.initial_scan().is_empty());
    }

    #[test]
    fn handle_event_ignores_unrelated_files() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let now = Instant::now();
        let note = fixture.dir.write("note.txt", "x");
        let outside = fixture.dir.write("outside.json", DEMO);
        let nested = fixture.dir.mkdir("auth/nested").join("deep.json");
        let cookie = fixture.write_auth("session.cookie", "x");
        for path in [&note, &outside, &nested, &cookie] {
            for op in [Op::Create, Op::Write, Op::Remove, Op::Rename] {
                assert_eq!(
                    state.handle_event(path, op, now),
                    Step::Nothing,
                    "{path:?} {op:?}"
                );
            }
        }
        assert!(state.auth_hashes.is_empty());
    }

    #[test]
    fn handle_event_config_change_schedules_reload() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let now = Instant::now();
        for op in [Op::Write, Op::Create, Op::Rename] {
            assert_eq!(
                state.handle_event(&fixture.config_path, op, now),
                Step::ScheduleConfigReload
            );
        }
        assert_eq!(
            state.handle_event(&fixture.config_path, Op::Remove, now),
            Step::Nothing
        );
        // The same file spelled another way is still the config file.
        let spelled = fixture.dir.join("auth").join("..").join("config.yaml");
        assert_eq!(
            state.handle_event(&spelled, Op::Write, now),
            Step::ScheduleConfigReload
        );
    }

    #[test]
    fn handle_event_auth_write_triggers_update() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.write_auth("a.json", DEMO);
        assert_eq!(
            sent(state.handle_event(&path, Op::Write, Instant::now())),
            Step::Send(added(&path))
        );
        assert!(known(&state, &path).is_some());
    }

    #[test]
    fn handle_event_matches_json_suffix_like_upstream() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.write_auth("UPPER.JSON", DEMO);
        let step = state.handle_event(&path, Op::Write, Instant::now());
        // Upstream lower-cases paths only on Windows before the suffix check.
        if cfg!(windows) {
            assert_eq!(sent(step), Step::Send(added(&path)));
        } else {
            assert_eq!(step, Step::Nothing);
        }
    }

    #[test]
    fn handle_event_removes_auth_file() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.auth("remove.json");
        remember(&mut state, &path, DEMO);
        assert_eq!(
            sent(state.handle_event(&path, Op::Remove, Instant::now())),
            Step::Send(WatchEvent::AuthRemoved(path.clone(), 0))
        );
        assert_eq!(known(&state, &path), None);
    }

    #[test]
    fn handle_event_remove_debounce_skips() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.auth("remove.json");
        remember(&mut state, &path, DEMO);
        let now = Instant::now();
        state.remove_times.insert(path_key(&path), now);
        assert_eq!(state.handle_event(&path, Op::Remove, now), Step::Nothing);
        assert!(known(&state, &path).is_some());
    }

    #[test]
    fn handle_event_atomic_replace_unchanged_skips() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.write_auth("same.json", DEMO);
        remember(&mut state, &path, DEMO);
        assert_eq!(
            state.handle_event(&path, Op::Rename, Instant::now()),
            Step::Nothing
        );
        assert!(known(&state, &path).is_some());
    }

    #[test]
    fn handle_event_atomic_replace_changed_triggers_update() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let new = r#"{"type":"demo","v":2}"#;
        let path = fixture.write_auth("change.json", new);
        remember(&mut state, &path, r#"{"type":"demo","v":1}"#);
        assert_eq!(
            sent(state.handle_event(&path, Op::Rename, Instant::now())),
            Step::Send(changed(&path))
        );
        assert_eq!(known(&state, &path), Some(sha256(new.as_bytes())));
    }

    #[test]
    fn handle_event_remove_unknown_file_ignored() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.auth("unknown.json");
        assert_eq!(
            state.handle_event(&path, Op::Remove, Instant::now()),
            Step::Nothing
        );
        assert!(state.auth_hashes.is_empty());
    }

    #[test]
    fn handle_event_atomic_replace_delayed_stat_preserves_client() {
        // The last check comes no sooner than this after the event. A loaded
        // machine can land the replacement later than that, and such a try
        // proves nothing, so it is run again.
        let window = REPLACE_CHECK_DELAY + REPLACE_RETRY_DELAY * REPLACE_RETRIES as u32;
        for _ in 0..5 {
            let fixture = Fixture::new();
            let mut state = fixture.state();
            let path = fixture.auth("token.json");
            remember(&mut state, &path, r#"{"type":"demo","v":1}"#);
            let new = r#"{"type":"demo","v":2}"#;

            // Written after the first check, within the retries, and renamed
            // into place so the retries never see it empty.
            let start = Instant::now();
            let writer = {
                let path = path.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(60));
                    let temp = path.with_extension("tmp");
                    fs::write(&temp, new)?;
                    fs::rename(&temp, &path)?;
                    Ok::<_, std::io::Error>(Instant::now())
                })
            };
            let step = state.handle_event(&path, Op::Rename, start);
            let landed = writer
                .join()
                .expect("writer thread")
                .expect("write replacement");
            if landed.duration_since(start) >= window {
                continue;
            }
            assert_eq!(sent(step), Step::Send(changed(&path)));
            assert_eq!(known(&state, &path), Some(sha256(new.as_bytes())));
            return;
        }
        eprintln!("the replacement never landed before the last check; inconclusive");
    }

    #[test]
    fn handle_event_remove_known_file_deletes() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let path = fixture.auth("known.json");
        remember(&mut state, &path, DEMO);
        assert_eq!(
            sent(state.handle_event(&path, Op::Rename, Instant::now())),
            Step::Send(WatchEvent::AuthRemoved(path.clone(), 0))
        );
        assert_eq!(known(&state, &path), None);
    }

    #[test]
    fn notify_events_map_to_fsnotify_ops() {
        use notify::event::RenameMode as R;
        let name = |mode| EventKind::Modify(ModifyKind::Name(mode));
        for (kind, want) in [
            (EventKind::Create(CreateKind::File), Some(Op::Create)),
            (EventKind::Modify(ModifyKind::Any), Some(Op::Write)),
            (
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                Some(Op::Write),
            ),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions)),
                None,
            ),
            (name(R::From), Some(Op::Rename)),
            (name(R::To), Some(Op::Create)),
            (name(R::Any), Some(Op::Rename)),
            (name(R::Both), None),
            (EventKind::Remove(RemoveKind::File), Some(Op::Remove)),
            (EventKind::Access(AccessKind::Any), None),
            (EventKind::Any, Some(Op::Write)),
        ] {
            assert_eq!(op_of(&kind), want, "{kind:?}");
        }
    }

    #[test]
    fn aliases_map_canonical_parents_back() {
        let dir = PathBuf::from("/given/auth");
        let aliases = Aliases(vec![(path_key(Path::new("/real/auth")), dir.clone())]);
        assert_eq!(
            aliases.translate(Path::new("/real/auth/a.json")),
            dir.join("a.json")
        );
        assert_eq!(
            aliases.translate(Path::new("/other/a.json")),
            Path::new("/other/a.json")
        );
        assert_eq!(aliases.translate(Path::new("/")), Path::new("/"));
    }

    fn fs_event(kind: EventKind, path: &Path) -> Message {
        Message::Fs(Ok(notify::Event::new(kind).add_path(path.to_path_buf())))
    }

    #[test]
    fn schedule_config_reload_debounces() {
        let fixture = Fixture::new();
        let auth = fixture.write_auth("a.json", DEMO);
        let (sender, messages) = std_mpsc::channel();
        let (events, mut receiver) = mpsc::channel(8);
        let state = fixture.state();
        let thread = thread::spawn(move || {
            run(
                state,
                &Aliases::default(),
                &messages,
                &events,
                &AtomicBool::new(false),
            )
        });
        assert_eq!(receiver.blocking_recv().map(unstamped), Some(added(&auth)));

        fixture.write_config("port: 7\n");
        let write = EventKind::Modify(ModifyKind::Any);
        sender
            .send(fs_event(write, &fixture.config_path))
            .expect("send");
        thread::sleep(Duration::from_millis(50));
        let last = Instant::now();
        sender
            .send(fs_event(write, &fixture.config_path))
            .expect("send");
        let config = config_of(receiver.blocking_recv());
        assert!(last.elapsed() >= CONFIG_RELOAD_DEBOUNCE);
        assert_eq!(config.port, 7);

        sender.send(Message::Stop).expect("send stop");
        thread.join().expect("watcher thread");
        assert!(receiver.try_recv().is_err(), "a single reload");
    }

    #[test]
    fn run_stops_when_the_receiver_is_dropped() {
        let fixture = Fixture::new();
        let (sender, messages) = std_mpsc::channel();
        let (events, receiver) = mpsc::channel(8);
        let state = fixture.state();
        let thread = thread::spawn(move || {
            run(
                state,
                &Aliases::default(),
                &messages,
                &events,
                &AtomicBool::new(false),
            )
        });
        drop(receiver);
        let path = fixture.write_auth("a.json", DEMO);
        // The thread may already have stopped, if its first scan saw the
        // file; either way it must stop.
        let _ = sender.send(fs_event(EventKind::Create(CreateKind::File), &path));
        thread.join().expect("watcher thread");
    }

    #[test]
    fn auth_events_carry_the_contents_that_were_checked() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let valid = r#"{"type":"demo","v":1}"#;
        let path = fixture.write_auth("a.json", valid);
        let step = state.add_or_update(&path, path_key(&path));
        // A write that doesn't parse lands before the consumer reads.
        fs::write(&path, "{not json").expect("rewrite");
        let Step::Send(WatchEvent::AuthAdded(file, _)) = step else {
            panic!("expected an added event, got {step:?}");
        };
        assert_eq!(&*file.data, valid.as_bytes());
        assert_eq!(state.add_or_update(&path, path_key(&path)), Step::Nothing);
    }

    #[test]
    fn auth_files_with_invalid_utf8_load() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let mut data = br#"{"type":"demo","note":"a"#.to_vec();
        data.push(0xff);
        data.extend_from_slice(br#"b"}"#);
        let path = fixture.auth("bad.json");
        fs::write(&path, &data).expect("write");
        let step = state.add_or_update(&path, path_key(&path));
        let Step::Send(WatchEvent::AuthAdded(file, _)) = step else {
            panic!("expected an added event, got {step:?}");
        };
        assert_eq!(&*file.data, &data[..]);
        // Outside a string it is still a syntax error.
        assert!(check_auth_json(&[b'{', 0xff, b'}']).is_err());
    }

    /// Not upstream's: the `Debug` of an auth file, and of an event
    /// carrying one, shows its path and length, never the credential.
    #[test]
    fn auth_file_debug_leaves_out_the_credential() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let data =
            r#"{"type":"demo","access_token":"TOKEN-SECRET","refresh_token":"REFRESH-SECRET"}"#;
        let path = fixture.write_auth("marker.json", data);
        let step = state.add_or_update(&path, path_key(&path));
        let Step::Send(event @ WatchEvent::AuthAdded(file, _)) = &step else {
            panic!("expected an added event");
        };
        assert_eq!(&*file.data, data.as_bytes());
        assert_eq!(
            format!("{file:?}"),
            format!(
                "AuthFile {{ path: {:?}, data_len: {} }}",
                file.path,
                data.len()
            )
        );
        // A derived `Debug` writes the bytes as numbers.
        let bytes: Vec<String> = "SECRET".bytes().map(|b| b.to_string()).collect();
        for shown in [
            format!("{file:#?}"),
            format!("{event:?}"),
            format!("{step:#?}"),
        ] {
            assert!(!shown.contains("SECRET"), "{shown}");
            assert!(!shown.contains(&bytes.join(", ")), "{shown}");
            assert!(shown.contains("marker.json"), "{shown}");
            assert!(
                shown.contains(&format!("data_len: {}", data.len())),
                "{shown}"
            );
        }
    }

    #[test]
    fn oversized_auth_files_are_skipped() {
        let fixture = Fixture::new();
        let mut state = fixture.state();
        let size = usize::try_from(crate::auth::file_store::MAX_AUTH_FILE_SIZE).unwrap();
        let big = format!(r#"{{"type":"demo","pad":"{}"}}"#, "x".repeat(size));
        let path = fixture.write_auth("big.json", &big);
        assert_eq!(state.add_or_update(&path, path_key(&path)), Step::Nothing);
        assert!(state.initial_scan().is_empty());
        assert!(state.auth_hashes.is_empty());
    }

    #[test]
    fn run_stops_while_the_channel_is_full() {
        let fixture = Fixture::new();
        for name in ["a.json", "b.json", "c.json"] {
            fixture.write_auth(name, DEMO);
        }
        let (_sender, messages) = std_mpsc::channel();
        let (events, receiver) = mpsc::channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = fixture.state();
            let stopped = Arc::clone(&stopped);
            thread::spawn(move || run(state, &Aliases::default(), &messages, &events, &stopped))
        };
        // The scan fills the channel and waits on the second file.
        thread::sleep(Duration::from_millis(50));
        assert!(!thread.is_finished());
        stopped.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(thread.is_finished(), "the watcher thread didn't stop");
        thread.join().expect("watcher thread");
        drop(receiver);
    }

    #[test]
    fn start_fails_when_config_missing() {
        let fixture = Fixture::new();
        let config = Config {
            auth_dir: fixture.auth_dir.display().to_string(),
            ..Config::default()
        };
        let error = ConfigWatcher::start(fixture.dir.join("missing-config.yaml"), &config)
            .expect_err("missing config file");
        assert!(
            error.to_string().starts_with("watch config file "),
            "{error}"
        );
    }

    #[test]
    fn start_fails_when_auth_dir_missing() {
        let fixture = Fixture::new();
        let missing = fixture.dir.join("missing-auth");
        let config = Config {
            auth_dir: missing.display().to_string(),
            ..Config::default()
        };
        let error =
            ConfigWatcher::start(&fixture.config_path, &config).expect_err("missing auth dir");
        assert!(
            error.to_string().starts_with("watch auth directory "),
            "{error}"
        );
    }

    async fn next(receiver: &mut mpsc::Receiver<WatchEvent>) -> WatchEvent {
        tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .expect("timed out waiting for a watch event")
            .expect("watcher stopped")
    }

    fn key_of(event: &WatchEvent) -> (&'static str, String) {
        match event {
            WatchEvent::AuthAdded(file, _) => ("added", path_key(&file.path)),
            WatchEvent::AuthChanged(file, _) => ("changed", path_key(&file.path)),
            WatchEvent::AuthRemoved(path, _) => ("removed", path_key(path)),
            WatchEvent::ConfigChanged(..) => ("config", String::new()),
            WatchEvent::ConfigInvalid(_) => ("invalid", String::new()),
            WatchEvent::Reloaded(ticket) => ("reloaded", ticket.to_string()),
        }
    }

    // Upstream's ReloadConfigIfChanged, as the management API calls it
    // after a save: the change comes at once and before the answer, and the
    // debounced reload the write brings, finding the same hash, sends
    // nothing.
    #[tokio::test]
    async fn reload_config_reloads_at_once_and_once() {
        let fixture = Fixture::new();
        let config = Config::load(&fixture.config_path).expect("load config");
        let (watcher, mut receiver) =
            ConfigWatcher::start(&fixture.config_path, &config).expect("start watcher");

        fixture.write_config("port: 3\n");
        watcher.reload_config(7);
        match next(&mut receiver).await {
            WatchEvent::ConfigChanged(config, _) => assert_eq!(config.port, 3),
            other => panic!("expected a config change, got {other:?}"),
        }
        assert_eq!(next(&mut receiver).await, WatchEvent::Reloaded(7));

        // The same contents: nothing to reload.
        watcher.reload_config(8);
        assert_eq!(next(&mut receiver).await, WatchEvent::Reloaded(8));
        let quiet = tokio::time::timeout(CONFIG_RELOAD_DEBOUNCE * 4, receiver.recv()).await;
        assert!(quiet.is_err(), "unexpected event: {quiet:?}");

        // Not upstream's: forced, they load again.
        watcher.force_reload_config(9);
        match next(&mut receiver).await {
            WatchEvent::ConfigChanged(config, _) => assert_eq!(config.port, 3),
            other => panic!("expected a config change, got {other:?}"),
        }
        assert_eq!(next(&mut receiver).await, WatchEvent::Reloaded(9));

        // A change after it is followed as before.
        fixture.write_config("port: 4\n");
        match next(&mut receiver).await {
            WatchEvent::ConfigChanged(config, _) => assert_eq!(config.port, 4),
            other => panic!("expected a config change, got {other:?}"),
        }
        drop(watcher);
    }

    #[tokio::test]
    async fn start_and_watch_end_to_end() {
        let fixture = Fixture::new();
        let first = fixture.write_auth("a.json", DEMO);
        let config = Config::load(&fixture.config_path).expect("load config");
        let (watcher, mut receiver) =
            ConfigWatcher::start(&fixture.config_path, &config).expect("start watcher");
        assert_eq!(path_key(watcher.auth_dir()), path_key(&fixture.auth_dir));
        assert_eq!(
            key_of(&next(&mut receiver).await),
            ("added", path_key(&first))
        );

        let second = fixture.write_auth("b.json", DEMO);
        assert_eq!(
            key_of(&next(&mut receiver).await),
            ("added", path_key(&second))
        );

        // Saved by writing a temporary file and renaming it over the config.
        let temporary = fixture.dir.join("config.yaml.tmp");
        let body = format!("auth-dir: '{}'\nport: 2\n", fixture.auth_dir.display());
        fs::write(&temporary, body).expect("write temporary config");
        fs::rename(&temporary, &fixture.config_path).expect("replace config");
        match next(&mut receiver).await {
            WatchEvent::ConfigChanged(config, _) => assert_eq!(config.port, 2),
            other => panic!("expected a config change, got {other:?}"),
        }

        fs::remove_file(&second).expect("remove auth file");
        assert_eq!(
            key_of(&next(&mut receiver).await),
            ("removed", path_key(&second))
        );

        fs::write(&first, r#"{"type":"demo","v":2}"#).expect("rewrite auth file");
        assert_eq!(
            key_of(&next(&mut receiver).await),
            ("changed", path_key(&first))
        );

        drop(watcher);
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            while receiver.recv().await.is_some() {}
        });
        drained.await.expect("watcher thread should stop");
    }
}
