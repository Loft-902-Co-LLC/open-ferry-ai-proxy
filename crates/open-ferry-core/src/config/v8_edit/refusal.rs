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
pub(super) mod tests {
    use super::*;
    use crate::config::save::{SaveError, SaveErrorKind};
    use crate::config::v8_edit::{V8EditError, rendered_error, stored_error, write_error};

    use std::cell::RefCell;

    thread_local! {
        static REFUSAL: RefCell<Option<(&'static str, SaveError)>> = const { RefCell::new(None) };
    }

    pub(crate) fn take_refusal(site: &str) -> Option<SaveError> {
        REFUSAL.with_borrow_mut(|slot| {
            if slot.as_ref().is_some_and(|(expected, _)| *expected == site) {
                slot.take().map(|(_, error)| error)
            } else {
                None
            }
        })
    }

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REFUSAL.with_borrow_mut(|slot| *slot = None);
        }
    }

    // These operations don't use the generator. After the loader removes
    // its Unicode guard they have no real Unwritable producer, so inject a
    // generator-produced error at the operation boundary, before its mapper.
    fn assert_real_refusal_at(site: &'static str) {
        use crate::config::Config;
        use crate::config::v8_edit::{V8Edit, V8Method, edit_v8_if, preview_v8};
        let config = Config::load_bytes(
            b"payload:\n  default:\n  - params:\n      field: {1: placeholder}\n",
        )
        .unwrap();
        let error = crate::config::save::render_preserving_comments(b"port: 1\n", &config, false)
            .unwrap_err();
        assert_eq!(error.kind(), SaveErrorKind::Unwritable);
        assert_eq!(error.to_string(), REASON);
        let dir = crate::config::testing::TempDir::new();
        let path = dir.join("config.yaml");
        let before = b"port: 1\n";
        std::fs::write(&path, before).unwrap();
        let edit = V8Edit {
            method: V8Method::Patch,
            path: vec!["server".into(), "port".into()],
            body: b"2".to_vec(),
            yaml: false,
        };
        REFUSAL.with_borrow_mut(|slot| {
            assert!(slot.is_none());
            *slot = Some((site, error));
        });
        let _reset = Reset;
        let error = if site == "preview" {
            preview_v8(before, &edit).unwrap_err()
        } else {
            edit_v8_if(&path, &edit, None).unwrap_err()
        };
        assert_eq!(error, V8EditError::Unwritable(REASON.into()));
        assert_eq!(error.to_string(), format!("config_unwritable: {REASON}"));
        assert!(
            REFUSAL.with_borrow(|slot| slot.is_none()),
            "mapping boundary was not reached"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!crate::config::save::backup_path(&path).exists());
    }

    #[test]
    fn real_refusal_at_lock_mapping() {
        assert_real_refusal_at("lock");
    }
    #[test]
    fn real_refusal_at_write_mapping() {
        assert_real_refusal_at("write");
    }
    #[test]
    fn real_refusal_at_preview_mapping() {
        assert_real_refusal_at("preview");
    }
    #[test]
    fn real_refusal_at_stored_mapping() {
        assert_real_refusal_at("stored");
    }
    #[test]
    fn real_refusal_at_rendered_mapping() {
        assert_real_refusal_at("rendered");
    }

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
