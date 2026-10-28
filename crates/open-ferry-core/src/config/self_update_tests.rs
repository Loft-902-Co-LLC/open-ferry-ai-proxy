//! Not upstream's: open-ferry's own `self-update` section, loaded, checked,
//! saved and edited in both layouts.

use std::time::Duration;

use super::save::{render_preserving_comments, update_nested_scalar};
use super::testing::TempDir;
use super::v8_edit::{V8Edit, V8Method, edit_v8, validate_v8_config};
use super::{Config, SelfUpdate, SelfUpdateMode};

const SECTION: &str = "\
self-update:
  mode: notify
  check-every: 12h
";

fn full() -> SelfUpdate {
    SelfUpdate {
        channel: String::new(),
        mode: "notify".into(),
        check_every: "12h".into(),
    }
}

/// The file `cfg` saves as over `data`.
fn saved(data: &str, cfg: &Config) -> String {
    let out = render_preserving_comments(data.as_bytes(), cfg, false).expect("save");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn decodes_the_section_and_its_defaults() {
    let config = Config::parse(SECTION).expect("load");
    assert_eq!(config.self_update, full());
    assert_eq!(config.self_update.mode(), SelfUpdateMode::Notify);
    assert_eq!(
        config.self_update.check_every(),
        Duration::from_secs(12 * 3600)
    );

    // Without the section: auto, every six hours.
    let plain = Config::parse("port: 8317\n").expect("load");
    assert_eq!(plain.self_update, SelfUpdate::default());
    assert_eq!(plain.self_update.mode(), SelfUpdateMode::Auto);
    assert_eq!(
        plain.self_update.check_every(),
        SelfUpdate::DEFAULT_CHECK_EVERY
    );

    // Case and surrounding space don't matter; off needs no quotes.
    let off = Config::parse("self-update:\n  mode: ' OFF '\n").expect("load");
    assert_eq!(off.self_update.mode, "off");
    assert_eq!(off.self_update.mode(), SelfUpdateMode::Off);
    let bare = Config::parse("self-update:\n  mode: off\n").expect("load");
    assert_eq!(bare.self_update.mode(), SelfUpdateMode::Off);
}

#[test]
fn the_modes_order_by_how_much_they_do() {
    assert!(SelfUpdateMode::Off < SelfUpdateMode::Notify);
    assert!(SelfUpdateMode::Notify < SelfUpdateMode::Auto);
    for mode in [
        SelfUpdateMode::Off,
        SelfUpdateMode::Notify,
        SelfUpdateMode::Auto,
    ] {
        assert_eq!(SelfUpdateMode::parse(mode.as_str()), Some(mode));
    }
    assert_eq!(SelfUpdateMode::parse(""), Some(SelfUpdateMode::Auto));
    assert_eq!(SelfUpdateMode::parse("sometimes"), None);
}

#[test]
fn a_bad_setting_fails_the_load() {
    for (text, want) in [
        ("self-update:\n  mode: sometimes\n", "self-update.mode"),
        ("self-update:\n  mode: false\n", "self-update.mode"),
        (
            "self-update:\n  check-every: soon\n",
            "self-update.check-every",
        ),
        (
            "self-update:\n  check-every: 0s\n",
            "self-update.check-every",
        ),
        (
            "self-update:\n  check-every: -1h\n",
            "self-update.check-every",
        ),
    ] {
        let error = Config::parse(text).expect_err(text);
        assert!(error.to_string().contains(want), "{text}: {error}");
    }
}

#[test]
fn is_a_known_section_of_a_v8_document() {
    let text = format!("config-version: 8\nserver:\n  port: 8317\n{SECTION}");
    let config = Config::parse(&text).expect("load");
    assert_eq!(config.self_update, full());
    validate_v8_config(text.as_bytes()).expect("a valid v8 config");

    let unknown = "config-version: 8\nself-update:\n  often: 1h\n";
    let error = validate_v8_config(unknown.as_bytes()).expect_err("unknown key");
    assert!(error.to_string().contains("often"), "{error}");
}

#[test]
fn saves_and_round_trips_in_the_legacy_layout() {
    let data = format!("# head\nport: 8317\n{SECTION}");
    let mut cfg = Config::parse(&data).expect("load");
    cfg.self_update.mode = "off".into();
    let out = saved(&data, &cfg);
    assert!(out.starts_with("# head\n"), "{out}");
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.self_update, cfg.self_update, "{out}");

    // Fields at their zero value are left out, and the section with them.
    cfg.self_update.check_every.clear();
    let out = saved(&data, &cfg);
    assert!(!out.contains("check-every"), "{out}");
    cfg.self_update = SelfUpdate::default();
    let out = saved(&data, &cfg);
    assert!(!out.contains("self-update"), "{out}");

    // A file without the section doesn't gain one.
    let plain = "port: 8317\n";
    let cfg = Config::parse(plain).expect("load");
    assert!(!saved(plain, &cfg).contains("self-update"));
}

