//! An entry's settings, from its credential or its config entry, and what
//! they come to: the program to run, its environment and its directories.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use open_ferry_core::auth::Auth;
use open_ferry_core::auth::synthesizer::claude_cli::{
    ATTRIBUTE_COMMAND, ATTRIBUTE_CONFIG_DIR, ATTRIBUTE_MAX_CONCURRENCY, ATTRIBUTE_SYSTEM_PROMPT,
    ATTRIBUTE_TIMEOUT_MS,
};
use open_ferry_core::config::{ClaudeCli, ClaudeCliSystemPrompt};
use sha2::{Digest as _, Sha256};

/// The program run when an entry names none.
const DEFAULT_PROGRAM: &str = "claude";

/// How old a prompt file left behind by an earlier run must be before it is
/// removed.
const STALE_PROMPT: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest part of an entry's name kept in its directory's name.
const MAX_DIR_NAME: usize = 40;

/// One `claude-cli` entry's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry's name.
    pub name: String,
    /// The `claude` executable, as configured; empty for `claude` on
    /// `PATH`.
    pub command: String,
    /// Claude Code's config directory, as configured; empty for its
    /// default.
    pub config_dir: String,
    /// How the client's system prompt is given.
    pub system_prompt: ClaudeCliSystemPrompt,
    /// How many requests run at once.
    pub max_concurrency: usize,
    /// How long a request may run, its wait for a slot included.
    pub timeout: Duration,
}

impl Entry {
    /// The settings of a config entry.
    pub fn from_config(entry: &ClaudeCli) -> Self {
        Self {
            name: entry.name.trim().to_owned(),
            command: entry.command.trim().to_owned(),
            config_dir: entry.config_dir.trim().to_owned(),
            system_prompt: entry.system_prompt_mode(),
            max_concurrency: entry.max_concurrency(),
            timeout: entry.timeout(),
        }
    }

    /// The settings a `claude-cli` credential carries; those it lacks take
    /// their defaults.
    pub fn from_auth(auth: &Auth) -> Self {
        let attribute = |name| auth.attribute(name).unwrap_or_default().trim().to_owned();
        let max_concurrency = attribute(ATTRIBUTE_MAX_CONCURRENCY)
            .parse::<usize>()
            .ok()
            .filter(|limit| *limit > 0)
            .unwrap_or(ClaudeCli::DEFAULT_MAX_CONCURRENCY);
        let timeout = attribute(ATTRIBUTE_TIMEOUT_MS)
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .map_or(ClaudeCli::DEFAULT_TIMEOUT, Duration::from_millis);
        Self {
            name: auth.label.trim().to_owned(),
            command: attribute(ATTRIBUTE_COMMAND),
            config_dir: attribute(ATTRIBUTE_CONFIG_DIR),
            system_prompt: ClaudeCliSystemPrompt::parse(&attribute(ATTRIBUTE_SYSTEM_PROMPT))
                .unwrap_or_default(),
            max_concurrency,
            timeout,
        }
    }

    /// The program to run: the command with a leading `~` made the home
    /// directory, or, for a bare name (`claude` when none is set), the
    /// first match on `PATH`. On Windows a bare name without an extension
    /// matches `<name>.exe`, then `<name>.cmd`.
    pub(crate) fn program(&self) -> Result<PathBuf, String> {
        let command = if self.command.is_empty() {
            DEFAULT_PROGRAM
        } else {
            self.command.as_str()
        };
        let expanded = expand_home(command);
        if expanded.is_absolute() || expanded.components().count() > 1 {
            return Ok(expanded);
        }
        find_on_path(expanded.as_os_str()).ok_or_else(|| {
            format!("{command} isn't on PATH; set the entry's command to the claude executable")
        })
    }

    /// The config directory, with a leading `~` made the home directory;
    /// `None` when the entry has none.
    pub(crate) fn config_dir_path(&self) -> Option<PathBuf> {
        (!self.config_dir.is_empty()).then(|| expand_home(&self.config_dir))
    }

