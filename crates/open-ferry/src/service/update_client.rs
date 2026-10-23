//! Overlap only the updater's initial downloader build with service preparation.
//! Service creation, errors and settings logs stay at the original boot point.

use std::sync::Arc;
use std::thread::JoinHandle;

use open_ferry_core::config::{Config, SelfUpdateMode};
use open_ferry_update::background::{self, MakeFetch};
use open_ferry_update::fetch::{Fetch, FetchError};

pub(super) type BuiltFetch = Result<Arc<dyn Fetch>, FetchError>;

pub(super) struct UpdateClient {
    task: Option<JoinHandle<BuiltFetch>>,
    make_fetch: MakeFetch,
}

impl UpdateClient {
    pub(super) fn start(config: &Config, enabled: bool) -> Option<Self> {
        Self::start_with(
            config,
            enabled,
            || std::env::var(open_ferry_update::MODE_ENV).ok(),
            background::http_fetch,
        )
    }

    fn start_with(
        config: &Config,
        enabled: bool,
        environment: impl FnOnce() -> Option<String>,
        make_fetch: impl FnOnce() -> MakeFetch,
    ) -> Option<Self> {
        if !enabled || config.self_update.mode() == SelfUpdateMode::Off {
            return None;
        }
        if environment().as_deref().and_then(SelfUpdateMode::parse) == Some(SelfUpdateMode::Off) {
            return None;
        }
        let make_fetch = make_fetch();
        let build = Arc::clone(&make_fetch);
        let proxy_url = config.proxy_url.clone();
        // HttpFetch::new and reqwest's build path emit no tracing events.
        // The Linux verifier's CA diagnostics use `log`, for which this binary
        // installs neither a logger nor a tracing bridge. No span is lost.
        // If spawning fails, finish() builds synchronously at the old point.
        let task = std::thread::Builder::new()
            .name("update-client".into())
            .spawn(move || build(&proxy_url).or_else(|_| build("")))
            .ok()?;
        Some(Self {
            task: Some(task),
            make_fetch,
        })
    }

