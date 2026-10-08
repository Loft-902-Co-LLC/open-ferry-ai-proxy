//! Not upstream's: open-ferry's `routing.strategy: quota` and
//! `routing.quota` section, loaded, checked, saved and edited in both
//! layouts.

use super::save::render_preserving_comments;
use super::testing::TempDir;
use super::v8_edit::{V8Edit, V8Method, edit_v8, validate_v8_config};
use super::{Config, RoutingQuota, RoutingStrategy};

const QUOTA: &str = "\
routing:
  strategy: quota
  quota:
    prefer: most-left
    reserve-percent: 10
    check-after: 1h
";

fn full() -> RoutingQuota {
    RoutingQuota {
        prefer: "most-left".into(),
        reserve_percent: 10,
        check_after: "1h".into(),
    }
}

/// The file `cfg` saves as over `data`.
fn saved(data: &str, cfg: &Config) -> String {
    let out = render_preserving_comments(data.as_bytes(), cfg, false).expect("save");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn decodes_the_strategy_and_section() {
    let config = Config::parse(QUOTA).expect("load");
    assert_eq!(config.routing.strategy, "quota");
    assert_eq!(config.routing_strategy(), RoutingStrategy::Quota);
    assert_eq!(RoutingStrategy::Quota.as_str(), "quota");
    assert_eq!(config.routing.quota, full());

    let plain = Config::parse("routing:\n  strategy: Quota\n").expect("load");
    assert_eq!(plain.routing_strategy(), RoutingStrategy::Quota);
    assert_eq!(plain.routing.quota, RoutingQuota::default());
}

#[test]
fn is_a_known_section_of_a_v8_document() {
    let text = format!("config-version: 8\nserver:\n  port: 8317\n{QUOTA}");
    let config = Config::parse(&text).expect("load");
    assert_eq!(config.routing.quota, full());
    validate_v8_config(text.as_bytes()).expect("a valid v8 config");

    let unknown = "config-version: 8\nrouting:\n  quota:\n    reserve: 10\n";
    let error = validate_v8_config(unknown.as_bytes()).expect_err("unknown key");
    assert!(error.to_string().contains("reserve"), "{error}");
}

#[test]
fn saves_and_round_trips_in_the_legacy_layout() {
    let data = format!("# head\nport: 8317\n{QUOTA}");
    let mut cfg = Config::parse(&data).expect("load");
    cfg.routing.quota.reserve_percent = 25;
    let out = saved(&data, &cfg);
    assert!(out.starts_with("# head\n"), "{out}");
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.routing.quota, cfg.routing.quota, "{out}");
    assert_eq!(reloaded.routing.strategy, "quota", "{out}");

    // Fields at their zero value are left out, and the section with them.
    cfg.routing.quota.check_after.clear();
    let out = saved(&data, &cfg);
    assert!(!out.contains("check-after"), "{out}");
    cfg.routing.quota = RoutingQuota::default();
    let out = saved(&data, &cfg);
    assert!(!out.contains("quota:"), "{out}");

    // A file without the section doesn't gain one.
    let plain = "port: 8317\n";
    let cfg = Config::parse(plain).expect("load");
    assert!(!saved(plain, &cfg).contains("quota"));
}

#[test]
fn saves_and_round_trips_in_the_v8_layout() {
    let data = format!("config-version: 8\nserver:\n  port: 8317\n{QUOTA}");
    let mut cfg = Config::parse(&data).expect("load");
    cfg.routing.quota.prefer = "soonest-reset".into();
    let out = saved(&data, &cfg);
    validate_v8_config(out.as_bytes()).expect("a valid v8 config");
    assert!(!out.contains("# quota"), "kept, not commented out:\n{out}");
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.routing.quota, cfg.routing.quota, "{out}");

    // The section alone doesn't make a legacy file v8.
    let legacy = "port: 8317\nrouting:\n  quota:\n    check-after: 30m\n";
    let cfg = Config::parse(legacy).expect("load");
    let out = saved(legacy, &cfg);
    assert!(out.contains("port: 8317"), "stays legacy:\n{out}");
    assert_eq!(
        Config::parse(&out)
            .expect("reload")
            .routing
            .quota
            .check_after,
        "30m"
    );
}

#[test]
fn v8_edits_set_and_check_the_section() {
    let dir = TempDir::new();
    let path = dir.write("config.yaml", "config-version: 8\nserver:\n  port: 8317\n");
    let put = V8Edit {
        method: V8Method::Put,
        path: vec!["routing".into(), "quota".into()],
        body: br#"{"prefer":"most-left","reserve-percent":10,"check-after":"1h"}"#.to_vec(),
        yaml: false,
    };
    let config = edit_v8(&path, &put).expect("edit");
    assert_eq!(config.routing.quota, full());
    assert_eq!(
        Config::load(&path).expect("load").routing.quota,
        config.routing.quota
    );

    let one = V8Edit {
        path: vec!["routing".into(), "quota".into(), "check-after".into()],
        body: br#""2h""#.to_vec(),
        ..put.clone()
    };
    let config = edit_v8(&path, &one).expect("edit");
    assert_eq!(config.routing.quota.check_after, "2h");

    let unknown = V8Edit {
        body: br#"{"reserve":10}"#.to_vec(),
        ..put
    };
    let error = edit_v8(&path, &unknown).expect_err("unknown key");
    assert!(error.to_string().contains("reserve"), "{error}");
}

#[test]
fn reload_lines_name_each_change() {
    let old = Config::parse("routing:\n  strategy: round-robin\n").expect("load");
    let new = Config::parse(QUOTA).expect("load");
    let lines = super::diff::build_change_details(&old, &new);
    for line in [
        "routing.strategy: round-robin -> quota",
        "routing.quota.prefer:  -> most-left",
        "routing.quota.reserve-percent: 0 -> 10",
        "routing.quota.check-after:  -> 1h",
    ] {
        assert!(lines.iter().any(|l| l == line), "{line}: {lines:?}");
    }
}
