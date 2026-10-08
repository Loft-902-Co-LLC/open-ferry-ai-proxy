//! `POST /config/undo`, over a config file in the test's log directory.

use std::sync::Arc;

use http::{Method, StatusCode};
use open_ferry_core::config::Config;
use open_ferry_core::config::save::backup_path;
use open_ferry_management::FileConfigWriter;

use super::{Dash, KEY, request};
use crate::router_from;

const UNDO: &str = "/open-ferry/api/v1/config/undo";

/// Not upstream's: undo puts the backup in place of the file, keeps the
/// file as the backup, and makes the backup's config the one the server
/// uses; without a backup, or a writer, nothing changes.
#[tokio::test]
async fn undo_swaps_the_file_and_its_backup() {
    let mut dash = Dash::new();
    dash.call(Method::POST, UNDO, "")
        .await
        .error(StatusCode::SERVICE_UNAVAILABLE, "config_writer_unavailable");

    let path = dash.logs().join("config.yaml");
    let current =
        format!("config-version: 8\nserver:\n  port: 8318\nmanagement:\n  secret-key: {KEY}\n");
    let before =
        format!("config-version: 8\nserver:\n  port: 8319\nmanagement:\n  secret-key: {KEY}\n");
    std::fs::write(&path, &current).unwrap();
    dash.state.management = dash
        .state
        .management
        .clone()
        .with_config_writer(Arc::new(FileConfigWriter::new(path.clone())));
    dash.router = router_from(dash.state.clone());
    dash.call(Method::POST, UNDO, "")
        .await
        .error(StatusCode::CONFLICT, "no_backup");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), current);

    std::fs::write(backup_path(&path), &before).unwrap();
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
