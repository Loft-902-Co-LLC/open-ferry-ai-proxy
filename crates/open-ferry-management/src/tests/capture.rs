//! Captures what a test thread logs, for the tests of this crate that
//! check a route or writer logs, and what it logs.

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, Once};

use tracing::subscriber::Interest;

thread_local! {
    /// Where this thread's logs go while a test captures them.
    static CAPTURED: RefCell<Option<Arc<Mutex<Captured>>>> = const { RefCell::new(None) };
}

/// What a test logs on its thread, one event a line: the message, then
/// any other field as ` name=value`.
#[derive(Clone, Default)]
pub(crate) struct Logs(Arc<Mutex<Captured>>);

/// The events of a capture: their lines, and their levels in the same order.
#[derive(Default)]
struct Captured {
    text: String,
    levels: Vec<tracing::Level>,
}

impl Logs {
    /// Captures what this thread logs until the guard is dropped. A
    /// `#[tokio::test]` runs its tasks on its thread, so their logs too.
    ///
    /// The subscriber is the global one, for every thread: a scoped one
    /// misses events whose callsite another thread registered first.
    pub(crate) fn capture() -> (Self, Capturing) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Capture);
            tracing::callsite::rebuild_interest_cache();
        });
        let logs = Self::default();
        CAPTURED.with(|captured| *captured.borrow_mut() = Some(Arc::clone(&logs.0)));
        (logs, Capturing)
    }

    pub(crate) fn text(&self) -> String {
        self.0.lock().unwrap().text.clone()
    }

    /// The level of each captured event, in the order they were logged.
    pub(crate) fn levels(&self) -> Vec<tracing::Level> {
        self.0.lock().unwrap().levels.clone()
    }
}

/// Ends this thread's capture when dropped.
pub(crate) struct Capturing;

impl Drop for Capturing {
    fn drop(&mut self) {
        CAPTURED.with(|captured| captured.borrow_mut().take());
    }
}

/// The subscriber that keeps the events of a thread that captures them.
struct Capture;

impl tracing::Subscriber for Capture {
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        CAPTURED.with(|captured| captured.borrow().is_some())
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let Some(captured) = CAPTURED.with(|captured| captured.borrow().clone()) else {
            return;
        };
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let mut captured = captured.lock().unwrap();
        captured.text.push_str(&line);
        captured.text.push('\n');
        captured.levels.push(*event.metadata().level());
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Writes an event's fields as [`Logs`] keeps them.
struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}