#[test]
fn saves_and_round_trips_in_the_v8_layout() {
    let data = format!("config-version: 8\nserver:\n  port: 8317\n{SECTION}");
    let mut cfg = Config::parse(&data).expect("load");
    cfg.self_update.mode = "auto".into();
    let out = saved(&data, &cfg);
    validate_v8_config(out.as_bytes()).expect("a valid v8 config");
    assert!(
        !out.contains("# self-update"),
        "kept, not commented out:\n{out}"
    );
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.self_update, cfg.self_update, "{out}");

    // The section alone doesn't make a legacy file v8.
    let legacy = "port: 8317\nself-update:\n  mode: notify\n";
    let cfg = Config::parse(legacy).expect("load");
    let out = saved(legacy, &cfg);
    assert!(out.contains("port: 8317"), "stays legacy:\n{out}");
    assert_eq!(
        Config::parse(&out).expect("reload").self_update.mode(),
        SelfUpdateMode::Notify
    );
}

#[test]
fn v8_edits_set_and_check_the_section() {
    let dir = TempDir::new();
    let path = dir.write("config.yaml", "config-version: 8\nserver:\n  port: 8317\n");
    let put = V8Edit {
        method: V8Method::Put,
        path: vec!["self-update".into(), "mode".into()],
        body: br#""off""#.to_vec(),
        yaml: false,
    };
    let config = edit_v8(&path, &put).expect("edit");
    assert_eq!(config.self_update.mode(), SelfUpdateMode::Off);
    assert_eq!(
        Config::load(&path).expect("load").self_update,
        config.self_update
    );

    let bad = V8Edit {
        body: br#""sometimes""#.to_vec(),
        ..put.clone()
    };
    edit_v8(&path, &bad).expect_err("unknown mode");
    assert_eq!(
        Config::load(&path).expect("load").self_update.mode(),
        SelfUpdateMode::Off
    );

    let unknown = V8Edit {
        path: vec!["self-update".into()],
        body: br#"{"often":"1h"}"#.to_vec(),
        ..put
    };
    let error = edit_v8(&path, &unknown).expect_err("unknown key");
    assert!(error.to_string().contains("often"), "{error}");
}

/// `open-ferry update -mode` sets the mode with the nested-scalar writer,
/// in a file with or without the section, keeping the rest.
#[test]
fn the_nested_scalar_writer_sets_the_mode() {
    let dir = TempDir::new();
    for raw in [
        "# keep me\nport: 8317\n",
        "port: 8317\nself-update:\n  # how often\n  check-every: 12h\n",
        "config-version: 8\nserver:\n  port: 8317\n",
    ] {
        let path = dir.write("config.yaml", raw);
        update_nested_scalar(&path, &["self-update", "mode"], "off").expect("write");
        let data = std::fs::read_to_string(&path).expect("read");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.self_update.mode(), SelfUpdateMode::Off, "{data}");
        for line in raw.lines().filter(|line| line.starts_with('#')) {
            assert!(data.contains(line), "lost {line:?}:\n{data}");
        }
        if raw.contains("check-every") {
            assert_eq!(config.self_update.check_every, "12h", "{data}");
            assert!(data.contains("# how often"), "{data}");
        }
        update_nested_scalar(&path, &["self-update", "mode"], "auto").expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.self_update.mode(), SelfUpdateMode::Auto);
    }
}

