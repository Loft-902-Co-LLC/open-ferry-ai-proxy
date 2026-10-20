//! Captures automatic-switch events with a scoped subscriber.
//!
//! A callsite first reached by another thread with no subscriber can cache
//! `never` and hide later events from a scoped capture. Keep every callsite
//! open before constructing a capture, as the core crate's tests do.

use std::fmt::Write;
use std::sync::{Arc, Mutex, Once};

use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber, span};

#[derive(Clone)]
pub(crate) struct Capture(pub Arc<Mutex<Vec<String>>>);

impl Default for Capture {
    fn default() -> Self {
        keep_every_callsite_open();
        Self(Arc::default())
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(event.metadata().level().to_string());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

struct Fields(String);
impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

/// Installs, once, a global subscriber that takes no event but keeps every
/// callsite's interest at `sometimes`, so each event asks the current
/// thread's subscriber. Call it before capturing a thread's logs.
fn keep_every_callsite_open() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(Open);
        tracing::callsite::rebuild_interest_cache();
    });
}

/// Interested in every callsite, enabled for none.
struct Open;

impl Subscriber for Open {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _attributes: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

#[test]
fn captures_a_callsite_first_reached_without_a_scoped_subscriber() {
    fn emit() {
        tracing::info!("a callsite first reached on another thread");
    }

    let capture = Capture::default();
    let dispatch = tracing::Dispatch::new(capture.clone());
    std::thread::spawn(emit).join().unwrap();
    // Reuse the dispatch: constructing another would rebuild interest.
    tracing::dispatcher::with_default(&dispatch, emit);

    let lines = capture.0.lock().unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
}

#[tokio::test]
async fn runtime_follower_reports_takeover_without_claiming_the_watch_finished() {
    use crate::{SwitchError, SwitchEvent, SwitchHandle, SwitchProgress};
    use tracing::instrument::WithSubscriber;

    struct ResultProgress(serde_json::Value);
    impl SwitchProgress for ResultProgress {
        fn next(
            &mut self,
            _: std::time::Instant,
        ) -> crate::fetch::BoxFuture<'_, Result<SwitchEvent, SwitchError>> {
            Box::pin(async {
                Ok(SwitchEvent::Done {
                    result: self.0.clone(),
                })
            })
        }
    }
    for outcome in ["switched", "rolled-back"] {
        for watching in [false, true] {
            for gap in [None, Some(143)] {
                let mut result =
                    serde_json::json!({"result": outcome, "reason": "/private/detail"});
                if let Some(gap) = gap {
                    result["gap_ms"] = gap.into();
                }
                if watching {
                    result["phase"] = "watching".into();
                }
                let logs = Capture::default();
                super::follow_switch(
                    SwitchHandle::new("takeover".into(), Box::new(ResultProgress(result))),
                    "0.2.0".into(),
                )
                .with_subscriber(logs.clone())
                .await;
                let events = logs.0.lock().unwrap();
                assert_eq!(events.len(), 1, "{events:?}");
                let event = &events[0];
                assert!(event.starts_with("INFO "), "{event}");
                assert!(event.contains("version=0.2.0"), "{event}");
                assert!(event.contains(outcome), "{event}");
                if gap.is_some() {
                    assert!(
                        event.contains("gap_ms=143") && !event.contains("Some("),
                        "{event}"
                    );
                } else {
                    assert!(!event.contains("gap_ms="), "{event}");
                }
                assert!(
                    !event.contains("unknown")
                        && !event.contains("finished")
                        && !event.contains("/private/"),
                    "{event}"
                );
                if watching {
                    assert!(
                        event.contains("took over; watching it before the switch is final")
                            && event.contains("phase=\"watching\""),
                        "{event}"
                    );
                } else {
                    assert!(
                        event.contains("reported a result") && !event.contains("took over"),
                        "{event}"
                    );
                }
            }
        }
    }
}
