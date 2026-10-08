//! `open-ferry status`: whether a server runs for the config, where, its
//! version, its credentials by state, today's calls and errors, and how
//! many client keys the config has. It exits with 4 when no server runs.

use std::collections::BTreeMap;

use axum::http::Method;
use serde::Serialize;
use serde_json::Value;

use super::api::encode;
use super::target::{KeySource, Reach, probe};
use super::values::{get, tree_of};
use super::{Context, Failure, Outcome, Report, credentials, exit};

/// Today's calls.
#[derive(Debug, Serialize)]
struct Calls {
    requests: u64,
    errors: u64,
}

/// What `status` found.
#[derive(Debug, Serialize)]
struct Status {
    config: String,
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    management_address: Option<String>,
    /// `ok`, `no_key`, `off` or `refused`, when a server runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    management: Option<&'static str>,
    /// Where the management key came from: `config`,
    /// `MANAGEMENT_PASSWORD` or `key-file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    key_source: Option<KeySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credentials: Option<BTreeMap<&'static str, usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    calls_today: Option<Calls>,
    client_keys: usize,
    /// What couldn't be found out, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

impl Report for Status {
    fn text(&self) -> String {
        let mut out = format!("Config: {}\n", self.config);
        if self.running {
            out.push_str(&format!(
                "Server: running{}",
                self.version
                    .as_deref()
                    .map(|version| format!(", version {version}"))
                    .unwrap_or_default()
            ));
            out.push('\n');
        } else {
            out.push_str(&format!(
                "Server: not running ({})\n",
                self.reason.as_deref().unwrap_or("unknown")
            ));
        }
        if let Some(address) = &self.address {
            out.push_str(&format!("Proxy address: {address}\n"));
        }
        if let Some(address) = &self.management_address
            && Some(address) != self.address.as_ref()
        {
            out.push_str(&format!("Management address: {address}\n"));
        }
        if let Some(management) = self.management {
            let what = match management {
                "ok" => format!(
                    "reached with the key from {}",
                    self.key_source.map_or("unknown", KeySource::describe)
                ),
                "no_key" => "no management key to call it with".to_owned(),
                "off" => "off: the server has no management key".to_owned(),
                _ => "refused the key".to_owned(),
            };
            out.push_str(&format!("Management API: {what}\n"));
        }
        if let Some(credentials) = &self.credentials {
            let total: usize = credentials.values().sum();
            let states: Vec<String> = credentials
                .iter()
                .map(|(state, count)| format!("{count} {state}"))
                .collect();
            out.push_str(&format!(
                "Credentials: {total}{}\n",
                if states.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", states.join(", "))
                }
            ));
        }
        if let Some(calls) = &self.calls_today {
            out.push_str(&format!(
                "Calls today: {} ({} errors)\n",
                calls.requests, calls.errors
            ));
        }
        out.push_str(&format!("Client keys: {}\n", self.client_keys));
        for note in &self.notes {
            out.push_str(&format!("Note: {note}\n"));
        }
        out
    }
}

/// `status`.
pub(crate) async fn status(ctx: &Context) -> Result<Outcome, Failure> {
    let target = probe(ctx).await?;
    let data = std::fs::read(&ctx.path).unwrap_or_default();
    let client_keys = tree_of(&data)
        .ok()
        .and_then(|tree| {
            get(&tree, &["access".to_owned(), "api-keys".to_owned()])
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(0);
    let mut report = Status {
        config: ctx.path.display().to_string(),
        running: true,
        reason: None,
        version: None,
        address: target.proxy_url.clone(),
        management_address: target.management_url.clone(),
        management: None,
        key_source: None,
        credentials: None,
        calls_today: None,
        client_keys,
        notes: Vec::new(),
    };
    if let Err(error) = &target.config {
        report
            .notes
            .push(format!("the config doesn't load: {error}"));
    }
    match &target.reach {
        Reach::NotRunning(why) => {
            report.running = false;
            report.reason = Some(why.clone());
        }
        Reach::NoKey => {
            report.management = Some("no_key");
            report.notes.push(format!(
                "without the key, credentials and calls aren't shown; {}",
                super::target::KEY_HINT
            ));
        }
        Reach::ManagementOff => {
            report.management = Some("off");
            report
                .notes
                .push("with the management API off, credentials and calls aren't shown".to_owned());
        }
        Reach::Refused(failure) => {
            report.management = Some("refused");
            report.notes.push(failure.message.clone());
        }
        Reach::Running(server) => {
            report.management = Some("ok");
            report.version = server.version.clone();
            report.key_source = Some(server.key_source);
            match credentials::all(server).await {
                Ok(listed) => report.credentials = Some(credentials::count_states(&listed)),
                Err(failure) => report
                    .notes
                    .push(format!("credentials: {}", failure.message)),
            }
            let midnight = local_midnight();
            let path = format!(
                "/open-ferry/api/v1/usage/summary?from={}",
                encode(&midnight)
            );
            match server.remote.json(Method::GET, &path, None).await {
                Ok(summary) => {
                    let total = |name: &str| {
                        summary
                            .get("totals")
                            .and_then(|totals| totals.get(name))
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    };
                    report.calls_today = Some(Calls {
                        requests: total("requests"),
                        errors: total("errors"),
                    });
                }
                Err(failure) => report
                    .notes
                    .push(format!("calls today: {}", failure.message)),
            }
        }
    }
    let code = if report.running {
        exit::OK
    } else {
        exit::NOT_RUNNING
    };
    Ok(Outcome::of(&report).code(code))
}

/// The start of today, local time, in RFC 3339.
fn local_midnight() -> String {
    let now = chrono::Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.and_local_timezone(chrono::Local).earliest())
        .unwrap_or(now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}
