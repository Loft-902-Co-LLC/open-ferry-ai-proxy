//! `check`'s findings for open-ferry's `management.separate-address`,
//! only when it is set, so a config without it is checked as before:
//! - `management address`: whether something already listens on its port,
//!   found as for the proxy's address; when nothing does, that the
//!   management API and the dashboard are served there alone, and that a
//!   change takes a restart. A port that is `server.port`'s, or an address
//!   that isn't host and port, fails the config's load first.
//! - `management access`, a warning when `management.allow-remote` doesn't
//!   fit the address: one that takes connections from other machines (every
//!   interface, or an address that isn't loopback) while `allow-remote` is
//!   false and `MANAGEMENT_PASSWORD` isn't set, so those machines are
//!   refused; or a loopback one while `allow-remote` is true and
//!   `server.trusted-proxies` is empty, so `allow-remote` does nothing. A
//!   host name isn't judged, as that would take a lookup.
//!
//! The dashboard finding gives the management address's URL.

use open_ferry_core::config::{Config, ManagementAddress, ManagementReach};

use super::{Environment, Finding, PortCheck, check_port};

const ADDRESS: &str = "management address";
const ACCESS: &str = "management access";

/// The management address's findings, when `config` has one.
pub(super) async fn check_management_address(
    config: &Config,
    env: &Environment,
    findings: &mut Vec<Finding>,
) {
    // The loader refuses one that doesn't parse.
    let Ok(Some(address)) = config.remote_management.separate_address() else {
        return;
    };
    findings.push(port_finding(&address).await);
    if let Some(finding) = access_finding(config, env, &address) {
        findings.push(finding);
    }
}

/// Whether the management address's port is free.
async fn port_finding(address: &ManagementAddress) -> Finding {
    let (host, port) = (address.host.as_str(), address.port);
    match check_port(host, port).await {
        PortCheck::NotChecked(what) => Finding::warning(
            ADDRESS,
            format!(
                "management.separate-address host {host} {what}, so whether port {port} is free there isn't checked (check makes no network call)"
            ),
            format!(
                "if the proxy fails to start because the management address is in use, free port {port} or change management.separate-address"
            ),
        ),
        PortCheck::Listening(listening) => Finding::error(
            ADDRESS,
            format!("something already listens on {listening}"),
            "stop it (if it is this proxy, it is already running), or set management.separate-address to a free port",
        ),
        PortCheck::Unknown(unknown) => Finding::warning(
            ADDRESS,
            format!(
                "couldn't tell whether something listens on {}",
                unknown.join("; ")
            ),
            format!(
                "run check again; if the proxy fails to start because the management address is in use, free port {port} or change management.separate-address"
            ),
        ),
        PortCheck::Free(shown) => Finding::ok(
            ADDRESS,
            format!(
                "nothing listens on {shown}; the management API and the dashboard are served on {address} alone, not on server.port (a change takes a restart)"
            ),
        ),
    }
}

/// A warning when `management.allow-remote` doesn't fit the address.
pub(super) fn access_finding(
    config: &Config,
    env: &Environment,
    address: &ManagementAddress,
) -> Option<Finding> {
    let management = &config.remote_management;
    let port = address.port;
    match address.reach() {
        ManagementReach::EveryInterface | ManagementReach::Address
            if !management.allow_remote && !env.management_password =>
        {
            Some(Finding::warning(
                ACCESS,
                format!(
                    "management.separate-address {address} takes connections from other machines, but management.allow-remote is false, so the management API and the dashboard refuse them"
                ),
                format!(
                    "set management.allow-remote to true to manage the proxy from other machines, or listen on loopback only, such as 127.0.0.1:{port}"
                ),
            ))
        }
        ManagementReach::Loopback
            if management.allow_remote && config.trusted_proxies.is_empty() =>
        {
            Some(Finding::warning(
                ACCESS,
                format!(
                    "management.allow-remote is true, but management.separate-address {address} is on loopback and server.trusted-proxies is empty, so no other machine can reach it and allow-remote does nothing"
                ),
                "set management.allow-remote to false, or set management.separate-address to an address other machines can reach",
            ))
        }
        _ => None,
    }
}

/// The dashboard's URL on the management address, when `config` has one.
pub(super) fn dashboard_url(config: &Config) -> Option<String> {
    let address = config.remote_management.separate_address().ok()??;
    Some(crate::init::dashboard_url(
        &address.host,
        i64::from(address.port),
        config.tls.enable,
    ))
}
