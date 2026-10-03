//! Config, accounts, credential store and routing.
//!
//! [`exec`] describes a call and its result, as the HTTP layer hands it to a
//! [`exec::Dispatcher`], and [`models`] which providers serve a model;
//! [`registry`] keeps that for the configured credentials.
//! Behind them, [`auth`] holds credentials and [`executor`] the trait each
//! provider's executor implements.

pub mod auth;
pub mod exec;
pub mod executor;
pub mod models;
pub mod registry;
