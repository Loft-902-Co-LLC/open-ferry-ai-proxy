//! Random input for the OpenAI Responses and Interactions translators'
//! suites (P4 WP4-C, see `crate::interactions::responses`): the request
//! suites' in [`request`] and the response suites' in [`response`], so that
//! if WP4-C is split, WP4-C1 and WP4-C2 each own one. This module needs no
//! change.

pub mod request;
pub mod response;
