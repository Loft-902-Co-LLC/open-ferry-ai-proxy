use std::time::Duration;

/// How long any one wait lasts. A wrong outcome ends far sooner; this is only
/// the bound that keeps a failure from hanging.
pub const LONG: Duration = Duration::from_secs(90);