    /// Applies the entry's environment to `command`, which otherwise
    /// inherits open-ferry's: removes every `ANTHROPIC_*` and `CLAUDE*`
    /// variable and `MAX_THINKING_TOKENS`, keeping `CLAUDE_CODE_OAUTH_TOKEN`
    /// and `CLAUDE_CONFIG_DIR` only when the entry has no config directory,
    /// and sets `CLAUDE_CONFIG_DIR` when it has one. Only the variables'
    /// names are looked at, never their values.
    pub(crate) fn apply_env(&self, command: &mut tokio::process::Command) {
        let config_dir = self.config_dir_path();
        for name in std::env::vars_os().map(|(name, _)| name) {
            if scrubbed(&name, config_dir.is_none()) {
                command.env_remove(&name);
            }
        }
        if let Some(dir) = config_dir {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
    }

    /// The name of the entry's directory under a work root: its name, made
    /// safe, and a hash of its name and config directory.
    pub(crate) fn dir_name(&self) -> String {
        let mut safe: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .take(MAX_DIR_NAME)
            .collect();
        if safe.is_empty() || safe.starts_with('.') {
            safe.insert(0, '_');
        }
        let mut hash = Sha256::new();
        hash.update(self.name.as_bytes());
        hash.update([0]);
        hash.update(self.config_dir.as_bytes());
        let digest = hash.finalize();
        let hex: String = digest
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{safe}-{hex}")
    }

    /// Makes the entry's directory under `root` and the empty working
    /// directory in it, private to the user, and gives both. Prompt files
    /// an earlier run left there for a day or more are removed, once a
    /// process.
    pub(crate) fn dirs(&self, root: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let entry = root.join(self.dir_name());
        let work = entry.join("work");
        create_private_dir(&work)?;
        restrict_dir(&entry)?;
        clean_stale_prompts(&entry);
        Ok((entry, work))
    }
}

/// Whether a variable named `name` is removed from Claude Code's
/// environment; `keep_account` keeps `CLAUDE_CODE_OAUTH_TOKEN` and
/// `CLAUDE_CONFIG_DIR`, which say whose account Claude Code uses.
pub(crate) fn scrubbed(name: &OsStr, keep_account: bool) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    if name == "CLAUDE_CODE_OAUTH_TOKEN" || name == "CLAUDE_CONFIG_DIR" {
        return !keep_account;
    }
    name.starts_with("ANTHROPIC_") || name.starts_with("CLAUDE") || name == "MAX_THINKING_TOKENS"
}

/// The directory `claude-cli` entries keep their files in: under
/// `%LOCALAPPDATA%` on Windows, and under `$XDG_RUNTIME_DIR`, else
/// `$XDG_CACHE_HOME` or `~/.cache`, elsewhere; the temporary directory when
/// none of those is set.
pub fn default_work_root() -> PathBuf {
    let base = if cfg!(windows) {
        non_empty_var("LOCALAPPDATA")
    } else {
        non_empty_var("XDG_RUNTIME_DIR")
            .or_else(|| non_empty_var("XDG_CACHE_HOME"))
            .or_else(|| non_empty_var("HOME").map(|home| home.join(".cache")))
    };
    match base {
        Some(base) => base.join("open-ferry").join("claude-cli"),
        None => std::env::temp_dir().join("open-ferry-claude-cli"),
    }
}

fn non_empty_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The user's home directory.
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        non_empty_var("USERPROFILE").or_else(|| non_empty_var("HOME"))
    } else {
        non_empty_var("HOME")
    }
}

/// `path` with a leading `~` (alone, or before a separator) made the home
/// directory.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/")
            .or_else(|| path.strip_prefix("~\\").filter(|_| cfg!(windows)))
    };
    match (rest, home_dir()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// The first file on `PATH` that `name` matches.
fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let candidates = candidates(name);
    std::env::split_paths(&paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            candidates
                .iter()
                .map(|candidate| dir.join(candidate))
                .find(|path| is_program(path))
        })
}

/// The file names a bare `name` matches in one directory.
fn candidates(name: &OsStr) -> Vec<PathBuf> {
    let name = PathBuf::from(name);
    if cfg!(windows) && name.extension().is_none() {
        vec![name.with_extension("exe"), name.with_extension("cmd")]
    } else {
        vec![name]
    }
}

#[cfg(unix)]
fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_program(path: &Path) -> bool {
    path.is_file()
}

/// Makes `dir` and its missing parents; on Unix those it makes are for the
/// user alone.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    restrict_dir(dir)
}