    fn join(&mut self) -> BuiltFetch {
        match self
            .task
            .take()
            .expect("the update client is joined once")
            .join()
        {
            Ok(fetch) => fetch,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

impl Drop for UpdateClient {
    fn drop(&mut self) {
        // Boot never reached the synchronous build point. Join, but discard
        // the unused result, including a panic, to preserve boot's own outcome.
        // Only a panic has a visible difference: its hook has already reported
        // from update-client, even when the unused result is discarded.
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

pub(super) fn finish(client: Option<UpdateClient>, config: &Config) -> (MakeFetch, BuiltFetch) {
    finish_with(client, config, background::http_fetch)
}

fn finish_with(
    client: Option<UpdateClient>,
    config: &Config,
    make_fetch: impl FnOnce() -> MakeFetch,
) -> (MakeFetch, BuiltFetch) {
    match client {
        Some(mut client) => {
            let fetch = client.join();
            (Arc::clone(&client.make_fetch), fetch)
        }
        None => {
            let make_fetch = make_fetch();
            let fetch = make_fetch(&config.proxy_url).or_else(|_| make_fetch(""));
            (make_fetch, fetch)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::ThreadId;
    use std::time::Duration;

    use open_ferry_update::fetch::BoxFuture;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    use super::*;
    use crate::os_service::Platform;
    use crate::update::scope_probe::Fake;

    struct NoRequests;

    impl Fetch for NoRequests {
        fn get<'a>(
            &'a self,
            _: &'a url::Url,
            _: u64,
            _: Duration,
        ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
            panic!("boot must not download anything")
        }
    }

    #[derive(Clone, Default)]
    struct Logged {
        lines: Arc<Mutex<Vec<(Level, String)>>>,
    }

    struct Message(String);

    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl<S: Subscriber> Layer<S> for Logged {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let mut message = Message(String::new());
            event.record(&mut message);
            self.lines
                .lock()
                .unwrap()
                .push((*event.metadata().level(), message.0));
        }
    }

    #[derive(Clone, Default)]
    struct Attempts(Arc<Mutex<Vec<(String, ThreadId)>>>);

    fn factory(primary: bool, fallback: bool, attempts: Attempts) -> MakeFetch {
        Arc::new(move |proxy| {
            let mut calls = attempts.0.lock().unwrap();
            calls.push((proxy.to_owned(), std::thread::current().id()));
            let succeeds = if calls.len() == 1 { primary } else { fallback };
            if succeeds {
                Ok(Arc::new(NoRequests))
            } else {
                Err(FetchError::Network(format!("build {} failed", calls.len())))
            }
        })
    }

    fn compare_preparation(primary: bool, fallback: bool, installed: bool) {
        let mut config = Config::default();
        config.proxy_url = "http://configured.invalid:3128".into();
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("open-ferry.exe");
        let installation = installed.then_some((dir.path(), binary.as_path()));
        let mut probe = Fake::new(Platform::Windows, false);
        probe.account = Ok(dir.path().to_str().unwrap().to_owned());
        let thread = std::thread::current().id();
        let mut runs = Vec::new();
        // Compare the independent reference with both shipped finish paths.
        for (reference, concurrent) in [(true, false), (false, false), (false, true)] {
            let logged = Logged::default();
            let subscriber = tracing_subscriber::registry().with(logged.clone());
            crate::test_tracing::keep_every_callsite_open();
            tracing::subscriber::with_default(subscriber, || {
                let span = tracing::info_span!("boot");
                let _entered = span.enter();
                tracing::info!("boot before service preparation");
                let attempts = Attempts::default();
                let make_fetch = factory(primary, fallback, attempts.clone());
                let client = concurrent.then(|| {
                    UpdateClient::start_with(&config, true, || None, || Arc::clone(&make_fetch))
                        .expect("start the client build")
                });
                tracing::info!("service prepared");
                let updates =
                    super::super::prepare_update_checks(&probe, &config, installation, || {
                        if reference {
                            // The original synchronous expression, independent of
                            // the threaded implementation under test.
                            let fetch = make_fetch(&config.proxy_url).or_else(|_| make_fetch(""));
                            (make_fetch, fetch)
                        } else {
                            finish_with(client, &config, || make_fetch)
                        }
                    });
                tracing::info!("updater attached");
                assert_eq!(updates.is_some(), primary || fallback);
                let calls = attempts.0.lock().unwrap();
                let proxies: Vec<_> = calls.iter().map(|(proxy, _)| proxy.as_str()).collect();
                let expected = if primary {
                    vec![config.proxy_url.as_str()]
                } else {
                    vec![config.proxy_url.as_str(), ""]
                };
                assert_eq!(proxies, expected);
                for (_, built_on) in &*calls {
                    assert_eq!(*built_on == thread, !concurrent);
                    assert_eq!(*built_on, calls[0].1);
                }
                runs.push((updates.is_some(), logged.lines.lock().unwrap().clone()));
            });
        }
        assert_eq!(runs[0], runs[1]);
        assert_eq!(runs[0], runs[2]);
        let lines = &runs[2].1;
        assert_eq!(
            lines[0],
            (Level::INFO, "boot before service preparation".into())
        );
        assert_eq!(lines[1], (Level::INFO, "service prepared".into()));
        assert_eq!(
            lines.last().unwrap(),
            &(Level::INFO, "updater attached".into())
        );
        if primary || fallback {
            assert!(lines[2..lines.len() - 1].iter().any(|(level, text)| {
                *level == Level::INFO && text.starts_with("automatic updates")
            }));
        } else {
            assert_eq!(
                lines[2],
                (
                    Level::WARN,
                    "update checks don't run: build 2 failed".into()
                )
            );
            if installed {
                assert_eq!(lines[3], (Level::INFO, "automatic switching is unavailable here: the update downloader could not be initialized".into()));
            }
            assert_eq!(lines.len(), if installed { 5 } else { 4 });
        }
    }

    #[test]
    fn primary_success_preserves_service_and_boot_logs_without_fallback() {
        for installed in [false, true] {
            compare_preparation(true, false, installed);
        }
    }

    #[test]
    fn fallback_success_preserves_service_and_boot_logs() {
        for installed in [false, true] {
            compare_preparation(false, true, installed);
        }
    }

    #[test]
    fn fallback_failure_preserves_absent_service_and_ordered_boot_logs() {
        for installed in [false, true] {
            compare_preparation(false, false, installed);
        }
    }

    #[test]
    fn client_build_overlaps_boot_work_before_join() {
        let (entered, started) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let make_fetch: MakeFetch = Arc::new(move |_| {
            let thread = std::thread::current();
            entered
                .send((thread.id(), thread.name().map(str::to_owned)))
                .unwrap();
            released.lock().unwrap().recv().unwrap();
            Ok(Arc::new(NoRequests))
        });
        let config = Config::default();
        let client = UpdateClient::start_with(&config, true, || None, || make_fetch).unwrap();
        let built_on = started.recv_timeout(Duration::from_secs(10));
        // Boot reaches here while the build is still blocked on its release.
        // Release before asserting so a failed assertion cannot strand Drop.
        release.send(()).unwrap();
        assert!(finish(Some(client), &config).1.is_ok());
        let (built_on, name) = built_on.unwrap();
        assert_ne!(built_on, std::thread::current().id());
        assert_eq!(name.as_deref(), Some("update-client"));
    }

    #[test]
    fn client_build_panic_reaches_boot_with_original_payload() {
        for fallback in [false, true] {
            let mut config = Config::default();
            config.proxy_url = "configured".into();
            let make_fetch: MakeFetch = Arc::new(move |proxy| {
                if fallback && !proxy.is_empty() {
                    return Err(FetchError::Network("try direct".into()));
                }
                std::panic::panic_any(902_u32);
            });
            let client = UpdateClient::start_with(&config, true, || None, || make_fetch).unwrap();
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = finish(Some(client), &config);
            }))
            .unwrap_err();
            assert_eq!(panic.downcast_ref::<u32>(), Some(&902));
        }
    }

    #[test]
    fn unused_client_build_panic_is_discarded_on_drop() {
        let make_fetch: MakeFetch = Arc::new(|_| std::panic::panic_any(903_u32));
        let client =
            UpdateClient::start_with(&Config::default(), true, || None, || make_fetch).unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(client))).is_ok());
    }

    #[test]
    fn unused_client_is_joined_before_drop_returns() {
        let finished = Arc::new(AtomicBool::new(false));
        let completed = Arc::clone(&finished);
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let make_fetch: MakeFetch = Arc::new(move |_| {
            released.lock().unwrap().recv().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            completed.store(true, Ordering::Release);
            Ok(Arc::new(NoRequests))
        });
        let client =
            UpdateClient::start_with(&Config::default(), true, || None, || make_fetch).unwrap();
        assert!(!finished.load(Ordering::Acquire));
        release.send(()).unwrap();
        drop(client);
        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn disabled_updates_do_not_read_environment_create_factory_or_start_thread() {
        for enabled in [false, true] {
            let mut config = Config::default();
            if enabled {
                config.self_update.mode = "off".into();
            }
            assert!(
                UpdateClient::start_with(
                    &config,
                    enabled,
                    || panic!("disabled updates must not read the environment"),
                    || panic!("disabled updates must not prepare a downloader"),
                )
                .is_none()
            );
        }
        assert!(
            UpdateClient::start_with(
                &Config::default(),
                true,
                || Some(" OFF ".into()),
                || panic!("environment-disabled updates must not prepare a downloader"),
            )
            .is_none()
        );
    }

    #[test]
    fn real_http_client_builds_without_a_runtime_or_tracing_events() {
        crate::test_tracing::keep_every_callsite_open();
        for (proxy, succeeds) in [
            ("", true),
            ("http://proxy.invalid:3128", true),
            ("socks5://proxy.invalid:1080", false),
        ] {
            let logged = Logged::default();
            let subscriber = tracing_subscriber::registry().with(logged.clone());
            // Call the real constructor on the recording subscriber's thread.
            // A thread-local subscriber cannot capture a child thread's events.
            tracing::subscriber::with_default(subscriber, || {
                let make_fetch = background::http_fetch();
                assert_eq!(make_fetch(proxy).is_ok(), succeeds, "{proxy}");
            });
            assert!(logged.lines.lock().unwrap().is_empty(), "{proxy}");
        }
        // Separately check real construction on a plain thread without a
        // runtime. Its logging is checked by the direct calls above.
        let client =
            UpdateClient::start_with(&Config::default(), true, || None, background::http_fetch)
                .unwrap();
        assert!(finish(Some(client), &Config::default()).1.is_ok());
    }
}
