//! Running Claude Code for one request: its arguments, its prompt file, its
//! standard input, and reading its output a line at a time.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use open_ferry_core::config::ClaudeCliSystemPrompt;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader,
};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;

use super::settings::Entry;
use crate::codex::stream::MAX_LINE;

/// How much of Claude Code's standard error is kept for the debug log.
const MAX_STDERR: usize = 8 * 1024;

/// What one run is given.
pub(crate) struct Invocation<'a> {
    /// The entry.
    pub(crate) entry: &'a Entry,
    /// The model, for `--model`.
    pub(crate) model: &'a str,
    /// The system prompt, written to the prompt file.
    pub(crate) system: &'a str,
    /// The `--effort` level, if any.
    pub(crate) effort: Option<&'a str>,
    /// `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, if any.
    pub(crate) max_output_tokens: Option<u64>,
    /// `MAX_THINKING_TOKENS`, if any.
    pub(crate) thinking_budget: Option<u64>,
    /// The line for standard input, without its newline.
    pub(crate) input: Vec<u8>,
}

/// Claude Code's arguments: print mode with JSON lines in and out, partial
/// messages, `model`, no tools, no MCP servers, no slash commands, no
/// session files, no permission prompts, one turn, the system prompt from
/// `prompt` as `mode` says, and `effort` if given. Never `--bare`, which
/// would skip the sign-in Claude Code keeps.
pub(crate) fn arguments(
    model: &str,
    mode: ClaudeCliSystemPrompt,
    prompt: &Path,
    effort: Option<&str>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--model",
        model,
        "--tools",
        "",
        "--strict-mcp-config",
        "--safe-mode",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--permission-prompts",
        "none",
        "--max-turns",
        "1",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(OsString::from(match mode {
        ClaudeCliSystemPrompt::Replace => "--system-prompt-file",
        ClaudeCliSystemPrompt::Append => "--append-system-prompt-file",
    }));
    args.push(prompt.as_os_str().to_owned());
    if let Some(effort) = effort {
        args.push(OsString::from("--effort"));
        args.push(OsString::from(effort));
    }
    args
}

/// A system prompt file, removed when dropped.
#[derive(Debug)]
pub(crate) struct PromptFile(PathBuf);

impl PromptFile {
    /// Writes `text` to a new file in `dir`, for the user alone on Unix.
    pub(crate) fn create(dir: &Path, text: &str) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("prompt-{}-{n}.txt", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let file_guard = Self(path);
        io::Write::write_all(&mut file, text.as_bytes())?;
        Ok(file_guard)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A running Claude Code, holding its entry's concurrency slot until it
/// ends. Dropping it kills the process and removes the prompt file.
pub(crate) struct Running {
    pub(crate) child: Child,
    pub(crate) stdout: Lines<ChildStdout>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    // Dropped after the child, so the file outlives the process it was
    // made for as far as can be told.
    prompt: Option<PromptFile>,
    slot: Option<OwnedSemaphorePermit>,
}

impl fmt::Debug for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Running")
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

/// Starts Claude Code for `invocation` in the entry's directories under
/// `root`, holding `slot` while it runs, and starts writing its standard
/// input.
pub(crate) fn spawn(
    root: &Path,
    invocation: Invocation<'_>,
    slot: OwnedSemaphorePermit,
) -> Result<Running, String> {
    let entry = invocation.entry;
    let program = entry.program()?;
    let (dir, work) = entry
        .dirs(root)
        .map_err(|error| format!("couldn't make the entry's directory: {error}"))?;
    let prompt = PromptFile::create(&dir, invocation.system)
        .map_err(|error| format!("couldn't write the system prompt file: {error}"))?;
    let mut command = Command::new(&program);
    command
        .args(arguments(
            invocation.model,
            entry.system_prompt,
            prompt.path(),
            invocation.effort,
        ))
        .current_dir(&work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    entry.apply_env(&mut command);
    if let Some(tokens) = invocation.max_output_tokens {
        command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", tokens.to_string());
    }
    if let Some(budget) = invocation.thinking_budget {
        command.env("MAX_THINKING_TOKENS", budget.to_string());
    }
    no_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("couldn't run {}: {error}", program.display()))?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("Claude Code's pipes weren't set up".to_owned());
    };
    let mut input = invocation.input;
    input.push(b'\n');
    // Written on its own, so a large request can't stall reading.
    tokio::spawn(async move {
        if let Err(error) = stdin.write_all(&input).await {
            tracing::debug!("claude-cli: writing the request: {error}");
        }
        let _ = stdin.shutdown().await;
    });
    Ok(Running {
        child,
        stdout: Lines::new(stdout),
        stderr: Some(tokio::spawn(read_stderr(stderr))),
        prompt: Some(prompt),
        slot: Some(slot),
    })
}

/// Keeps no console window from opening for Claude Code on Windows.
#[cfg(windows)]
pub(crate) fn no_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn no_window(_command: &mut Command) {}

/// Reads standard error to its end, keeping the first [`MAX_STDERR`] bytes.
async fn read_stderr(mut stderr: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = MAX_STDERR.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..n.min(room)]);
            }
        }
    }
}

