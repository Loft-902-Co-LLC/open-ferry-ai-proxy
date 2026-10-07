//! Reading the model catalogs from their sources: upstream's
//! catalog_sources_test.go, catalog_reload_test.go and
//! catalog_policy_test.go, with files in place of URLs, and open-ferry's
//! own cases.

use std::fmt::Write as _;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::*;

/// How long a test runtime waits between looks at a file: long enough
/// never to look within a test.
const NEVER: Duration = Duration::from_secs(3600);

/// An absolute path to `name` in the temp directory, which needn't exist.
fn file_source(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("open-ferry-catalog-test-{name}"))
        .to_string_lossy()
        .into_owned()
}

/// Writes `data` to `name` in `dir`, and returns its path.
fn write(dir: &Path, name: &str, data: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path.to_string_lossy().into_owned()
}

fn sources(catalog: &str, codex_catalog: &str) -> CatalogSources {
    CatalogSources {
        catalog: catalog.to_owned(),
        codex_catalog: codex_catalog.to_owned(),
        devin_catalog: String::new(),
    }
}

/// The built-in general catalog with a Claude model `id` added.
fn with_claude_model(id: &str) -> Vec<u8> {
    let mut root: serde_json::Value = serde_json::from_str(embedded_catalog_json()).unwrap();
    root["claude"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({ "id": id }));
    serde_json::to_vec(&root).unwrap()
}

/// The built-in Codex client catalog with its default template only.
fn codex_with_one_model() -> Vec<u8> {
    let mut root: serde_json::Value =
        serde_json::from_slice(codex_client::EMBEDDED_CATALOG).unwrap();
    root["models"]
        .as_array_mut()
        .unwrap()
        .retain(|model| model["slug"] == codex_client::DEFAULT_TEMPLATE);
    serde_json::to_vec(&root).unwrap()
}

/// An updater reading with `fetch` and publishing with `publish`.
fn updater(
    fetch: impl Fn(&str) -> Result<Vec<u8>, String> + Send + Sync + 'static,
    publish: impl Fn(&[u8]) -> Result<Vec<String>, String> + Send + Sync + 'static,
) -> Arc<Updater> {
    Updater::new(
        "catalog",
        Box::new(fetch),
        Box::new(publish),
        Arc::new(Notifier::default()),
        NEVER,
    )
}

/// An updater that reads each source as its own text, and sends what it
/// publishes.
fn echo_updater() -> (Arc<Updater>, mpsc::Receiver<String>) {
    let (published, receiver) = mpsc::channel();
    let updater = updater(
        |source| Ok(source.as_bytes().to_vec()),
        move |data| {
            published
                .send(String::from_utf8_lossy(data).into_owned())
                .unwrap();
            Ok(Vec::new())
        },
    );
    (updater, receiver)
}

/// A runtime over two echo updaters, and what each publishes.
fn echo_runtime() -> (
    CatalogRuntime,
    mpsc::Receiver<String>,
    mpsc::Receiver<String>,
) {
    let (general, general_published) = echo_updater();
    let (codex, codex_published) = echo_updater();
    let runtime = CatalogRuntime::with_updaters(
        Arc::new(CatalogStore::new()),
        Arc::new(Notifier::default()),
        general,
        codex,
    );
    (runtime, general_published, codex_published)
}

// Ported from catalog_sources_test.go (TestCatalogFetcherSources), the file
// and validation cases, for the general and Codex client catalogs: an empty
// source is the built-in catalog, as upstream's `embed` is; a file is read,
// and read again each time; a missing or invalid file fails. The URL cases
// are left out, as URL sources aren't fetched, and so is the Devin
// catalog, which isn't ported.
#[test]
fn catalog_fetcher_sources() {
    type Fetcher = fn(&str) -> Result<Vec<u8>, String>;
    let cases: [(&str, Fetcher, &[u8]); 2] = [
        ("general", fetch_general, embedded_catalog_json().as_bytes()),
        ("codex", fetch_codex, codex_client::EMBEDDED_CATALOG),
    ];
    for (name, fetch, data) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "catalog.json", data);
        for source in ["", path.as_str()] {
            assert_eq!(fetch(source).as_deref(), Ok(data), "{name}: {source:?}");
        }
        assert!(fetch(&format!("{path}.missing")).is_err(), "{name}");
        std::fs::write(&path, "invalid").unwrap();
        assert!(fetch(&path).is_err(), "{name}: the file was not read again");
    }
}

