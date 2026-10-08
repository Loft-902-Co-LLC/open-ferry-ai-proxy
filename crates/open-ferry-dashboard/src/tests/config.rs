//! `POST /config/undo`, over a config file in the test's log directory.

use std::path::PathBuf;
use std::sync::Arc;

use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use open_ferry_core::config::save::{backup_path, sha256_hex, write_as_is};
use open_ferry_management::FileConfigWriter;

use super::{Dash, KEY, request};
use crate::router_from;

const UNDO: &str = "/open-ferry/api/v1/config/undo";

/// A config at `port`, with the test's management key.
fn config_at(port: u16) -> String {
    format!("config-version: 8\nserver:\n  port: {port}\nmanagement:\n  secret-key: {KEY}\n")
}

/// `dash`, given a config writer over `config.yaml` in its log directory,
/// and that path.
fn writing(dash: &mut Dash) -> PathBuf {
    let path = dash.logs().join("config.yaml");
    dash.state.management = dash
        .state
        .management
        .clone()
        .with_config_writer(Arc::new(FileConfigWriter::new(path.clone())));
    dash.router = router_from(dash.state.clone());
    path
}

/// Not upstream's: undo puts the backup in place of the file, keeps the
/// file as the backup, and makes the backup's config the one the server
/// uses; without a backup, or a writer, nothing changes.
#[tokio::test]
async fn undo_swaps_the_file_and_its_backup() {
    let mut dash = Dash::new();
    dash.call(Method::POST, UNDO, "")
        .await
        .error(StatusCode::SERVICE_UNAVAILABLE, "config_writer_unavailable");

    let current = config_at(8318);
    let before = config_at(8319);
    let path = writing(&mut dash);
    std::fs::write(&path, &current).unwrap();
    dash.call(Method::POST, UNDO, "")
        .await
        .error(StatusCode::CONFLICT, "no_backup");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), current);

    // Two writes through the writer leave a backup and a record of what
    // was written last, so the undo isn't asked to be forced.
    write_as_is(&path, before.as_bytes()).unwrap();
    write_as_is(&path, current.as_bytes()).unwrap();
    dash.send(request("127.0.0.1:1", Method::POST, UNDO, ""))
        .await
        .error(StatusCode::UNAUTHORIZED, "missing_management_key");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), current);

    let answer = dash.call(Method::POST, UNDO, "").await;
    assert_eq!(
        answer.json(StatusCode::OK),
        serde_json::json!({"status": "ok"})
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(backup_path(&path)).unwrap(),
        current
    );
    assert_eq!(dash.state.management.config().port, 8319);
    assert!(*dash.state.management.config() == Config::load(&path).unwrap());

    dash.call(Method::POST, UNDO, "").await.json(StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
    assert_eq!(dash.state.management.config().port, 8318);

    dash.call(Method::GET, UNDO, "")
        .await
        .error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
}

/// Not upstream's: an undo after the file was changed by hand is refused
/// with `changed_since` unless forced; one whose file or backup isn't the
/// one the body names is refused with `config_changed`; a refused undo
/// changes nothing.
#[tokio::test]
async fn undo_refuses_a_file_changed_since() {
    let mut dash = Dash::new();
    let path = writing(&mut dash);
    let before = config_at(8319);
    let written = config_at(8318);
    let edited = config_at(8320);
    write_as_is(&path, before.as_bytes()).unwrap();
    write_as_is(&path, written.as_bytes()).unwrap();
    std::fs::write(&path, &edited).unwrap();

    let message = dash
        .call(Method::POST, UNDO, "")
        .await
        .error(StatusCode::CONFLICT, "changed_since");
    assert!(message.contains("force"), "{message}");
    dash.call(Method::POST, UNDO, r#"{"force": false}"#)
        .await
        .error(StatusCode::CONFLICT, "changed_since");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);

    // The file or the backup isn't the one named, forced or not.
    let wrong = sha256_hex(written.as_bytes());
    for body in [
        format!(r#"{{"config_sha256": "{wrong}", "force": true}}"#),
        format!(r#"{{"backup_sha256": "{wrong}", "force": true}}"#),
        format!(r#"{{"config_sha256": "{wrong}"}}"#),
    ] {
        dash.call(Method::POST, UNDO, &body)
            .await
            .error(StatusCode::CONFLICT, "config_changed");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);

    // Forced with the right SHA-256s, it goes ahead.
    let body = serde_json::json!({
        "config_sha256": sha256_hex(edited.as_bytes()),
        "backup_sha256": sha256_hex(before.as_bytes()).to_ascii_uppercase(),
        "force": true,
    });
    dash.call(Method::POST, UNDO, &body.to_string())
        .await
        .json(StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), edited);
    assert_eq!(dash.state.management.config().port, 8319);
}

/// Not upstream's: a body that isn't a JSON object of the known fields,
/// or names a SHA-256 that isn't one, is a `400` and changes nothing.
#[tokio::test]
async fn undo_checks_its_body() {
    let mut dash = Dash::new();
    let path = writing(&mut dash);
    let before = config_at(8319);
    let current = config_at(8318);
    write_as_is(&path, before.as_bytes()).unwrap();
    write_as_is(&path, current.as_bytes()).unwrap();
    for body in [
        r#"{"config_sha256": "abc"}"#,
        r#"{"backup_sha256": "not hex, but sixty-four characters long, which is what it needs"}"#,
        r#"{"force": true, "other": 1}"#,
        r#"{"force": "yes"}"#,
    ] {
        dash.call(Method::POST, UNDO, body)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    for body in ["[]", "not json"] {
        dash.call(Method::POST, UNDO, body)
            .await
            .error(StatusCode::BAD_REQUEST, "invalid_json");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), current);

    // Blank is the same as no body.
    dash.call(Method::POST, UNDO, " \n")
        .await
        .json(StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}