/// On Unix, makes `dir` the user's alone.
fn restrict_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Removes the prompt files in `dir` a day old or more, the first time this
/// process uses it.
fn clean_stale_prompts(dir: &Path) {
    static CLEANED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);
    let first = CLEANED
        .lock()
        .map(|mut cleaned| cleaned.insert(dir.to_path_buf()))
        .unwrap_or(false);
    if !first {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("prompt-") && name.ends_with(".txt")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_PROMPT);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn reads_a_credential_and_a_config_entry_alike() {
        let config = open_ferry_core::config::Config::parse(
            "claude-cli:\n  - name: max-1\n    command: /bin/claude\n    config-dir: /srv/a\n    \
             system-prompt: append\n    max-concurrency: 3\n    timeout: 90s\n",
        )
        .expect("load");
        let from_config = Entry::from_config(&config.claude_cli[0]);
        let auth = open_ferry_core::auth::synthesizer::claude_cli::claude_cli_auth(
            0,
            &config.claude_cli[0],
            &open_ferry_core::auth::synthesizer::SynthesisContext::new("", chrono::Utc::now()),
            &mut open_ferry_core::auth::synthesizer::StableIdGenerator::new(),
        );
        assert_eq!(Entry::from_auth(&auth), from_config);
        assert_eq!(
            from_config,
            Entry {
                name: "max-1".into(),
                command: "/bin/claude".into(),
                config_dir: "/srv/a".into(),
                system_prompt: ClaudeCliSystemPrompt::Append,
                max_concurrency: 3,
                timeout: Duration::from_secs(90),
            }
        );

        // A credential without the settings takes the defaults.
        let bare = Entry::from_auth(&Auth::default());
        assert_eq!(bare.system_prompt, ClaudeCliSystemPrompt::Replace);
        assert_eq!(bare.max_concurrency, 2);
        assert_eq!(bare.timeout, Duration::from_secs(600));
        assert!(bare.config_dir_path().is_none());
    }

    #[test]
    fn scrubs_by_name() {
        for (name, keep_account, removed) in [
            ("ANTHROPIC_API_KEY", true, true),
            ("anthropic_base_url", true, true),
            ("CLAUDECODE", true, true),
            ("CLAUDECODE_X", true, true),
            ("CLAUDE_CODE_ENTRYPOINT", true, true),
            ("CLAUDE_CODE_USE_BEDROCK", true, true),
            ("CLAUDE_AGENT_SDK_VERSION", true, true),
            ("CLAUDE_EFFORT", true, true),
            ("claude_pid", true, true),
            ("MAX_THINKING_TOKENS", true, true),
            ("CLAUDE_CODE_OAUTH_TOKEN", true, false),
            ("CLAUDE_CODE_OAUTH_TOKEN", false, true),
            ("claude_code_oauth_token", false, true),
            ("CLAUDE_CONFIG_DIR", true, false),
            ("CLAUDE_CONFIG_DIR", false, true),
            ("PATH", false, false),
            ("HOME", false, false),
            ("ANTHROPIC", false, false),
            ("MY_CLAUDE_KEY", false, false),
        ] {
            assert_eq!(
                scrubbed(&OsString::from(name), keep_account),
                removed,
                "{name} {keep_account}"
            );
        }
    }

    #[test]
    fn names_directories_safely() {
        let entry = |name: &str, config_dir: &str| Entry {
            name: name.into(),
            config_dir: config_dir.into(),
            ..Entry::from_auth(&Auth::default())
        };
        let a = entry("max 1/..", "");
        let name = a.dir_name();
        assert!(name.starts_with("max_1_..-"), "{name}");
        assert_eq!(name.len(), "max_1_..-".len() + 12);
        assert_ne!(a.dir_name(), entry("max 1/..", "/srv").dir_name());
        assert!(entry("..", "").dir_name().starts_with("_..-"));
        assert!(entry("", "").dir_name().starts_with("_-"));
    }

    #[test]
    fn expands_the_home_directory() {
        let Some(home) = home_dir() else {
            return;
        };
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/bin/claude"), home.join("bin/claude"));
        assert_eq!(expand_home("/~/x"), PathBuf::from("/~/x"));
        assert_eq!(expand_home("~user/x"), PathBuf::from("~user/x"));
    }

    #[test]
    fn makes_private_directories() {
        let root = tempfile::tempdir().expect("temp dir");
        let entry = Entry {
            name: "max".into(),
            ..Entry::from_auth(&Auth::default())
        };
        let (dir, work) = entry.dirs(root.path()).expect("dirs");
        assert_eq!(work, dir.join("work"));
        assert!(work.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [&dir, &work] {
                let mode = path.metadata().unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{}", path.display());
            }
        }
        // A second time finds them.
        assert_eq!(entry.dirs(root.path()).expect("dirs"), (dir, work));
    }

    #[test]
    fn a_path_command_isnt_looked_up() {
        let entry = Entry {
            command: "./bin/claude".into(),
            ..Entry::from_auth(&Auth::default())
        };
        assert_eq!(entry.program().unwrap(), PathBuf::from("./bin/claude"));
        let missing = Entry {
            command: "open-ferry-no-such-claude".into(),
            ..Entry::from_auth(&Auth::default())
        };
        let error = missing.program().unwrap_err();
        assert!(error.contains("isn't on PATH"), "{error}");
    }
}
