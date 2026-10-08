//! Running a staged binary's `--version`, the check that it runs on this
//! machine before open-ferry ever switches to it.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::fetch::BoxFuture;

/// Runs a binary's `--version`.
pub trait Runner: Send + Sync {
    /// What `binary --version` printed, trimmed, when it exited with
    /// success within `timeout`; else what went wrong.
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// Runs the binary as a process of its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner;

/// The most output kept from the run.
const MAX_OUTPUT: usize = 4 * 1024;

impl Runner for ProcessRunner {
    fn version<'a>(
        &'a self,
        binary: &'a Path,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut command = tokio::process::Command::new(binary);
            command
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let child = command
                .spawn()
                .map_err(|error| format!("it doesn't start: {error}"))?;
            let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return Err(format!("running it failed: {error}")),
                Err(_) => {
                    return Err(format!(
                        "it didn't finish within {} seconds",
                        timeout.as_secs()
                    ));
                }
            };
            let text = |bytes: &[u8]| {
                let end = bytes.len().min(MAX_OUTPUT);
                String::from_utf8_lossy(bytes.get(..end).unwrap_or_default())
                    .trim()
                    .to_owned()
            };
            if !output.status.success() {
                return Err(format!(
                    "it exited with {}: {}",
                    output.status,
                    text(&output.stderr)
                ));
            }
            Ok(text(&output.stdout))
        })
    }
}
