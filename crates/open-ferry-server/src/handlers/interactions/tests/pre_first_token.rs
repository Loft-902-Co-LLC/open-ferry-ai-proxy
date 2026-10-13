// Ported from CLIProxyAPI sdk/api/handlers/gemini/interactions_handlers_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use crate::stream::tests::pre_first_token::check_protocol;

#[tokio::test(start_paused = true)]
async fn pre_first_token_interactions_failure_is_an_sse_error_after_keepalive() {
    check_protocol(
        "/v1beta/interactions",
        r#"{"model":"m","input":"hi","stream":true}"#,
        "gemini-interactions",
        "m",
        "event: error\ndata: ",
        false,
    )
    .await;
}