impl Running {
    /// Lets the process end on its own, as it does once it has answered,
    /// then logs its standard error, removes the prompt file and frees the
    /// slot; kills it if it hasn't ended within a few seconds.
    pub(crate) fn finish(mut self, name: String) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let wait =
                tokio::time::timeout(std::time::Duration::from_secs(10), self.child.wait()).await;
            match wait {
                Ok(Ok(status)) if !status.success() => {
                    tracing::debug!("claude-cli {name}: Claude Code exited with {status}");
                }
                Ok(_) => {}
                Err(_) => {
                    tracing::debug!("claude-cli {name}: Claude Code didn't exit; killing it");
                    let _ = self.child.kill().await;
                }
            }
            self.log_stderr(&name).await;
            self.slot.take();
        });
    }

    /// Kills the process and waits for it, then logs its standard error,
    /// removes the prompt file and frees the slot.
    pub(crate) async fn kill(&mut self, name: &str) {
        let _ = self.child.kill().await;
        self.log_stderr(name).await;
        self.prompt.take();
        self.slot.take();
    }

    /// Waits for the process to end, for its exit status.
    pub(crate) async fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
            .await
            .ok()
            .and_then(Result::ok)
    }

    /// Logs what Claude Code wrote to standard error, at debug level,
    /// shortened and with emails and token-like strings masked.
    pub(crate) async fn log_stderr(&mut self, name: &str) {
        let Some(task) = self.stderr.take() else {
            return;
        };
        let text = match tokio::time::timeout(std::time::Duration::from_secs(2), task).await {
            Ok(Ok(bytes)) => bytes,
            _ => return,
        };
        let text = String::from_utf8_lossy(&text);
        let text = text.trim();
        if !text.is_empty() {
            tracing::debug!(
                "claude-cli {name}: Claude Code's standard error: {}",
                super::events::mask(text, MAX_STDERR)
            );
        }
        self.prompt.take();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // `kill_on_drop` ends the process; the prompt file goes after it.
        let _ = self.child.start_kill();
        self.prompt.take();
    }
}

/// Why a line couldn't be read.
#[derive(Debug)]
pub(crate) enum LineError {
    /// The line is longer than the largest kept.
    TooLong,
    /// Reading failed.
    Read(io::Error),
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "a line from Claude Code is over {MAX_LINE} bytes"),
            Self::Read(error) => write!(f, "reading Claude Code's output: {error}"),
        }
    }
}

/// Reads lines, without their line ending, up to [`MAX_LINE`] bytes each.
pub(crate) struct Lines<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    /// The next line; `None` at the end.
    pub(crate) async fn next_line(&mut self) -> Option<Result<Vec<u8>, LineError>> {
        let mut line = Vec::new();
        loop {
            let buffer = match self.reader.fill_buf().await {
                Ok(buffer) => buffer,
                Err(error) => return Some(Err(LineError::Read(error))),
            };
            if buffer.is_empty() {
                return (!line.is_empty()).then(|| Ok(trim_line_end(line)));
            }
            let (taken, done) = match buffer.iter().position(|&b| b == b'\n') {
                Some(at) => (at + 1, true),
                None => (buffer.len(), false),
            };
            if line.len() + taken > MAX_LINE + 2 {
                return Some(Err(LineError::TooLong));
            }
            line.extend_from_slice(&buffer[..taken]);
            self.reader.consume(taken);
            if done {
                return Some(Ok(trim_line_end(line)));
            }
        }
    }
}

fn trim_line_end(mut line: Vec<u8>) -> Vec<u8> {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_the_flags_the_brief_names() {
        let args = arguments(
            "claude-sonnet-5-5",
            ClaudeCliSystemPrompt::Replace,
            Path::new("/tmp/p.txt"),
            None,
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--model",
                "claude-sonnet-5-5",
                "--tools",
                "",
                "--strict-mcp-config",
                "--safe-mode",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--permission-prompts",
                "none",
                "--max-turns",
                "1",
                "--system-prompt-file",
                "/tmp/p.txt",
            ]
        );
        let args = arguments(
            "m",
            ClaudeCliSystemPrompt::Append,
            Path::new("p"),
            Some("high"),
        );
        let tail: Vec<_> = args[args.len() - 4..]
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            tail,
            ["--append-system-prompt-file", "p", "--effort", "high"]
        );
        assert!(!args.iter().any(|arg| arg == "--bare" || arg == "--betas"));
    }

    #[test]
    fn a_prompt_file_goes_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let file = PromptFile::create(dir.path(), "be brief").unwrap();
        let path = file.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "be brief");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let other = PromptFile::create(dir.path(), "").unwrap();
        assert_ne!(other.path(), path);
        drop(file);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn reads_lines_up_to_the_limit() {
        let data: &[u8] = b"one\r\ntwo\n\nlast";
        let mut lines = Lines::new(data);
        let mut got = Vec::new();
        while let Some(line) = lines.next_line().await {
            got.push(String::from_utf8(line.unwrap()).unwrap());
        }
        assert_eq!(got, ["one", "two", "", "last"]);

        let long = vec![b'x'; MAX_LINE + 10];
        let mut lines = Lines::new(long.as_slice());
        assert!(matches!(
            lines.next_line().await,
            Some(Err(LineError::TooLong))
        ));
    }
}