// Not upstream's, for readCatalogSource: a file over 8 MiB is refused, as
// upstream refuses it, and a path that isn't absolute is never read.
#[test]
fn catalog_files_have_a_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "big.json", &vec![b' '; MAX_CATALOG_SIZE + 1]);
    assert_eq!(
        read_catalog_file(&path),
        Err("catalog exceeds size limit".to_owned())
    );
    std::fs::write(&path, vec![b' '; MAX_CATALOG_SIZE]).unwrap();
    assert_eq!(
        read_catalog_file(&path).map(|data| data.len()),
        Ok(MAX_CATALOG_SIZE)
    );
    let error = read_catalog_file("catalog.json").unwrap_err();
    assert_eq!(error, "catalog.json isn't an absolute path");
}

// Ported from catalog_sources_test.go
// (TestCatalogSourceSwitchRejectsStalePublication), with file sources: a
// new source is read while a slow read of the old one is under way, the
// old read publishes nothing when it ends, and configuring the same source
// again starts nothing.
#[test]
fn catalog_source_switch_rejects_stale_publication() {
    let old = file_source("old");
    let new = file_source("new");
    let (started, on_start) = mpsc::channel();
    let (release, on_release) = mpsc::channel::<()>();
    let on_release = Mutex::new(on_release);
    let (published, on_publish) = mpsc::channel();
    let slow = old.clone();
    let u = updater(
        move |source| {
            if source == slow {
                started.send(()).unwrap();
                let _ = on_release.lock().unwrap().recv();
            }
            Ok(source.as_bytes().to_vec())
        },
        move |data| {
            published
                .send(String::from_utf8_lossy(data).into_owned())
                .unwrap();
            Ok(Vec::new())
        },
    );
    lock(&u.state).generation = 1;
    let reader = {
        let u = Arc::clone(&u);
        thread::spawn(move || u.refresh(&old, 1))
    };
    on_start.recv_timeout(Duration::from_secs(5)).unwrap();
    u.configure(&new);
    assert_eq!(
        on_publish.try_recv().as_deref(),
        Ok(new.as_str()),
        "new source blocked behind old fetch"
    );
    release.send(()).unwrap();
    reader.join().unwrap();
    assert_eq!(
        on_publish.try_recv(),
        Err(TryRecvError::Empty),
        "stale publication"
    );
    u.configure(&new);
    assert_eq!(lock(&u.state).generation, 2, "unchanged source restarted");
}

// Ported from catalog_sources_test.go (TestCatalogRefreshKeepsLastValidData):
// neither a failed read nor a catalog the publisher refuses changes the
// last valid one.
#[test]
fn catalog_refresh_keeps_last_valid_data() {
    let last = Arc::new(Mutex::new("last valid".to_owned()));
    let kept = Arc::clone(&last);
    let failing = updater(
        |_| Err("unavailable".to_owned()),
        move |data| {
            *kept.lock().unwrap() = String::from_utf8_lossy(data).into_owned();
            Ok(Vec::new())
        },
    );
    failing.refresh("", 0);
    assert_eq!(
        *last.lock().unwrap(),
        "last valid",
        "failed fetch changed catalog"
    );
    let invalid = updater(|_| Ok(b"invalid".to_vec()), |_| Err("invalid".to_owned()));
    invalid.refresh("", 0);
    assert_eq!(
        *last.lock().unwrap(),
        "last valid",
        "invalid catalog changed data"
    );
}

