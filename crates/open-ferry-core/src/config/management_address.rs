//! open-ferry's `management.separate-address` (`remote-management.separate-address`
//! in the legacy layout): an address of their own for the management API,
//! the dashboard and the dashboard API, so the proxy's address doesn't
//! serve them. Upstream has no such setting; the server binary does the
//! serving.
//!
//! The value is `host:port`: `127.0.0.1:8318`, `[::1]:8318` for an IPv6
//! address, `localhost:8318` for a host name, or `:8318` for every
//! interface. Empty, or absent, it is off and everything is served on the
//! proxy's address, as upstream serves it.
//!
//! Loading trims the value and checks it ([`clean_up`]): it must have a
//! port from 1 to 65535 other than the proxy's own `port`, an IPv6 address
//! must be in brackets, and a URL is refused. A config that fails a check
//! doesn't load, as one with a bad `claude-cli` entry doesn't.
//!
//! CLIProxyAPI reads the same file without complaint: its loader decodes
//! `management` (or `remote-management`) into a struct without this field
//! and without `KnownFields`, so the key is ignored. A save that moves a
//! file to the v8 layout comments it out, as for any `management` key it
//! doesn't know, and its v8 config write (`PUT /v8/management/config.yaml`)
//! refuses a file that has it.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use super::types::{Config, RemoteManagement};
use super::{ConfigError, ConfigErrorKind};

/// The setting's name, in the v8 layout.
pub const SEPARATE_ADDRESS: &str = "management.separate-address";

/// What a value without a port should look like.
const EXAMPLE: &str = "write it as host:port, such as 127.0.0.1:8318";

/// Where the management API, the dashboard and the dashboard API are served
/// when `management.separate-address` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagementAddress {
    /// The host as written, without brackets: an address or a host name;
    /// empty for every interface.
    pub host: String,
    /// The port, never 0.
    pub port: u16,
}

/// Who can reach a [`ManagementAddress`], as far as its host tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagementReach {
    /// A loopback address or `localhost`: this machine only.
    Loopback,
    /// Every interface (an empty host, `0.0.0.0` or `::`).
    EveryInterface,
    /// An address other than loopback: whoever can reach that address.
    Address,
    /// A host name other than `localhost`, which isn't resolved here.
    Name,
}

impl ManagementAddress {
    /// Parses `text`, trimmed, as `host:port`. The error says what is
    /// wrong, without the setting's name.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.contains("://") {
            return Err(format!("{text:?} is a URL; {EXAMPLE}"));
        }
        if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!(
                "{text:?} has no host; {EXAMPLE}, or :{text} for every interface"
            ));
        }
        let (host, port) = match text.strip_prefix('[') {
            Some(rest) => {
                let Some((host, after)) = rest.split_once(']') else {
                    return Err(format!("{text:?} has a [ without a ]"));
                };
                let Some(port) = after.strip_prefix(':') else {
                    return Err(format!("{text:?} has no port; {EXAMPLE}"));
                };
                if host.parse::<Ipv6Addr>().is_err() {
                    return Err(format!("[{host}] isn't an IPv6 address"));
                }
                (host, port)
            }
            None => {
                let Some((host, port)) = text.rsplit_once(':') else {
                    return Err(format!("{text:?} has no port; {EXAMPLE}"));
                };
                if host.contains(':') {
                    return Err(format!(
                        "{text:?} needs its IPv6 address in brackets, such as [::1]:8318"
                    ));
                }
                (host, port)
            }
        };
        let not_host =
            |c: char| c.is_whitespace() || matches!(c, '/' | '[' | ']' | '@' | '?' | '#');
        if host.contains(not_host) {
            return Err(format!("{host:?} isn't a host; {EXAMPLE}"));
        }
        let number = port
            .parse::<u16>()
            .ok()
            .filter(|&number| number != 0 && number.to_string() == port)
            .ok_or_else(|| format!("{port:?} isn't a port from 1 to 65535"))?;
        Ok(Self {
            host: host.to_owned(),
            port: number,
        })
    }

    /// Who can reach the address.
    pub fn reach(&self) -> ManagementReach {
        if self.host.is_empty() {
            return ManagementReach::EveryInterface;
        }
        match self.host.parse::<IpAddr>() {
            Ok(ip) if ip.is_loopback() => ManagementReach::Loopback,
            Ok(ip) if ip.is_unspecified() => ManagementReach::EveryInterface,
            Ok(_) => ManagementReach::Address,
            Err(_) if self.host.eq_ignore_ascii_case("localhost") => ManagementReach::Loopback,
            Err(_) => ManagementReach::Name,
        }
    }

    /// The host a client on this machine uses in a URL: 127.0.0.1 for every
    /// interface, an IPv6 address in brackets, else the host.
    pub fn url_host(&self) -> String {
        if self.reach() == ManagementReach::EveryInterface {
            "127.0.0.1".to_owned()
        } else if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// The root URL a client on this machine reaches the management API at,
    /// over HTTPS when `tls` is on.
    pub fn base_url(&self, tls: bool) -> String {
        let scheme = if tls { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.url_host(), self.port)
    }
}

impl fmt::Display for ManagementAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl RemoteManagement {
    /// The address of their own the management API, the dashboard and the
    /// dashboard API are served on, if `separate-address` sets one; an
    /// error says what is wrong with the value. A config that loaded has a
    /// good one.
    pub fn separate_address(&self) -> Result<Option<ManagementAddress>, String> {
        if self.separate_address.trim().is_empty() {
            return Ok(None);
        }
        ManagementAddress::parse(&self.separate_address).map(Some)
    }
}

/// Trims `management.separate-address` and checks it: a good `host:port`,
/// on a port other than the proxy's.
pub(crate) fn clean_up(config: &mut Config) -> Result<(), ConfigError> {
    let management = &mut config.remote_management;
    management.separate_address = management.separate_address.trim().to_owned();
    let invalid = |message: String| {
        ConfigError::new(
            ConfigErrorKind::Invalid,
            format!("{SEPARATE_ADDRESS}: {message}"),
        )
    };
    let Some(address) = management.separate_address().map_err(invalid)? else {
        return Ok(());
    };
    if i64::from(address.port) == config.port {
        return Err(invalid(format!(
            "port {} is server.port's; the management address needs a port of its own",
            address.port
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
