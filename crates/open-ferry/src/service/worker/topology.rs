//! open-ferry's own: the listeners a supervised worker serves.
//!
//! A worker serves the topology its supervisor's `welcome` gives (v3 2.6),
//! not the one its config names: the supervisor bound the sockets, and a
//! switch hands the same sockets from one worker to the next, so a config
//! edited meanwhile can't move them. The topology is the main socket, named
//! `main`, and the management socket, named `management`, which only a
//! config with `management.separate-address` has; each with the address it
//! is bound to and whether it serves TLS.
//!
//! When the config disagrees on the host, the port, TLS or the separate
//! management address, the worker serves the topology's and warns once per
//! run, naming each difference ([`Topology::warn_once`]).

use std::net::{IpAddr, SocketAddr, TcpListener};

use open_ferry_core::config::Config;
use open_ferry_supervisor::protocol::ListenerTopology;

/// The name of the main socket.
pub(super) const MAIN: &str = "main";

/// The name of the management socket.
pub(super) const MANAGEMENT: &str = "management";

/// A socket to serve on.
pub(super) struct Served {
    pub(super) listener: TcpListener,
    /// The address `welcome` says it is bound to.
    pub(super) address: SocketAddr,
    /// Whether it serves TLS.
    pub(super) tls: bool,
}

/// The sockets a worker serves.
pub(super) struct Topology {
    pub(super) main: Served,
    pub(super) management: Option<Served>,
}

impl Topology {
    /// The topology of `welcome`, each entry with the socket `listeners`
    /// names it. Entries with another name than `main` and `management`
    /// are ignored, as a newer supervisor's addition, and are logged.
    ///
    /// # Errors
    /// The line to log when there is no main socket, or an entry has no
    /// socket, or two have the same name.
    pub(super) fn from_welcome(
        welcome: &[ListenerTopology],
        mut listeners: Vec<(String, TcpListener)>,
    ) -> Result<Self, String> {
        let mut names = std::collections::BTreeSet::new();
        for (name, _) in &listeners {
            if !names.insert(name) {
                tracing::warn!(
                    "duplicate supplied socket name {name:?}; only the first match is served"
                );
            }
        }
        let mut main = None;
        let mut management = None;
        for entry in welcome {
            let slot = match entry.name.as_str() {
                MAIN => &mut main,
                MANAGEMENT => &mut management,
                other => {
                    tracing::warn!("ignoring the unknown listener {other:?} in the welcome");
                    continue;
                }
            };
            if slot.is_some() {
                return Err(format!(
                    "the welcome names the {} listener twice",
                    entry.name
                ));
            }
            let Some(at) = listeners.iter().position(|(name, _)| *name == entry.name) else {
                return Err(format!(
                    "the welcome names a {} listener, and none was given",
                    entry.name
                ));
            };
            let (_, listener) = listeners.swap_remove(at);
            // Failure here means the supplied handle cannot be inspected,
            // not merely a topology mismatch. Keep it fatal: serving an
            // invalid socket cannot safely establish the ready contract.
            let actual = listener
                .local_addr()
                .map_err(|error| format!("couldn't inspect the {} socket: {error}", entry.name))?;
            if actual != entry.address {
                tracing::warn!(
                    "the {} welcome address {} differs from its socket's local_addr {actual}; serving the socket",
                    entry.name,
                    entry.address
                );
            }
            *slot = Some(Served {
                listener,
                address: entry.address,
                tls: entry.tls,
            });
        }
        for (name, _) in &listeners {
            if !welcome.iter().any(|entry| entry.name == *name) {
                tracing::warn!("ignoring socket {name:?}, which the welcome doesn't name");
            }
        }
        let main = main.ok_or_else(|| "the welcome has no main listener".to_owned())?;
        Ok(Self { main, management })
    }

    /// Whether a listener serves TLS.
    pub(super) fn needs_tls(&self) -> bool {
        self.main.tls || self.management.as_ref().is_some_and(|served| served.tls)
    }

    /// What `config` says differently from this topology, one line for each
    /// difference. Nothing for a config that agrees.
    pub(super) fn differences(&self, config: &Config) -> Vec<String> {
        let mut found = Vec::new();
        let tls = config.tls.enable;
        let configured = display_host(&config.host, config.port);
        if !host_agrees(&config.host, self.main.address.ip())
            || i64::from(self.main.address.port()) != config.port
        {
            found.push(format!(
                "the main address is {} (config: {configured})",
                self.main.address
            ));
        }
        if self.main.tls != tls {
            found.push(format!(
                "TLS on the main listener is {} (config: {})",
                on_off(self.main.tls),
                on_off(tls)
            ));
        }
        match (
            config.remote_management.separate_address(),
            &self.management,
        ) {
            (Ok(Some(address)), Some(served)) => {
                if !host_agrees(&address.host, served.address.ip())
                    || served.address.port() != address.port
                {
                    found.push(format!(
                        "the management address is {} (config: {address})",
                        served.address
                    ));
                }
                if served.tls != tls {
                    found.push(format!(
                        "TLS on the management listener is {} (config: {})",
                        on_off(served.tls),
                        on_off(tls)
                    ));
                }
            }
            (Ok(Some(address)), None) => found.push(format!(
                "there is no management listener (config: {address})"
            )),
            (Ok(None), Some(served)) => found.push(format!(
                "there is a management listener on {} (config: none)",
                served.address
            )),
            // The config doesn't load with a bad address; nothing to compare.
            (Err(_), _) | (Ok(None), None) => {}
        }
        found
    }

    /// Logs one warning naming the differences of `config`, if there are
    /// any and `warned` says none was logged yet, and sets `warned`.
    pub(super) fn warn_once(&self, config: &Config, warned: &mut bool) {
        if *warned {
            return;
        }
        let found = self.differences(config);
        if found.is_empty() {
            return;
        }
        *warned = true;
        tracing::warn!(
            "the config disagrees with the listeners the supervisor gave; serving the supervisor's: {}",
            found.join("; ")
        );
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// `host` and `port` as the config writes them.
fn display_host(host: &str, port: i64) -> String {
    let host = host.trim();
    if host.is_empty() {
        format!("every interface, port {port}")
    } else if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Whether the socket address `bound` is one the config's `host` names. An
/// empty host is every interface, which only an unspecified address is. A
/// host name other than `localhost` isn't resolved: it agrees, as there is
/// nothing sure to compare.
fn host_agrees(host: &str, bound: IpAddr) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return bound.is_unspecified();
    }
    if host.eq_ignore_ascii_case("localhost") {
        return bound.is_loopback();
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => ip == bound || (ip.is_unspecified() && bound.is_unspecified()),
        Err(_) => true,
    }
}