// Ported from catalog_reload_test.go (TestCatalogPolicyHotReload), with
// file and empty sources: start reads each source, a reload reads each one
// that changed, a reload after stop starts nothing, and a new start reads
// every source again. Its URL sources become files, as URLs aren't
// fetched; its Home cases are left out, as Home isn't ported; and an empty
// source is the built-in catalog without `-local-model`.
#[test]
fn catalog_policy_hot_reload() {
    let (runtime, general, codex) = echo_runtime();
    let first = file_source("general");
    runtime.start(&sources(&first, ""));
    assert_eq!(general.try_recv().as_deref(), Ok(first.as_str()));
    assert_eq!(codex.try_recv().as_deref(), Ok(""));
    let second = file_source("codex");
    runtime.update(&sources("", &second));
    assert_eq!(general.try_recv().as_deref(), Ok(""));
    assert_eq!(codex.try_recv().as_deref(), Ok(second.as_str()));
    runtime.update(&sources("", &second));
    assert_eq!(general.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(codex.try_recv(), Err(TryRecvError::Empty));
    runtime.update(&CatalogSources::default());
    assert_eq!(general.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(codex.try_recv().as_deref(), Ok(""));

    // Stopping fences reloads, and a new start reads every source again.
    runtime.stop();
    let generation = lock(&runtime.inner.codex.state).generation;
    runtime.update(&sources("", &file_source("stopped")));
    assert_eq!(
        lock(&runtime.inner.codex.state).generation,
        generation,
        "reload after shutdown restarted catalog readers"
    );
    assert_eq!(codex.try_recv(), Err(TryRecvError::Empty));
    runtime.start(&sources(&first, ""));
    assert_eq!(general.try_recv().as_deref(), Ok(first.as_str()));
    assert_eq!(codex.try_recv().as_deref(), Ok(""));
}

// Ported from catalog_policy_test.go (TestEffectiveCatalogSources), without
// Home: for each mix of empty and URL sources, an empty source is the
// built-in catalog, as upstream's is under `-local-model`, and a URL
// becomes the source but is never fetched. TestDisabledCatalogDoesNotFetch
// is left out, as Home isn't ported.
#[test]
fn effective_catalog_sources() {
    const URL: &str = "https://example.com/catalog.json";
    for mask in 0..8_u32 {
        let pick = |bit: u32| if mask & (1 << bit) != 0 { URL } else { "" };
        let given = CatalogSources {
            catalog: pick(0).to_owned(),
            codex_catalog: pick(1).to_owned(),
            devin_catalog: pick(2).to_owned(),
        };
        let (runtime, general, codex) = echo_runtime();
        runtime.start(&given);
        let cases = [
            (&runtime.inner.general, &general, &given.catalog),
            (&runtime.inner.codex, &codex, &given.codex_catalog),
        ];
        for (updater, published, source) in cases {
            assert_eq!(lock(&updater.state).source, *source, "mask {mask}");
            let read = published.try_recv().ok();
            assert_eq!(read, source.is_empty().then(String::new), "mask {mask}");
        }
    }
}

// Not upstream's: a URL source is never fetched. The load or reload that
// sets it logs one warning naming the setting, not the URL, and the
// catalog in use stays, even one read from a file before.
#[test]
fn a_url_source_keeps_the_catalog_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "models.json",
        &with_claude_model("claude-from-file"),
    );
    let runtime = CatalogRuntime::new(Arc::new(CatalogStore::new()), NEVER);
    runtime.start(&sources(&path, ""));
    assert!(runtime.general().lookup("claude-from-file").is_some());

    let warnings = Warnings::capture();
    let url = "https://example.com/models.json?key=secret-key";
    runtime.update(&sources(url, ""));
    runtime.update(&sources(url, ""));
    assert!(runtime.general().lookup("claude-from-file").is_some());
    let lines = warnings.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.contains("models.catalog names a URL")
                && !line.contains("example.com")
                && !line.contains("secret-key")),
        "{lines:?}"
    );
    runtime.stop();
}

