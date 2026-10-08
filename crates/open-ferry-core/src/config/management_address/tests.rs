//! Not upstream's: open-ferry's `management.separate-address`, parsed,
//! loaded, checked, saved and edited in both layouts.

use std::fs;

use crate::config::save::render_preserving_comments;
use crate::config::testing::TempDir;
use crate::config::v8_edit::{V8Edit, V8Method, edit_v8, validate_v8_config};
use crate::config::{Config, ConfigErrorKind, ManagementAddress, ManagementReach};

use ManagementReach::{Address, EveryInterface, Loopback, Name};

#[test]
fn parses_host_and_port() {
    for (text, host, port, reach, display, url) in [
        (
            "127.0.0.1:8318",
            "127.0.0.1",
            8318,
            Loopback,
            "127.0.0.1:8318",
            "http://127.0.0.1:8318",
        ),
        (
            " 127.0.0.2:1\t",
            "127.0.0.2",
            1,
            Loopback,
            "127.0.0.2:1",
            "http://127.0.0.2:1",
        ),
        (
            "[::1]:8318",
            "::1",
            8318,
            Loopback,
            "[::1]:8318",
            "http://[::1]:8318",
        ),
        (
            "localhost:8318",
            "localhost",
            8318,
            Loopback,
            "localhost:8318",
            "http://localhost:8318",
        ),
        (
            "LocalHost:65535",
            "LocalHost",
            65535,
            Loopback,
            "LocalHost:65535",
            "http://LocalHost:65535",
        ),
        (
            ":8318",
            "",
            8318,
            EveryInterface,
            ":8318",
            "http://127.0.0.1:8318",
        ),
        (
            "0.0.0.0:9000",
            "0.0.0.0",
            9000,
            EveryInterface,
            "0.0.0.0:9000",
            "http://127.0.0.1:9000",
        ),
        (
            "[::]:9000",
            "::",
            9000,
            EveryInterface,
            "[::]:9000",
            "http://127.0.0.1:9000",
        ),
        (
            "192.0.2.5:8318",
            "192.0.2.5",
            8318,
            Address,
            "192.0.2.5:8318",
            "http://192.0.2.5:8318",
        ),
        (
            "[2001:db8::5]:8318",
            "2001:db8::5",
            8318,
            Address,
            "[2001:db8::5]:8318",
            "http://[2001:db8::5]:8318",
        ),
        (
            "admin.internal:8318",
            "admin.internal",
            8318,
            Name,
            "admin.internal:8318",
            "http://admin.internal:8318",
        ),
    ] {
        let address =
            ManagementAddress::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
        assert_eq!(address.host, host, "{text}");
        assert_eq!(address.port, port, "{text}");
        assert_eq!(address.reach(), reach, "{text}");
        assert_eq!(address.to_string(), display, "{text}");
        assert_eq!(address.base_url(false), url, "{text}");
        assert_eq!(
            address.base_url(true),
            url.replacen("http://", "https://", 1),
            "{text}"
        );
    }
}

#[test]
fn refuses_what_isnt_host_and_port() {
    const EXAMPLE: &str = "write it as host:port, such as 127.0.0.1:8318";
    for (text, message) in [
        (
            "http://127.0.0.1:8318",
            format!("\"http://127.0.0.1:8318\" is a URL; {EXAMPLE}"),
        ),
        (
            "8318",
            format!("\"8318\" has no host; {EXAMPLE}, or :8318 for every interface"),
        ),
        ("127.0.0.1", format!("\"127.0.0.1\" has no port; {EXAMPLE}")),
        ("[::1]", format!("\"[::1]\" has no port; {EXAMPLE}")),
        (
            "::1:8318",
            "\"::1:8318\" needs its IPv6 address in brackets, such as [::1]:8318".to_owned(),
        ),
        ("[::1:8318", "\"[::1:8318\" has a [ without a ]".to_owned()),
        (
            "[127.0.0.1]:8318",
            "[127.0.0.1] isn't an IPv6 address".to_owned(),
        ),
        (
            "me@host:8318",
            format!("\"me@host\" isn't a host; {EXAMPLE}"),
        ),
        (
            "my host:8318",
            format!("\"my host\" isn't a host; {EXAMPLE}"),
        ),
        ("host/x:8318", format!("\"host/x\" isn't a host; {EXAMPLE}")),
        ("127.0.0.1:", "\"\" isn't a port from 1 to 65535".to_owned()),
        (
            "127.0.0.1:0",
            "\"0\" isn't a port from 1 to 65535".to_owned(),
        ),
        (
            "127.0.0.1:65536",
            "\"65536\" isn't a port from 1 to 65535".to_owned(),
        ),
        (
            "127.0.0.1:+8318",
            "\"+8318\" isn't a port from 1 to 65535".to_owned(),
        ),
        (
            "127.0.0.1:08318",
            "\"08318\" isn't a port from 1 to 65535".to_owned(),
        ),
        (
            "127.0.0.1:http",
            "\"http\" isn't a port from 1 to 65535".to_owned(),
        ),
    ] {
        assert_eq!(ManagementAddress::parse(text), Err(message), "{text}");
    }
}

