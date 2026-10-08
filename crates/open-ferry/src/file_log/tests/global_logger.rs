// Ported from CLIProxyAPI internal/logging/global_logger_test.go
// (TestLogFormatterPrintsVersionField,
// TestLogFormatterPrintsMediaForwardingFields,
// TestLogFormatterPrintsPluginFields,
// TestLogFormatterOmitsGenericPathField,
// TestLogFormatterFormatsShortRequestID) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The lines' format.
//!
//! Dropped: TestConfigureLogOutput_V8MigrationMirror and
//! TestConfigureLogOutput_ConcurrentMigration, as open-ferry never
//! migrates a config, so there is no migration warning to mirror to the
//! console.

use chrono::{Local, TimeZone};
use tracing::Level;

use crate::file_log::format::{Entry, Fields, format};

/// The line for an event at `time` (year, month, day, hour, minute,
/// second), logged from `manager.rs:524`.
fn line(
    time: (i32, u32, u32, u32, u32, u32),
    level: Level,
    message: &str,
    fields: &Fields,
) -> String {
    let (year, month, day, hour, minute, second) = time;
    let time = Local
        .with_ymd_and_hms(year, month, day, hour, minute, second)
        .unwrap();
    format(&Entry {
        time,
        level,
        caller: Some("manager.rs:524".to_owned()),
        message,
        fields,
    })
}

// Ports TestLogFormatterPrintsVersionField.
#[test]
fn prints_version_field() {
    let fields = Fields::default().text("version", "2.1.0");
    let line = line(
        (2026, 6, 9, 11, 10, 2),
        Level::INFO,
        "fetched latest antigravity version",
        &fields,
    );
    assert!(line.contains("version=2.1.0"), "{line:?}");
}

// Ports TestLogFormatterPrintsMediaForwardingFields.
#[test]
fn prints_media_forwarding_fields() {
    let fields = Fields::default()
        .text("credential", "Voice credential\nsecondary")
        .text("connection", "via socks5 proxy")
        .text("proxy_scheme", "socks5")
        .text("remote_transport", "tcp")
        .text("media_session_id", "media-session-id")
        .text("call_id", "call-id")
        .text("peer", "remote")
        .text("state", "connected");
    let line = line(
        (2026, 7, 25, 7, 36, 4),
        Level::INFO,
        "codex live remote media forwarding started",
        &fields,
    );
    for want in [
        r#"credential="Voice credential\nsecondary""#,
        r#"connection="via socks5 proxy""#,
        r#"proxy_scheme="socks5""#,
        r#"remote_transport="tcp""#,
        r#"media_session_id="media-session-id""#,
        r#"call_id="call-id""#,
        r#"peer="remote""#,
        r#"state="connected""#,
    ] {
        assert!(line.contains(want), "{line:?} lacks {want}");
    }
    assert_eq!(line.matches('\n').count(), 1, "{line:?}");
}

// Not upstream's: the connection reuse fields upstream's Antigravity
// connection trace logs, which isn't ported, come in their place and are
// quoted as upstream quotes them.
#[test]
fn prints_connection_trace_fields() {
    use crate::file_log::format::FieldValue;

    let mut fields = Fields::default()
        .text("idle_time", "1.5s")
        .text("upstream_host", "example.test")
        .text("operation", "generate")
        .text("auth_index", "7")
        .text("auth_id", "auth-1")
        .text("provider", "codex");
    fields.set("reused", FieldValue::Plain("true".to_owned()));
    fields.set("was_idle", FieldValue::Plain("false".to_owned()));
    let line = line(
        (2026, 10, 2, 22, 12, 20),
        Level::DEBUG,
        "upstream connection",
        &fields,
    );
    assert_eq!(
        line,
        "[2026-10-02 22:12:20] [--------] [debug] [manager.rs:524] upstream connection \
         provider=codex auth_id=\"auth-1\" auth_index=\"7\" operation=\"generate\" \
         upstream_host=\"example.test\" reused=true was_idle=false idle_time=1.5s\n"
    );
}

