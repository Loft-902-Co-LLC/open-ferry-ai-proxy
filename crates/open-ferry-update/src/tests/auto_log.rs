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
