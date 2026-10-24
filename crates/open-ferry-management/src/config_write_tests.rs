use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn state(path: &Path, writer: Arc<dyn ConfigWriter>) -> ManagementState {
    let registry = Arc::new(open_ferry_core::registry::ModelRegistry::new());
    let manager = open_ferry_core::manager::Manager::new(
        open_ferry_core::manager::Settings::default(),
        registry.clone(),
        None,
    );
    ManagementState::new(Arc::new(Config::default()), manager, registry, None)
        .with_config_path(path.to_owned())
        .with_config_writer(writer)
}

struct NeverCalled;

#[tokio::test]
async fn unwritable_responses_never_expose_arbitrary_writer_paths() {
    let error = WriteError::unwritable("credential-canary");
    assert!(error.is_unwritable());
    assert!(!error.to_string().contains("credential-canary"));
    let response = v8_error_response(&V8EditError::Unwritable("credential-canary".into()));
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "config_unwritable");
    assert!(
        !body["message"]
            .as_str()
            .unwrap()
            .contains("credential-canary")
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("values and comments")
    );
}

impl ConfigWriter for NeverCalled {
    fn save_preserving_comments(
        &self,
        _: &Config,
        _: bool,
        _: Option<&str>,
    ) -> Result<Option<String>, WriteError> {
        panic!("refused writer must not read or write");
    }
    fn write_file(&self, _: &[u8]) -> Result<Option<String>, WriteError> {
        panic!("refused writer must not read or write");
    }
    fn edit_v8(
        &self,
        _: &V8Edit,
        _: Option<&V8Precondition>,
    ) -> Result<(Config, Option<String>), V8EditError> {
        panic!("refused writer must not read or write");
    }
    fn undo(&self, _: &UndoCheck) -> Result<(Config, Option<String>), UndoError> {
        panic!("refused writer must not read or write");
    }
}

fn edit() -> V8Edit {
    V8Edit {
        method: V8Method::Patch,
        path: vec!["server".into(), "port".into()],
        body: b"1234".to_vec(),
        yaml: false,
    }
}

fn refuse_all(writer: &dyn ConfigWriter) {
    let expected = "config file authorization refused";
    assert_eq!(
        writer
            .save_preserving_comments(&Config::default(), false, None)
            .unwrap_err()
            .to_string(),
        expected
    );
    assert_eq!(
        writer.write_file(b"port: 1234\n").unwrap_err().to_string(),
        expected
    );
    assert_eq!(
        writer.edit_v8(&edit(), None).unwrap_err(),
        V8EditError::WriteFailed(expected.into())
    );
    assert_eq!(
        writer.undo(&UndoCheck::default()).unwrap_err(),
        UndoError::Failed(expected.into())
    );
}

#[tokio::test]
async fn refused_writes_never_call_the_writer_and_log_once_without_content() {
    let state = state(Path::new("config.yaml"), Arc::new(NeverCalled))
        .with_config_read_check(|_| Err("test refusal".into()));
    let writer = state.config_writer().unwrap();
    let (logs, capturing) = crate::tests::capture::Logs::capture();
    refuse_all(writer.as_ref());
    drop(capturing);
    assert_eq!(logs.levels(), [tracing::Level::WARN; 4]);
    let logs = logs.text();
    assert_eq!(logs.lines().count(), 4, "{logs}");
    for line in logs.lines() {
        assert!(
            line.starts_with("management config file not written reason=")
                && line.contains("test refusal")
                && !line.contains("1234"),
            "{line}"
        );
    }
    let error = writer.edit_v8(&edit(), None).unwrap_err();
    let response = v8_error_response(&error);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        body.as_ref(),
        br#"{"error":"write_failed","message":"config file authorization refused"}"#
    );
    let response = update(&state, false, |_| Ok(())).await;
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        body.as_ref(),
        br#"{"error":"failed to save config: config file authorization refused"}"#
    );
    assert_eq!(
        undo_config(&state, UndoCheck::default()).await.unwrap_err(),
        UndoError::Failed("config file authorization refused".into())
    );
}

