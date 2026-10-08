//! For the tests that capture what a thread logs with a scoped subscriber.
//!
//! tracing caches, at each callsite, whether any subscriber wants its
//! events. A thread that reaches a callsite first while only one dispatcher
//! is registered asks just its own default, and with none caches `never`
//! for every thread: a scoped subscriber elsewhere then misses the event.
//! [`keep_every_callsite_open`] prevents that.

use std::sync::Once;

use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber, span};

/// Installs, once, a global subscriber that takes no event but keeps every
/// callsite's interest at `sometimes`, so each event asks the current
/// thread's subscriber. Call it before capturing a thread's logs.
pub(crate) fn keep_every_callsite_open() {
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
