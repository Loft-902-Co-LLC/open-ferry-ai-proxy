//! Not upstream's: the worker's state machine, over a duplex channel and
//! 127.0.0.1 loopback listeners.

use std::fmt::Debug;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};

use open_ferry_core::config::Config;
use open_ferry_supervisor::protocol::ListenerTopology;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

use super::topology::{MAIN, MANAGEMENT, Topology};

/// The warnings and errors logged while a test runs.
#[derive(Clone, Default)]
struct Logged(Arc<Mutex<Vec<(Level, String)>>>);

impl Logged {
    /// The messages logged at `level` or worse so far.
    fn at_least(&self, level: Level) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(logged, _)| *logged <= level)
            .map(|(_, message)| message.clone())
            .collect()
    }
}

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for Logged {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), message.0));
    }
}

/// Runs `work` with the log captured on this thread.
fn capture<T>(work: impl FnOnce() -> T) -> (T, Logged) {
    crate::test_tracing::keep_every_callsite_open();
    let logged = Logged::default();
    let subscriber = tracing_subscriber::registry().with(logged.clone());
    let result = tracing::subscriber::with_default(subscriber, work);
    (result, logged)
}

/// A listener on a loopback port of the system's choosing.
fn loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

fn entry(name: &str, address: SocketAddr, tls: bool) -> ListenerTopology {
    ListenerTopology {
        name: name.into(),
        address,
        tls,
    }
}

/// A config that says what a welcome of `main` (and `management`) says.
fn config_for(main: SocketAddr, management: Option<SocketAddr>) -> Config {
    let mut config = Config::parse(format!("host: 127.0.0.1\nport: {}\n", main.port())).unwrap();
    if let Some(management) = management {
        config.remote_management.separate_address = management.to_string();
    }
    config
}

mod machine;

mod listeners {
    use super::*;

    // The sockets are matched to the welcome's entries by name, whatever
    // order they come in.
    #[test]
    fn listeners_are_matched_to_the_welcome_by_name() {
        let (main, main_address) = loopback();
        let (management, management_address) = loopback();
        let topology = Topology::from_welcome(
            &[
                entry(MAIN, main_address, false),
                entry(MANAGEMENT, management_address, true),
            ],
            vec![(MANAGEMENT.into(), management), (MAIN.into(), main)],
        )
        .ok()
        .unwrap();
        assert_eq!(topology.main.listener.local_addr().unwrap(), main_address);
        assert!(!topology.main.tls);
        let served = topology.management.unwrap();
        assert_eq!(served.listener.local_addr().unwrap(), management_address);
        assert!(served.tls);
    }

    #[test]
    fn a_welcome_without_a_main_listener_or_a_socket_is_refused() {
        let (listener, address) = loopback();
        let refused = |welcome: &[ListenerTopology], listeners: Vec<(String, TcpListener)>| {
            Topology::from_welcome(welcome, listeners).err().unwrap()
        };
        assert_eq!(
            refused(&[], vec![(MAIN.into(), listener.try_clone().unwrap())]),
            "the welcome has no main listener"
        );
        assert_eq!(
            refused(&[entry(MAIN, address, false)], Vec::new()),
            "the welcome names a main listener, and none was given"
        );
        assert_eq!(
            refused(
                &[entry(MAIN, address, false), entry(MAIN, address, false)],
                vec![(MAIN.into(), listener)]
            ),
            "the welcome names the main listener twice"
        );
    }