#[test]
fn reload_lines_name_each_change() {
    let old = Config::parse("port: 8317\n").expect("load");
    let new = Config::parse(SECTION).expect("load");
    let lines = super::diff::build_change_details(&old, &new);
    for line in [
        "self-update.mode:  -> notify",
        "self-update.check-every:  -> 12h",
    ] {
        assert!(lines.iter().any(|l| l == line), "{line}: {lines:?}");
    }
}

#[test]
fn channels_default_validate_and_survive_v8_edits() {
    use super::SelfUpdateChannel;
    for (value, expected) in [
        ("", SelfUpdateChannel::Stable),
        ("stable", SelfUpdateChannel::Stable),
        (" Preview ", SelfUpdateChannel::Preview),
    ] {
        let config = Config::parse(format!("self-update:\n  channel: '{value}'\n")).unwrap();
        assert_eq!(config.self_update.channel(), expected);
        let out = saved("port: 8317\n", &config);
        assert_eq!(Config::parse(&out).unwrap().self_update.channel(), expected);
    }
    assert_eq!(
        Config::default().self_update.channel(),
        SelfUpdateChannel::Stable
    );
    assert!(
        Config::parse("self-update:\n  channel: beta\n")
            .unwrap_err()
            .to_string()
            .contains("self-update.channel: must be stable or preview")
    );
    let dir = TempDir::new();
    let path = dir.write("channel.yaml", "config-version: 8\nserver:\n  port: 8317\n");
    let put = V8Edit {
        method: V8Method::Put,
        path: vec!["self-update".into(), "channel".into()],
        body: br#""preview""#.to_vec(),
        yaml: false,
    };
    assert_eq!(
        edit_v8(&path, &put).unwrap().self_update.channel(),
        SelfUpdateChannel::Preview
    );
    let bad = V8Edit {
        body: br#""beta""#.to_vec(),
        ..put
    };
    assert!(edit_v8(&path, &bad).is_err());
    assert_eq!(
        Config::load(&path).unwrap().self_update.channel(),
        SelfUpdateChannel::Preview
    );
}

#[test]
fn stable_saves_omit_channel_for_pre_channel_validators() {
    // origin/main's config.SelfUpdate fields, before update-channels.
    const MAIN_SELF_UPDATE_KEYS: &[&str] = &["check-every", "mode"];
    let dir = TempDir::new();
    for spelling in ["", "stable", " STABLE "] {
        let raw = format!(
            "config-version: 8\nserver:\n  port: 8317\nself-update:\n  mode: off\n  channel: '{spelling}'\n"
        );
        let cfg = Config::parse(&raw).unwrap();
        let out = saved(&raw, &cfg);
        let node = super::save::unmarshal(out.as_bytes()).unwrap();
        if let Some(section) = super::save::yaml_path(&node.content[0], "self-update") {
            assert!(
                section
                    .pairs()
                    .all(|(key, _)| MAIN_SELF_UPDATE_KEYS.contains(&key.value.as_str())),
                "{out}"
            );
        }
        validate_v8_config(out.as_bytes()).unwrap();
        assert!(!out.contains("channel:"), "{out}");
        let path = dir.write("stable.yaml", &raw);
        update_nested_scalar(&path, &["self-update", "channel"], "stable").unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("channel:"));
        let put = V8Edit {
            method: V8Method::Put,
            path: vec!["self-update".into(), "channel".into()],
            body: br#""stable""#.to_vec(),
            yaml: false,
        };
        edit_v8(&path, &put).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("channel:"));
        super::save::write_file(&path, raw.as_bytes()).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("channel:"));
    }
}

#[test]
fn stable_channel_omission_does_not_repair_duplicate_input() {
    let dir = TempDir::new();
    let path = dir.write("unchanged.yaml", "port: 8317\n");
    for prefix in ["", "config-version: 8\n"] {
        for section in [
            "self-update:\n  channel: stable\n  channel: preview\n",
            "self-update:\n  channel: stable\nself-update:\n  channel: preview\n",
        ] {
            assert!(
                super::save::write_file(&path, format!("{prefix}{section}").as_bytes()).is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "port: 8317\n");
        }
    }
}
