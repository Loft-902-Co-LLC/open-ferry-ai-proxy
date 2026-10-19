//! Running a staged binary's `--version`, the check that it runs on this
//! machine before open-ferry ever switches to it.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::fetch::BoxFuture;

// Creating a process can wait for the executable's storage or OS checks even
// though its pipes and subsequent exit wait are asynchronous. Keep that wait
// off the serving runtime. An unclaimed Child is dropped if the caller was
// cancelled, preserving kill-on-drop when process creation finishes late.
async fn spawn_child(
    spawn: impl FnOnce() -> std::io::Result<tokio::process::Child> + Send + 'static,
) -> Result<tokio::process::Child, String> {
    tokio::task::spawn_blocking(spawn)
        .await
        .map_err(|error| format!("spawning it failed: {error}"))?
        .map_err(|error| format!("it doesn't start: {error}"))
}

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
            let child = spawn_child(move || command.spawn()).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;

    #[tokio::test(flavor = "current_thread")]
    async fn process_creation_does_not_stall_http_serving() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        assert_ne!(address.port(), 8317);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/", axum::routing::get(|| async { "still serving" })),
            )
            .await
            .unwrap();
        });
        let (entered, arrived) = mpsc::channel();
        let (released, release) = mpsc::channel();
        let observer = std::thread::spawn(move || {
            let response = (|| -> std::io::Result<String> {
                arrived
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(std::io::Error::other)?;
                let wait = Duration::from_secs(5);
                let mut stream = std::net::TcpStream::connect_timeout(&address, wait)?;
                stream.set_read_timeout(Some(wait))?;
                stream.set_write_timeout(Some(wait))?;
                stream.write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")?;
                let mut response = String::new();
                stream.read_to_string(&mut response)?;
                Ok(response)
            })();
            // Always release the fake OS call, including when HTTP timed out.
            let _ = released.send(());
            response
        });
        let result = spawn_child(move || {
            entered.send(()).map_err(std::io::Error::other)?;
            release
                .recv_timeout(Duration::from_secs(10))
                .map_err(std::io::Error::other)?;
            Err(std::io::Error::other("injected process creation failure"))
        })
        .await;
        let response = observer
            .join()
            .unwrap()
            .expect("HTTP must complete while process creation is held");
        server.abort();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("still serving"), "{response}");
        assert!(
            result
                .unwrap_err()
                .contains("injected process creation failure")
        );
    }
}