// Ports TestLogFormatterPrintsPluginFields.
#[test]
fn prints_plugin_fields() {
    let fields = Fields::default()
        .text("plugin_id", "sample-provider")
        .text("plugin_name", "Sample Provider")
        .text("version", "0.2.0")
        .text("active_version", "0.1.0")
        .text("retired_version", "0.2.0")
        .text("path", "plugins/windows/amd64/sample-provider-v0.2.0.dll")
        .text(
            "active_path",
            "plugins/windows/amd64/sample-provider-v0.1.0.dll",
        )
        .text(
            "retired_path",
            "plugins/windows/amd64/sample-provider-v0.2.0.dll",
        );
    let line = line(
        (2026, 6, 25, 20, 10, 0),
        Level::INFO,
        "pluginhost: plugin loaded",
        &fields,
    );
    for want in [
        "plugin_id=sample-provider",
        "plugin_name=Sample Provider",
        "version=0.2.0",
        "active_version=0.1.0",
        "retired_version=0.2.0",
        "path=plugins/windows/amd64/sample-provider-v0.2.0.dll",
        "active_path=plugins/windows/amd64/sample-provider-v0.1.0.dll",
        "retired_path=plugins/windows/amd64/sample-provider-v0.2.0.dll",
    ] {
        assert!(line.contains(want), "{line:?} lacks {want}");
    }
}

// Ports TestLogFormatterOmitsGenericPathField.
#[test]
fn omits_generic_path_field() {
    let fields = Fields::default()
        .text("path", "auths/private-token.json")
        .text(
            "active_path",
            "plugins/windows/amd64/sample-provider-v0.1.0.dll",
        )
        .text(
            "retired_path",
            "plugins/windows/amd64/sample-provider-v0.2.0.dll",
        );
    let line = line(
        (2026, 6, 25, 20, 20, 0),
        Level::WARN,
        "failed to roll back token",
        &fields,
    );
    for forbidden in ["path=", "active_path=", "retired_path="] {
        assert!(!line.contains(forbidden), "{line:?} has {forbidden}");
    }
}

// Ports TestLogFormatterFormatsShortRequestID.
#[test]
fn formats_short_request_id() {
    let time = (2026, 9, 28, 12, 0, 0);
    let full = "018f3a5b-1234-7abc-def0-12345678abcd";
    let uuid = line(
        time,
        Level::INFO,
        "handling request",
        &Fields::default().text("request_id", full),
    );
    assert!(uuid.contains("[5678abcd]"), "{uuid:?}");
    assert!(!uuid.contains(full), "{uuid:?}");

    let short = line(
        time,
        Level::INFO,
        "handling short id request",
        &Fields::default().text("request_id", "00000042"),
    );
    assert!(short.contains("[00000042]"), "{short:?}");

    let none = line(time, Level::INFO, "system event", &Fields::default());
    assert!(none.contains("[--------]"), "{none:?}");
}

/// Not upstream's: the whole line, with the level padded and a warning
/// shown as `warn`, the message's trailing newlines trimmed, and the
/// fields in upstream's order whatever order they were logged in.
#[test]
fn writes_upstreams_line() {
    let fields = Fields::default()
        .text("reason", "quota")
        .text("model", "gpt-5")
        .text("provider", "codex")
        .text("unlisted", "never shown");
    let line = line(
        (2026, 6, 9, 11, 10, 2),
        Level::WARN,
        "cooling down\r\n",
        &fields,
    );
    assert_eq!(
        line,
        "[2026-06-09 11:10:02] [--------] [warn ] [manager.rs:524] cooling down \
         provider=codex model=gpt-5 reason=\"quota\"\n"
    );
    let info = format(&Entry {
        time: Local.with_ymd_and_hms(2026, 6, 9, 11, 10, 2).unwrap(),
        level: Level::INFO,
        caller: None,
        message: "no caller",
        fields: &Fields::default(),
    });
    assert_eq!(info, "[2026-06-09 11:10:02] [--------] [info ] no caller\n");
}