    #[test]
    fn an_unknown_listener_is_ignored_and_logged() {
        let (main, address) = loopback();
        let (other, other_address) = loopback();
        let (topology, logged) = capture(|| {
            Topology::from_welcome(
                &[
                    entry(MAIN, address, false),
                    entry("metrics", other_address, false),
                ],
                vec![(MAIN.into(), main), ("metrics".into(), other)],
            )
        });
        assert!(topology.ok().unwrap().management.is_none());
        let warned = logged.at_least(Level::WARN);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("metrics"), "{warned:?}");
    }

    #[test]
    fn captures_a_production_callsite_first_reached_without_a_subscriber() {
        fn warn_about_unknown_listener() {
            let (main, address) = loopback();
            assert!(
                Topology::from_welcome(
                    &[
                        entry(MAIN, address, false),
                        entry("metrics", address, false)
                    ],
                    vec![(MAIN.into(), main)],
                )
                .is_ok()
            );
        }

        let ((), logged) = capture(|| {
            std::thread::spawn(warn_about_unknown_listener)
                .join()
                .unwrap();
            // Reuse the capture's subscriber: a new dispatch rebuilds interest.
            warn_about_unknown_listener();
        });
        let lines = logged.at_least(Level::WARN);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("ignoring the unknown listener \"metrics\""));
    }

    #[test]
    fn duplicate_supplied_socket_names_are_warned() {
        let (main, address) = loopback();
        let (duplicate, _) = loopback();
        let (topology, logged) = capture(|| {
            Topology::from_welcome(
                &[entry(MAIN, address, false)],
                vec![(MAIN.into(), main), (MAIN.into(), duplicate)],
            )
        });
        assert!(topology.is_ok());
        let lines = logged.at_least(Level::WARN);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("duplicate supplied socket name") && lines[0].contains("main"));
    }

    #[test]
    fn a_socket_not_named_by_the_welcome_is_warned_once() {
        let (main, address) = loopback();
        let (other, _) = loopback();
        let (topology, logged) = capture(|| {
            Topology::from_welcome(
                &[entry(MAIN, address, false)],
                vec![(MAIN.into(), main), ("extra".into(), other)],
            )
        });
        assert!(topology.is_ok());
        let lines = logged.at_least(Level::WARN);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("extra") && lines[0].contains("doesn't name"));
    }

    #[test]
    fn a_welcome_address_different_from_the_socket_is_warned_once() {
        let (main, address) = loopback();
        let (_, different) = loopback();
        let (topology, logged) = capture(|| {
            Topology::from_welcome(&[entry(MAIN, different, false)], vec![(MAIN.into(), main)])
        });
        assert_eq!(
            topology.unwrap().main.listener.local_addr().unwrap(),
            address
        );
        let lines = logged.at_least(Level::WARN);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("local_addr") && lines[0].contains(&different.to_string()));
    }

    fn topology(management: bool, tls: bool) -> (Topology, SocketAddr, Option<SocketAddr>) {
        let (main, main_address) = loopback();
        let mut welcome = vec![entry(MAIN, main_address, tls)];
        let mut listeners = vec![(MAIN.to_owned(), main)];
        let mut management_address = None;
        if management {
            let (listener, address) = loopback();
            welcome.push(entry(MANAGEMENT, address, tls));
            listeners.push((MANAGEMENT.to_owned(), listener));
            management_address = Some(address);
        }
        let topology = Topology::from_welcome(&welcome, listeners).ok().unwrap();
        (topology, main_address, management_address)
    }

    #[test]
    fn a_config_that_agrees_has_no_differences() {
        let (topology, main, management) = topology(true, false);
        assert_eq!(
            topology.differences(&config_for(main, management)),
            Vec::<String>::new()
        );
        let (topology, main, _) = self::topology(false, false);
        assert!(topology.differences(&config_for(main, None)).is_empty());
    }

    #[test]
    fn each_difference_is_named() {
        let (topology, main, management) = topology(true, false);
        let configured = |edit: &dyn Fn(&mut Config)| {
            let mut config = config_for(main, management);
            edit(&mut config);
            topology.differences(&config)
        };

        let port = main.port().wrapping_add(1);
        let found = configured(&|config| config.port = i64::from(port));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("the main address is "), "{found:?}");
        assert!(found[0].contains(&format!("127.0.0.1:{port}")), "{found:?}");

        let found = configured(&|config| config.host = "10.1.2.3".into());
        assert!(found[0].starts_with("the main address is "), "{found:?}");

        let found = configured(&|config| config.tls.enable = true);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with("TLS on the main listener is off"));
        assert!(found[1].starts_with("TLS on the management listener is off"));

        let found = configured(&|config| config.remote_management.separate_address.clear());
        assert!(
            found[0].contains("there is a management listener"),
            "{found:?}"
        );

        let found = configured(&|config| {
            config.remote_management.separate_address = "127.0.0.1:9".into();
        });
        assert!(
            found[0].starts_with("the management address is "),
            "{found:?}"
        );

        let (topology, main, _) = self::topology(false, false);
        let mut config = config_for(main, None);
        config.remote_management.separate_address = "127.0.0.1:9".into();
        let found = topology.differences(&config);
        assert!(
            found[0].contains("there is no management listener"),
            "{found:?}"
        );
    }

    // The empty host is every interface, which a loopback socket isn't.
    #[test]
    fn hosts_are_compared_by_what_they_name() {
        let (topology, main, _) = topology(false, false);
        let mut config = config_for(main, None);
        config.host = "localhost".into();
        assert!(topology.differences(&config).is_empty());
        config.host = "some.host.example".into();
        assert!(topology.differences(&config).is_empty());
        config.host = String::new();
        assert_eq!(topology.differences(&config).len(), 1);
    }

    #[test]
    fn the_disagreement_is_warned_once_per_run() {
        let (topology, main, _) = topology(false, false);
        let mut config = config_for(main, None);
        config.tls.enable = true;
        config.port = i64::from(main.port()) + 1;
        let (warned, logged) = capture(|| {
            let mut warned = false;
            topology.warn_once(&config, &mut warned);
            topology.warn_once(&config, &mut warned);
            warned
        });
        assert!(warned);
        let lines = logged.at_least(Level::WARN);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("the main address is"), "{lines:?}");
        assert!(
            lines[0].contains("TLS on the main listener is off"),
            "{lines:?}"
        );
        assert!(lines[0].contains("serving the supervisor's"), "{lines:?}");

        // A config that agrees logs nothing and leaves the flag.
        let (warned, logged) = capture(|| {
            let mut warned = false;
            topology.warn_once(&config_for(main, None), &mut warned);
            warned
        });
        assert!(!warned);
        assert!(logged.at_least(Level::WARN).is_empty());
    }
}