// Not upstream's: a file source is read again when it changes, and the
// listener is told which providers' models changed; a file that turns
// invalid keeps the last valid catalog; an empty source brings back the
// built-in one. The Codex client catalog is read from a file too, and its
// changes name no providers.
#[test]
fn a_catalog_file_is_followed() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "models.json",
        &with_claude_model("claude-from-file"),
    );
    let runtime = CatalogRuntime::new(Arc::new(CatalogStore::new()), Duration::from_millis(10));
    let (changes, changed) = mpsc::channel();
    runtime.set_listener(Some(Arc::new(move |providers| {
        let _ = changes.send(providers);
    })));
    let claude = || Some(vec!["claude".to_owned()]);

    runtime.start(&sources(&path, ""));
    assert_eq!(changed.try_recv().ok(), claude());
    assert!(runtime.general().lookup("claude-from-file").is_some());

    std::fs::write(&path, with_claude_model("claude-from-the-changed-file")).unwrap();
    assert_eq!(changed.recv_timeout(Duration::from_secs(10)).ok(), claude());
    let catalog = runtime.general();
    assert!(catalog.lookup("claude-from-the-changed-file").is_some());
    assert!(catalog.lookup("claude-from-file").is_none());

    std::fs::write(&path, "invalid").unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(
        runtime
            .general()
            .lookup("claude-from-the-changed-file")
            .is_some()
    );

    runtime.update(&CatalogSources::default());
    assert_eq!(changed.try_recv().ok(), claude());
    assert_eq!(*runtime.general(), *StaticCatalog::embedded());

    let codex = write(dir.path(), "codex.json", &codex_with_one_model());
    runtime.update(&sources("", &codex));
    assert_eq!(runtime.store().codex().unwrap().templates().count(), 1);
    std::fs::write(&codex, br#"{"models":[]}"#).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(runtime.store().codex().unwrap().templates().count(), 1);
    runtime.stop();
    assert_eq!(changed.try_recv(), Err(TryRecvError::Empty));
}

// Not upstream's, for configureCatalogs: sources that don't validate are
// logged and change nothing.
#[test]
fn invalid_sources_change_nothing() {
    let (runtime, general, codex) = echo_runtime();
    let warnings = Warnings::capture();
    runtime.start(&sources("relative.json", ""));
    assert_eq!(general.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(codex.try_recv(), Err(TryRecvError::Empty));
    let lines = warnings.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("invalid catalog sources")
                && line
                    .contains("models.catalog must be an http(s) URL or an absolute local path")),
        "{lines:?}"
    );
}

// Not upstream's, for SetModelRefreshCallback, notifyModelRefresh and
// mergeProviderNames: changes made with no listener wait for one, merged,
// trimmed and lowercased; with a listener they are told at once, as they
// came.
#[test]
fn changes_wait_for_a_listener() {
    let notifier = Notifier::default();
    notifier.notify(vec!["Claude".to_owned(), " codex ".to_owned()]);
    notifier.notify(vec![
        "claude".to_owned(),
        String::new(),
        "gemini".to_owned(),
    ]);
    notifier.notify(Vec::new());
    let (sender, told) = mpsc::channel();
    notifier.set_listener(Some(Arc::new(move |providers| {
        let _ = sender.send(providers);
    })));
    assert_eq!(
        told.try_recv(),
        Ok(vec![
            "claude".to_owned(),
            "codex".to_owned(),
            "gemini".to_owned()
        ])
    );
    notifier.notify(vec!["Vertex".to_owned()]);
    assert_eq!(told.try_recv(), Ok(vec!["Vertex".to_owned()]));
    notifier.set_listener(None);
    notifier.notify(vec!["xai".to_owned()]);
    assert_eq!(told.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(lock(&notifier.state).pending, ["xai"]);
}

/// The warnings logged on this thread while it lives, each as its fields.
struct Warnings {
    lines: Arc<Mutex<Vec<String>>>,
    _guard: tracing::dispatcher::DefaultGuard,
}

impl Warnings {
    fn capture() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let dispatch = tracing::Dispatch::new(Capture(Arc::clone(&lines)));
        let guard = tracing::dispatcher::set_default(&dispatch);
        Self {
            lines,
            _guard: guard,
        }
    }

    fn lines(&self) -> Vec<String> {
        lock(&self.lines).clone()
    }
}

/// A subscriber that keeps the fields of each warning.
struct Capture(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for Capture {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = Line(String::new());
        event.record(&mut line);
        lock(&self.0).push(line.0);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// A warning's fields, each as ` name=value`.
struct Line(String);

impl tracing::field::Visit for Line {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}
