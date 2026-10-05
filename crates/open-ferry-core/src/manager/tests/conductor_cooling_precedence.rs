// Ported from CLIProxyAPI sdk/cliproxy/auth/conductor_cooling_precedence_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Whether a failure cools a credential down: the credential's own
//! `disable_cooling` wins, then its OpenAI-compatible provider's, then the
//! global setting.
//!
//! Deviations from upstream:
//! - The case "home mode disables local cooling despite credential false" is
//!   dropped: the Home dispatcher is not ported.

use super::support::*;

use crate::auth::{AuthError, Status};
use crate::manager::{CallResult, OpenAiCompat, Settings};

struct Case {
    name: &'static str,
    global_disable: bool,
    credential: Option<bool>,
    provider_override: Option<bool>,
    want_cooldown: bool,
}

#[tokio::test(start_paused = true)]
async fn manager_mark_result_uses_credential_cooling_precedence() {
    let cases = [
        Case {
            name: "credential true overrides global false",
            global_disable: false,
            credential: Some(true),
            provider_override: None,
            want_cooldown: false,
        },
        Case {
            name: "credential false overrides global true",
            global_disable: true,
            credential: Some(false),
            provider_override: None,
            want_cooldown: true,
        },
        Case {
            name: "unset inherits global true",
            global_disable: true,
            credential: None,
            provider_override: None,
            want_cooldown: false,
        },
        Case {
            name: "unset inherits global false",
            global_disable: false,
            credential: None,
            provider_override: None,
            want_cooldown: true,
        },
        Case {
            name: "provider false overrides global true",
            global_disable: true,
            credential: None,
            provider_override: Some(false),
            want_cooldown: true,
        },
        Case {
            name: "provider true overrides global false",
            global_disable: false,
            credential: None,
            provider_override: Some(true),
            want_cooldown: false,
        },
        Case {
            name: "credential false overrides provider true",
            global_disable: false,
            credential: Some(false),
            provider_override: Some(true),
            want_cooldown: true,
        },
    ];

    for tc in cases {
        let mut settings = Settings {
            disable_cooling: tc.global_disable,
            ..Settings::default()
        };
        let mut a = auth(tc.name, "claude");
        a.status = Status::Active;
        if let Some(credential) = tc.credential {
            a.metadata.insert(
                "disable_cooling".into(),
                serde_json::Value::Bool(credential),
            );
        }
        if let Some(provider_override) = tc.provider_override {
            a.provider = "openai-compatibility".into();
            a.attributes.insert("provider_key".into(), "compat".into());
            a.attributes.insert("compat_name".into(), "compat".into());
            settings.openai_compatibility = vec![OpenAiCompat {
                name: "compat".into(),
                disable_cooling: Some(provider_override),
                ..OpenAiCompat::default()
            }];
        }
        let provider = a.provider.clone();
        let h = Harness::new(settings);
        h.add(a, &[]);

        let model = "test-model";
        h.manager.mark_result(&CallResult {
            auth_id: tc.name.into(),
            provider,
            model: model.into(),
            error: Some(AuthError {
                http_status: 500,
                message: "upstream failed".into(),
                ..AuthError::default()
            }),
            ..CallResult::default()
        });

        let updated = h.get(tc.name);
        let state = updated
            .model_states
            .get(model)
            .unwrap_or_else(|| panic!("{}: updated model state missing", tc.name));
        let got_cooldown = state.next_retry_after.is_some();
        assert_eq!(
            got_cooldown, tc.want_cooldown,
            "{}: cooldown present = {got_cooldown}, want {}",
            tc.name, tc.want_cooldown
        );
    }
}
