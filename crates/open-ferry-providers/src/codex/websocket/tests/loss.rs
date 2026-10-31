// Transport-loss regressions for upstream f73e66ac.
use super::*;
use open_ferry_core::exec::WebsocketInput;

#[tokio::test]
async fn failed_write_reports_loss_once_without_replay() {
    let conn = Conn::detached(Target::new(
        "auth",
        "ws://example.test/responses",
        "",
        "token",
    ));
    let (_tx, rx) = mpsc::channel(1);
    let input = WebsocketInput::new(rx);
    conn.bind_input(Some(input.clone()));
    let error = conn
        .send_turn("{}".into(), Some(&input), br#"{"event_id":"write"}"#)
        .await
        .unwrap_err();
    assert!(error.stops_replay());
    assert!(error.ws_close.is_none());
    assert!(error.message.contains("upstream_websocket_lost"));
    assert!(error.message.contains("write"));
    assert_eq!(input.epoch(), 1);
    conn.lose_input();
    assert_eq!(
        input.epoch(),
        1,
        "read and write failure share one loss epoch"
    );
}

#[tokio::test]
async fn loss_while_waiting_for_writer_is_not_sent() {
    let conn = Conn::detached(Target::new(
        "auth",
        "ws://example.test/responses",
        "",
        "token",
    ));
    let (_tx, rx) = mpsc::channel(1);
    let input = WebsocketInput::new(rx);
    let turn = input.clone().for_turn();
    let permit = conn.lock_send().await;
    let sending = {
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.send_turn(
                "{}".into(),
                Some(&turn),
                br#"{"type":"response.create","input":[],"event_id":"queued"}"#,
            )
            .await
        })
    };
    input.lose();
    drop(permit);
    let error = sending.await.unwrap().unwrap_err();
    assert!(error.message.contains("upstream_websocket_not_sent"));
    assert!(error.message.contains("queued"));
}

#[test]
fn only_concrete_transport_loss_recovers_a_turn() {
    assert!(errors::is_turn_loss(&Failure::eof()));
    assert!(errors::is_turn_loss(&Failure::closed()));
    assert!(!errors::is_turn_loss(&Failure::timeout()));
    assert!(!errors::is_turn_loss(&Failure::binary()));
    for code in [1000, 1001, 1008, 1009, 1011, 1012, 1013] {
        assert!(
            !errors::is_turn_loss(&Failure::Close {
                code,
                reason: String::new()
            }),
            "{code}"
        );
    }
}

#[tokio::test]
async fn late_input_binding_does_not_advance_an_already_reported_loss() {
    let conn = Conn::detached(Target::new(
        "auth",
        "ws://example.test/responses",
        "",
        "token",
    ));
    let (_tx, rx) = mpsc::channel(1);
    let input = WebsocketInput::new(rx);
    conn.bind_input(Some(input.clone()));
    conn.lose_input();
    assert_eq!(input.epoch(), 1);
    // An open/reuse path can have passed its epoch check just before the
    // reader reports loss, then publish the same input before noticing EOF.
    conn.bind_input(Some(input.clone()));
    conn.lose_input();
    assert_eq!(
        input.epoch(),
        1,
        "one transport loss advances one shared epoch"
    );
}
