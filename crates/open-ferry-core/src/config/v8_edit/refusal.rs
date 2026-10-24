//! Safe public reasons for a typed writer refusal.

/// Keeps the generator's reasons, whose paths contain only schema keys and
/// numeric indexes. Other writer messages can contain user-chosen YAML keys
/// (including credentials), so they get a fixed reason. This only sanitizes
/// text: callers must classify errors by `SaveErrorKind::Unwritable`.
pub fn safe_unwritable_message(message: &str) -> String {
    const REASONS: [&str; 2] = [
        ": a timestamp with a zone 24 hours or more from UTC can't be written",
        ": a mapping with a key that isn't a string can't be written",
    ];
    for reason in REASONS {
        if let Some(path) = message.strip_suffix(reason)
            && let Some(path) = path.strip_prefix("payload.")
            && let Some((name, index)) = path.split_once('[')
            && [
                "default",
                "default-raw",
                "override",
                "override-raw",
                "filter",
            ]
            .contains(&name)
            && let Some(index) = index.strip_suffix(']')
            && !index.is_empty()
            && index.bytes().all(|byte| byte.is_ascii_digit())
        {
            return message.to_owned();
        }
    }
    "the config writer can't preserve the config's values and comments; write refused, file unchanged"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::save::{SaveError, SaveErrorKind};
    use crate::config::v8_edit::{V8EditError, rendered_error, stored_error, write_error};

    const REASON: &str =
        "payload.default[0]: a mapping with a key that isn't a string can't be written";

    #[test]
    fn refusal_mappings_use_kind_and_keep_safe_reasons() {
        for map in [stored_error, write_error, rendered_error] {
            let error = map(SaveError::new(SaveErrorKind::Unwritable, REASON));
            assert_eq!(error, V8EditError::Unwritable(REASON.into()));
            assert_eq!(error.to_string(), format!("config_unwritable: {REASON}"));
        }
        assert!(matches!(
            stored_error(SaveError::new(SaveErrorKind::Yaml, "invalid")),
            V8EditError::StoredInvalid(_)
        ));
        for message in ["write refused", REASON] {
            assert!(matches!(
                write_error(SaveError::new(SaveErrorKind::Io, message)),
                V8EditError::WriteFailed(_)
            ));
            assert!(matches!(
                stored_error(SaveError::new(SaveErrorKind::Io, message)),
                V8EditError::StoredInvalid(_)
            ));
        }
    }

    #[test]
    fn refusal_messages_never_forward_user_keys_or_values() {
        assert_eq!(safe_unwritable_message(REASON), REASON);
        for message in [
            "config scalar at line 3, path $.credential-canary, is read differently; write refused, file unchanged",
            "payload.credential-canary[0]: a mapping with a key that isn't a string can't be written",
            "payload.default[credential-canary]: a mapping with a key that isn't a string can't be written",
            "credential-canary",
        ] {
            let public = safe_unwritable_message(message);
            assert!(!public.contains("credential-canary"));
            assert!(public.contains("values and comments"));
            let error = stored_error(SaveError::new(SaveErrorKind::Unwritable, message));
            assert!(!error.to_string().contains("credential-canary"));
        }
    }
}
