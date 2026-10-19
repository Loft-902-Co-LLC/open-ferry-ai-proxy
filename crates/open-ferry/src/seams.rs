//! Not upstream's: the cross-process test seams of a supervised worker (L7,
//! L9.10-L9.12), only in `blue-green-test` builds: fault files, named
//! barriers, `events.log` and the test marker. Everything lives in the data
//! dir the supervisor's `welcome` names, under `.blue-green-test`.
//!
//! Async core barriers offload the synchronous file poll and await it; the
//! synchronous persist barrier runs inside the refresh's blocking save.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::service::worker::hooks::{Barrier, Event, Fault};

/// The directory of fault, barrier and event files, in the data dir.
const DIR: &str = ".blue-green-test";

/// How long a worker under test lives at most: a safety net against
/// orphans. No test may depend on it.
const SAFETY_NET: Duration = Duration::from_secs(600);

/// A signed test release carries its version in a trailer on the real binary.
/// Only the automatic-update process tests use this trailer.
pub fn update_version() -> Option<String> {
    let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(128))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let text = String::from_utf8_lossy(&tail);
    let (_, version) = text.rsplit_once("\n--update-test-version ")?;
    Some(version.strip_suffix("--")?.to_owned())
}

/// Installs the file barriers once, rooted only in the supplied test data dir.
pub fn install_barriers(data_dir: &Path) -> io::Result<()> {
    let root = data_dir.to_path_buf();
    open_ferry_core::test_hooks::install(Box::new(move |name| {
        open_ferry_update::test_seams::hit(&root, name)
    }))
}

/// One worker's seams: where its files are and what it is called.
#[derive(Debug)]
pub struct Seams {
    data_dir: PathBuf,
    marker: String,
}

impl Seams {
    /// The seams of a worker whose data dir is `data_dir`, with the marker
    /// of this binary ([`binary_marker`]). The core's barriers are installed
    /// once per process, rooted in the first data dir; a second install is
    /// not an error.
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        Self::with_marker(data_dir, binary_marker()?)
    }

    pub fn with_marker(data_dir: &Path, marker: String) -> io::Result<Self> {
        std::fs::create_dir_all(data_dir.join(DIR))?;
        match install_barriers(data_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            marker,
        })
    }

    pub fn marker(&self) -> &str {
        &self.marker
    }

    /// The fault `fault`'s file holds for this worker, if any: its value
    /// (empty for a presence fault).
    pub fn fault(&self, fault: Fault) -> Option<String> {
        let text = std::fs::read_to_string(
            self.data_dir
                .join(DIR)
                .join(format!("fault.{}", fault_name(fault))),
        )
        .ok()?;
        parse_fault(&text, &self.marker)
    }

    /// Appends one line of `events.log`. Failing to is logged, and nothing
    /// else: a test that needs the line fails by not finding it.
    pub fn event(&self, name: &str, lease: bool) {
        let mut line = serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(),
            "marker": self.marker,
            "event": name,
            "lease": lease,
        }))
        .unwrap_or_default();
        line.push(b'\n');
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join(DIR).join("events.log"))
            .and_then(|mut file| file.write_all(&line));
        if let Err(error) = written {
            tracing::warn!("couldn't write the test event log: {error}");
        }
    }

    /// Logs the arrival at a barrier, then waits at it: it fails when its
    /// `.fail` file is there, and waits for `.release` while `.hold` is.
    /// Blocking: call it off runtime threads.
    pub fn barrier_blocking(&self, barrier: Barrier, lease: bool) -> io::Result<()> {
        let name = barrier_name(barrier);
        self.event(name, lease);
        open_ferry_update::test_seams::hit(&self.data_dir, name)
    }

    /// Removes the `.arrived` receipts of `barriers`, so a test sees this
    /// transition's arrival, not an earlier one's.
    pub fn clear_arrivals(&self, barriers: &[Barrier]) {
        for barrier in barriers {
            let path = self
                .data_dir
                .join(DIR)
                .join(format!("{}.arrived", barrier_name(*barrier)));
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!("couldn't clear {}: {error}", path.display());
            }
        }
    }
}

/// The `test-marker` file beside this binary, trimmed; the version when
/// there is none.
pub fn binary_marker() -> io::Result<String> {
    let path = std::env::current_exe()?.with_file_name("test-marker");
    match std::fs::read_to_string(path) {
        Ok(marker) => Ok(marker.trim().to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(update_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()))
        }
        Err(error) => Err(error),
    }
}

/// The value of a fault file for the worker marked `marker`: the trimmed
/// text is `<value>[@<marker>]`. With a marker, only the worker so marked
/// applies it; a presence fault may hold nothing or just `@<marker>`.
pub fn parse_fault(text: &str, marker: &str) -> Option<String> {
    let text = text.trim();
    match text.rsplit_once('@') {
        Some((value, target)) if target == marker => Some(value.to_owned()),
        Some(_) => None,
        None => Some(text.to_owned()),
    }
}

/// Exits the process after [`SAFETY_NET`], whatever it is doing.
pub fn start_safety_net() {
    let _ = std::thread::Builder::new()
        .name("test-safety-net".into())
        .spawn(|| {
            std::thread::sleep(SAFETY_NET);
            std::process::exit(70);
        });
}

/// The header every response carries, with the worker's marker.
const MARKER_HEADER: &str = "x-open-ferry-test-marker";

/// Adds the marker header to every response of `router`, whatever answers
/// it (the management listener's 503 included).
pub fn mark(router: axum::Router, marker: &str) -> axum::Router {
    let value = axum::http::HeaderValue::from_str(marker)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("invalid"));
    router.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let value = value.clone();
            async move {
                let mut response = next.run(request).await;
                response.headers_mut().insert(MARKER_HEADER, value);
                response
            }
        },
    ))
}

pub fn fault_name(fault: Fault) -> &'static str {
    match fault {
        Fault::FailLoad => "fail-load",
        Fault::FailBeforeReady => "fail-before-ready",
        Fault::ExitAfter => "exit-after",
        Fault::SlowReady => "slow-ready",
        Fault::IgnoreHandover => "ignore-handover",
        Fault::HoldRefresh => "hold-refresh",
        Fault::HoldLoad => "hold-load",
        Fault::DropChannel => "drop-channel",
    }
}

fn barrier_name(barrier: Barrier) -> &'static str {
    match barrier {
        Barrier::GateClosing => "handover.gate_closing",
        Barrier::RefreshBeforeSave => "refresh.before_save",
        Barrier::LoadBeforeLoaded => "load.before_loaded",
        Barrier::GateClosed => "handover.gate_closed",
        Barrier::LeaseRelease => "lease.release",
        Barrier::BeforeReload => "take_over.before_reload",
    }
}

pub fn event_name(event: Event) -> &'static str {
    match event {
        Event::Loaded => "loaded",
        Event::Active => "active",
        Event::Standby => "standby",
        Event::HandoverWritten => "handover.written",
        Event::HandoverFailed => "handover_failed",
        Event::HandedOver => "handed_over",
        Event::Resumed => "resumed",
        Event::Ready => "ready",
        Event::Go => "go",
        Event::Draining => "draining",
        Event::Drained => "drained",
        Event::PeersGone => "peers_gone",
    }
}

#[cfg(test)]
mod tests;