#[test]
fn missing_configs_require_an_authorized_existing_parent() {
    let state = state(Path::new("untrusted/config.yaml"), Arc::new(NeverCalled))
        .with_config_read_check(|_| Ok(false));
    refuse_all(state.config_writer().unwrap().as_ref());
    let state = state.with_config_directory_check(|path| {
        assert_eq!(path, Path::new("untrusted"));
        Err("untrusted parent".into())
    });
    refuse_all(state.config_writer().unwrap().as_ref());
    let state = state.with_config_read_check(|_| Ok(true));
    refuse_all(state.config_writer().unwrap().as_ref());
}

#[test]
fn backup_and_other_sibling_reads_are_authorized_before_the_writer() {
    for suffix in [".bak", ".lock", ".sha256"] {
        let state = state(Path::new("config.yaml"), Arc::new(NeverCalled)).with_config_read_check(
            move |path| {
                if path == Path::new(&format!("config.yaml{suffix}")) {
                    Err("untrusted sibling".into())
                } else {
                    Ok(true)
                }
            },
        );
        refuse_all(state.config_writer().unwrap().as_ref());
    }
}

#[test]
fn allowed_writes_and_missing_files_under_trusted_parents_work_as_before() {
    let dir = crate::tests::scratch_dir();
    let path = dir.path().join("config.yaml");
    let parent = dir.path().to_owned();
    let calls = Arc::new(AtomicUsize::new(0));
    let checked = calls.clone();
    let state = state(&path, Arc::new(FileConfigWriter::new(&path)))
        .with_config_read_check(move |path| {
            checked.fetch_add(1, Ordering::SeqCst);
            Ok(path.exists())
        })
        .with_config_directory_check(move |path| {
            assert_eq!(path, parent);
            Ok(true)
        });
    let writer = state.config_writer().unwrap();
    writer.write_file(b"port: 1233\n").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"port: 1233\n");
    writer.edit_v8(&edit(), None).unwrap();
    assert_eq!(Config::load(&path).unwrap().port, 1234);
    writer.undo(&UndoCheck::default()).unwrap();
    assert_eq!(Config::load(&path).unwrap().port, 1233);
    let mut config = Config::load(&path).unwrap();
    config.port = 1235;
    writer
        .save_preserving_comments(&config, false, None)
        .unwrap();
    assert_eq!(Config::load(&path).unwrap().port, 1235);
    assert_eq!(calls.load(Ordering::SeqCst), 16);
}

thread_local! {
    static RAW_WRITE_REFUSAL: std::cell::RefCell<Option<save::SaveError>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn write_file_bytes(path: &Path, data: &[u8]) -> Result<String, save::SaveError> {
    match RAW_WRITE_REFUSAL.with_borrow_mut(Option::take) {
        Some(error) => Err(error),
        None => save::write_file_with_sha256(path, data),
    }
}

#[tokio::test]
async fn raw_write_maps_a_producer_refusal_to_the_exact_conflict_code() {
    let dir = crate::tests::scratch_dir();
    let path = dir.path().join("config.yaml");
    let before = b"port: 1\n";
    std::fs::write(&path, before).unwrap();
    let config =
        Config::load_bytes(b"payload:\n  default:\n  - params:\n      field: {1: placeholder}\n")
            .unwrap();
    let refusal = save::save_preserving_comments(&path, &config, false).unwrap_err();
    assert_eq!(refusal.kind(), SaveErrorKind::Unwritable);
    let reason = refusal.to_string();
    RAW_WRITE_REFUSAL.with_borrow_mut(|slot| *slot = Some(refusal));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RAW_WRITE_REFUSAL.with_borrow_mut(|slot| *slot = None);
        }
    }
    let _reset = Reset;
    let error = FileConfigWriter::new(&path)
        .write_file(b"port: 2\n")
        .unwrap_err();
    assert_eq!(error, WriteError::unwritable(&reason));
    assert!(error.is_unwritable());
    assert!(RAW_WRITE_REFUSAL.with_borrow(|slot| slot.is_none()));
    let response = unwritable_response(&error.to_string());
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error":"config_unwritable", "message":reason})
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!save::backup_path(&path).exists());
}
