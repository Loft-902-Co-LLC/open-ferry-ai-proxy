//! Not upstream's: the update crate's tests.

mod archive;
mod background;
mod blue_green;
mod fetch;
#[cfg(not(feature = "test-support"))]
mod support;
#[cfg(feature = "test-support")]
use crate::test_support as support;
mod switch_view;
mod switching;
mod units;
mod updater;
