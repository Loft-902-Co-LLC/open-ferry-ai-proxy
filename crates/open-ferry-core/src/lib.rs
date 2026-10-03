//! Config, accounts, credential store and routing.
//!
//! So far this holds the seam between the HTTP layer and provider execution:
//! [`exec`] describes a call and its result, and [`models`] which providers
//! serve a model. The credential manager and the provider executors behind
//! them come next.

pub mod exec;
pub mod models;