#[test]
fn loads_in_both_layouts() {
    let legacy =
        Config::parse("port: 8317\nremote-management:\n  separate-address: ' 127.0.0.1:8318 '\n")
            .expect("load");
    assert_eq!(legacy.remote_management.separate_address, "127.0.0.1:8318");
    assert_eq!(
        legacy.remote_management.separate_address(),
        Ok(Some(ManagementAddress {
            host: "127.0.0.1".into(),
            port: 8318,
        }))
    );

    let text =
        "config-version: 8\nserver:\n  port: 8317\nmanagement:\n  separate-address: '[::1]:8318'\n";
    validate_v8_config(text.as_bytes()).expect("a valid v8 config");
    let v8 = Config::parse(text).expect("load");
    assert_eq!(v8.remote_management.separate_address, "[::1]:8318");

    // Off: absent, empty or blank.
    assert_eq!(
        Config::default().remote_management.separate_address(),
        Ok(None)
    );
    for text in [
        "port: 8317\n",
        "management: {separate-address: ''}\n",
        "management: {separate-address: '  '}\n",
    ] {
        let config = Config::parse(text).expect(text);
        assert_eq!(config.remote_management.separate_address, "", "{text}");
        assert_eq!(
            config.remote_management.separate_address(),
            Ok(None),
            "{text}"
        );
    }
}

#[test]
fn a_bad_address_fails_the_load() {
    for (text, message) in [
        (
            "management: {separate-address: 127.0.0.1}\n",
            "management.separate-address: \"127.0.0.1\" has no port; \
             write it as host:port, such as 127.0.0.1:8318",
        ),
        (
            "port: 8317\nremote-management: {separate-address: '127.0.0.1:8317'}\n",
            "management.separate-address: port 8317 is server.port's; \
             the management address needs a port of its own",
        ),
        (
            "config-version: 8\nserver: {port: 9000}\nmanagement: {separate-address: ':9000'}\n",
            "management.separate-address: port 9000 is server.port's; \
             the management address needs a port of its own",
        ),
    ] {
        let error = Config::parse(text).expect_err(text);
        assert_eq!(error.kind(), ConfigErrorKind::Invalid, "{text}");
        assert_eq!(error.to_string(), message, "{text}");
    }

    // Without a port, the proxy's isn't known, so any port will do.
    let config = Config::parse("management: {separate-address: ':8317'}\n").expect("load");
    assert_eq!(config.remote_management.separate_address, ":8317");

    let error = Config::parse("management: {separate-address: [a]}\n").expect_err("a list");
    assert_eq!(error.kind(), ConfigErrorKind::Decode);
}

/// The file `cfg` saves as over `data`.
fn saved(data: &str, cfg: &Config, migrate_v8: bool) -> String {
    let out = render_preserving_comments(data.as_bytes(), cfg, migrate_v8).expect("save");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn saves_and_round_trips_in_both_layouts() {
    // Legacy: written under remote-management, and only when set.
    let legacy = "# head\nport: 8317\nremote-management:\n  allow-remote: false\n";
    let mut cfg = Config::parse(legacy).expect("load");
    let out = saved(legacy, &cfg, false);
    assert!(!out.contains("separate-address"), "{out}");
    cfg.remote_management.separate_address = "127.0.0.1:8318".into();
    let out = saved(legacy, &cfg, false);
    assert!(out.starts_with("# head\n"), "{out}");
    assert!(!out.contains("config-version"), "stays legacy:\n{out}");
    assert!(out.contains("separate-address: 127.0.0.1:8318"), "{out}");
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.remote_management, cfg.remote_management, "{out}");

    // Moving to the v8 layout keeps it, under management.
    let out = saved(&out, &reloaded, true);
    validate_v8_config(out.as_bytes()).expect("a valid v8 config");
    assert!(
        !out.contains("# separate-address"),
        "kept, not commented out:\n{out}"
    );
    let v8 = Config::parse(&out).expect("reload");
    assert_eq!(
        v8.remote_management.separate_address, "127.0.0.1:8318",
        "{out}"
    );

    // v8: emptying it clears the file's.
    let mut cfg = v8;
    cfg.remote_management.separate_address.clear();
    let out = saved(&out, &cfg, false);
    validate_v8_config(out.as_bytes()).expect("a valid v8 config");
    let cleared = Config::parse(&out).expect("reload");
    assert_eq!(cleared.remote_management.separate_address, "", "{out}");

    // A v8 file without it doesn't gain it.
    let plain = "config-version: 8\nserver:\n  port: 8317\nmanagement:\n  allow-remote: false\n";
    let cfg = Config::parse(plain).expect("load");
    let out = saved(plain, &cfg, false);
    assert!(!out.contains("separate-address"), "{out}");
}

#[test]
fn v8_edits_set_and_check_it() {
    let dir = TempDir::new();
    let path = dir.write("config.yaml", "config-version: 8\nserver:\n  port: 8317\n");
    let put = V8Edit {
        method: V8Method::Put,
        path: vec!["management".into(), "separate-address".into()],
        body: br#""127.0.0.1:8318""#.to_vec(),
        yaml: false,
    };
    let config = edit_v8(&path, &put).expect("edit");
    assert_eq!(config.remote_management.separate_address, "127.0.0.1:8318");
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("separate-address:"), "{text}");
    assert_eq!(
        Config::load(&path).expect("load").remote_management,
        config.remote_management
    );

    // A value the loader refuses is refused, and the file is kept.
    let same_port = V8Edit {
        body: br#"":8317""#.to_vec(),
        ..put
    };
    let error = edit_v8(&path, &same_port).expect_err("server.port's");
    assert!(
        error
            .to_string()
            .contains("management.separate-address: port 8317"),
        "{error}"
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), text);
}

#[test]
fn reload_lines_show_both_values() {
    let old = Config::parse("port: 8317\n").expect("load");
    let new = Config::parse("port: 8317\nmanagement: {separate-address: '127.0.0.1:8318'}\n")
        .expect("load");
    assert_eq!(
        crate::config::diff::build_change_details(&old, &new),
        ["remote-management.separate-address:  -> 127.0.0.1:8318"]
    );
}
