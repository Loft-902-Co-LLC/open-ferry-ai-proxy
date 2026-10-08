// Ported from DetectContentType in Go's net/http/sniff.go (go1.26.4,
// BSD-3-Clause), as Go's server applies it to CLIProxyAPI
// sdk/api/handlers/gemini/gemini_handlers.go's raw streams (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `Content-Type` Go's server gives a response that sets none, from the
//! first bytes written: [`open_ferry_core::multipart::detect_content_type`],
//! which the executors also give a form's file without a type.
//!
//! Deviations from upstream: none.

pub(crate) use open_ferry_core::multipart::detect_content_type;
