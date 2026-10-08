//! Tests ported from upstream, and tests of the router.
//!
//! Each module under `tests/` named after an upstream test file ports it
//! and names the tests it drops or changes; `router` tests what no upstream
//! test covers. The modules named after a module of this crate
//! (`credential_files`, `vertex_import`, `credential_state`, `oauth`,
//! `config_read`, `model_definitions` and `latest_version`) hold the tests
//! of that module's routes, each saying which upstream test files it
//! ports. Every test drives the real router, from a client at 127.0.0.1
//! unless it says otherwise, and every upstream a call reaches is a server
//! on a 127.0.0.1 ephemeral port.
//!
//! The routes that write credentials are tested over an [`AuthDir`]: a
//! temporary auth directory, with the store the manager saves to, and a
//! [`FakeSync`] standing in for the service. The routes that write the
//! config are tested with [`Api::writing`]: a [`FakeWriter`] records what
//! they ask to write, and a [`FakeReload`] counts the reloads they ask
//! for; or, over a config file in a temporary directory, with
//! [`Api::over_config_file`]: the [`FileConfigWriter`] the service uses
//! writes the file, and a [`FileReload`] loads it again as the service
//! does. [`Multipart`] builds the bodies of uploads.

mod api_key_usage;
mod api_tools;
mod auth_files_cooldown;
mod auth_files_filter;
mod auth_files_pagination;
mod auth_files_project_id;
mod auth_files_quota;
mod auth_files_quota_checks;
mod auth_files_recent_requests;
mod auth_files_relogin_preserve;
mod config_file_write;
mod config_keys;
mod config_lists;
mod config_read;
mod config_settings;
mod config_v8_write;
mod config_write;
mod credential_files;
mod credential_state;
mod handler;
mod latest_version;
mod logs;
mod model_definitions;
mod oauth;
mod observability_settings;
mod plugin_quota;
mod quota;
mod request_logs;
mod router;
mod server_management_v8;
mod usage;
mod vertex_import;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use chrono::{DateTime, Utc};
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use open_ferry_core::auth::synthesizer::SynthesisContext;
use open_ferry_core::auth::synthesizer::file::synthesize_auth_file;
use open_ferry_core::auth::{Auth, AuthStore, FileStore, Status, Timestamp};
use open_ferry_core::config::{AuthFile, Config};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::registry::ModelRegistry;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt as _;

use crate::{
    ConfigReload, ConfigWriter, CredentialSync, FileConfigWriter, ManagementState, ReloadFuture,
    SyncError, SyncFuture, V8Edit, V8EditError, WriteError, router,
};

/// The management key the tests set.
const KEY: &str = "test-secret";

/// Where requests come from unless a test says otherwise.
const LOCAL: &str = "127.0.0.1:12345";

/// The management API over a manager and a model registry of its own.
struct Api {
    manager: Manager,
    registry: Arc<ModelRegistry>,
    state: ManagementState,
    router: Router,
    /// What stands in for the service; the state uses it only when made
    /// over an [`AuthDir`].
    sync: Arc<FakeSync>,
    /// What saves the config; the state uses it only when made by
    /// [`Api::with_writer`].
    writer: Arc<FakeWriter>,
    /// What reloads the service; the state uses it only when made by
    /// [`Api::with_writer`].
    reload: Arc<FakeReload>,
}

impl Api {
    /// The API with management key [`KEY`] in the config.
    fn new() -> Self {
        Self::with(keyed_config(), None)
    }

    /// The API with `config` and, if given, `MANAGEMENT_PASSWORD`.
    fn with(config: Config, password: Option<&str>) -> Self {
        Self::with_store(config, password, None)
    }

    /// The API with `config`, `MANAGEMENT_PASSWORD` if given, and a manager
    /// saving to `store` if given.
    fn with_store(
        config: Config,
        password: Option<&str>,
        store: Option<Arc<dyn AuthStore>>,
    ) -> Self {
        Self::build(config, password, store, |state, _| state)
    }

    /// The API with management key [`KEY`], keeping credentials in
    /// `auth_dir`: its manager saves there, and the state has the
    /// directory's store, the [`FakeSync`] and the config path.
    fn over(auth_dir: &AuthDir) -> Self {
        Self::over_with(auth_dir, auth_dir.config(), None)
    }

    /// [`Api::over`], with `config` and, if given, `MANAGEMENT_PASSWORD`.
    fn over_with(auth_dir: &AuthDir, config: Config, password: Option<&str>) -> Self {
        let store = Arc::clone(&auth_dir.store);
        Self::build(config, password, Some(store as _), |state, sync| {
            state
                .with_store(Arc::clone(&auth_dir.store))
                .with_sync(sync)
                .with_config_path(auth_dir.config_path())
        })
    }

    /// The API with `config`, saving it through the [`FakeWriter`] and
    /// reloading through the [`FakeReload`] (see [`Api::with_writer`]).
    fn writing(config: Config) -> Self {
        Self::with(config, None).with_writer()
    }

    /// The API over the config file of `dir`, written with `raw` first:
    /// with the config the file loads as, key [`KEY`] as
    /// `MANAGEMENT_PASSWORD` (the file need not hold one), the directory's
    /// store, the [`FakeSync`] and the config path; saving the file with a
    /// [`FileConfigWriter`] and reloading it with a [`FileReload`].
    fn over_config_file(dir: &AuthDir, raw: &str) -> Self {
        let path = dir.config_path();
        std::fs::write(&path, raw).unwrap();
        let config = Config::load(&path).unwrap();
        let mut api = Self::over_with(dir, config, Some(KEY));
        let reload = Arc::new(FileReload::new(path.clone()));
        let state = api
            .state
            .clone()
            .with_config_writer(Arc::new(FileConfigWriter::new(path)))
            .with_config_reload(Arc::clone(&reload) as _);
        reload.watch(state.clone());
        api.router = router(state.clone());
        api.state = state;
        api
    }

    /// This API, saving the config through the [`FakeWriter`] and reloading
    /// through the [`FakeReload`]. The writer watches the state's config
    /// write lock.
    fn with_writer(mut self) -> Self {
        let state = self
            .state
            .clone()
            .with_config_writer(Arc::clone(&self.writer) as _)
            .with_config_reload(Arc::clone(&self.reload) as _);
        self.writer.watch_lock(state.clone());
        self.router = router(state.clone());
        self.state = state;
        self
    }

    /// The API, its state made by `configure` from the plain state and the
    /// [`FakeSync`].
    fn build(
        config: Config,
        password: Option<&str>,
        store: Option<Arc<dyn AuthStore>>,
        configure: impl FnOnce(ManagementState, Arc<dyn CredentialSync>) -> ManagementState,
    ) -> Self {
        let registry = Arc::new(ModelRegistry::new());
        let manager = Manager::new(Settings::default(), Arc::clone(&registry) as _, store);
        let sync = Arc::new(FakeSync::new(manager.clone()));
        let state = ManagementState::new(
            Arc::new(config),
            manager.clone(),
            Arc::clone(&registry),
            password.map(OsString::from),
        );
        let state = configure(state, Arc::clone(&sync) as _);
        Self {
            router: router(state.clone()),
            manager,
            registry,
            state,
            sync,
            writer: Arc::new(FakeWriter::default()),
            reload: Arc::new(FakeReload::default()),
        }
    }

    /// Registers `auth` and returns its index.
    fn register(&self, auth: Auth) -> String {
        self.manager.register(auth).unwrap().index.clone()
    }

    /// The answer to `request`.
    async fn send(&self, request: Request<Body>) -> Answer {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8(body.to_vec()).unwrap(),
        }
    }

    /// `GET path`, with the key.
    async fn get(&self, path: &str) -> Answer {
        self.send(keyed(Method::GET, path, "")).await
    }

    /// `POST path` with `body`, with the key.
    async fn post(&self, path: &str, body: &str) -> Answer {
        self.send(keyed(Method::POST, path, body)).await
    }

    /// `method path` with `body`, with the key.
    async fn call(&self, method: Method, path: &str, body: &str) -> Answer {
        self.send(keyed(method, path, body)).await
    }

    /// The config the handlers read, after checking that it is the one the
    /// [`FakeWriter`] last saved.
    fn saved(&self) -> Config {
        let config = Config::clone(&self.state.config());
        let saved = self.writer.last_saved();
        assert!(saved == config, "{saved:#?}\n!=\n{config:#?}");
        config
    }

    /// The credential list, given `query` (empty, or `?` and a query).
    async fn list(&self, query: &str) -> Value {
        self.get(&format!("/v0/management/auth-files{query}"))
            .await
            .expect(StatusCode::OK)
    }

    /// The answer to an `api-call` with `body`.
    async fn api_call(&self, body: &Value) -> Answer {
        self.post("/v0/management/api-call", &body.to_string())
            .await
    }

    /// The listed credentials, given `query`.
    async fn files(&self, query: &str) -> Vec<Value> {
        match self.list(query).await["files"].take() {
            Value::Array(files) => files,
            other => panic!("files isn't an array: {other}"),
        }
    }
}

/// A response, read.
#[derive(Debug)]
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    /// The body as JSON, after checking the status.
    fn expect(&self, status: StatusCode) -> Value {
        assert_eq!(self.status, status, "{}", self.body);
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// Checks the status and that the body is exactly `body`.
    fn assert(&self, status: StatusCode, body: &str) {
        assert_eq!((self.status, self.body.as_str()), (status, body));
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }
}

/// The default config, with management key [`KEY`].
fn keyed_config() -> Config {
    let mut config = Config::default();
    config.remote_management.secret_key = KEY.into();
    config
}

/// A request from `peer`, without a key.
fn request_from(peer: &str, method: Method, path: &str, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    request
}

/// A request from [`LOCAL`] with the key as a bearer token.
fn keyed(method: Method, path: &str, body: &str) -> Request<Body> {
    let mut request = request_from(LOCAL, method, path, body);
    let value = format!("Bearer {KEY}").parse().unwrap();
    request.headers_mut().insert(header::AUTHORIZATION, value);
    request
}

/// A temporary directory holding an auth directory, `auths`, and where a
/// config file would be, `config.yaml` (not written).
struct AuthDir {
    dir: tempfile::TempDir,
    /// The store over the auth directory.
    store: Arc<FileStore>,
}

impl AuthDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let auths = dir.path().join("auths");
        std::fs::create_dir(&auths).unwrap();
        Self {
            store: Arc::new(FileStore::new(&auths)),
            dir,
        }
    }

    /// The auth directory.
    fn path(&self) -> PathBuf {
        self.dir.path().join("auths")
    }

    /// Where the config file would be.
    fn config_path(&self) -> PathBuf {
        self.dir.path().join("config.yaml")
    }

    /// [`keyed_config`], with this auth directory.
    fn config(&self) -> Config {
        let mut config = keyed_config();
        config.auth_dir = self.path().to_str().unwrap().to_owned();
        config
    }

    /// Writes file `name` in the auth directory with `contents`, and
    /// returns its path.
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// File `name` of the auth directory, as JSON.
    fn read_json(&self, name: &str) -> Value {
        let data = std::fs::read(self.path().join(name)).unwrap();
        serde_json::from_slice(&data).unwrap()
    }
}

/// A call [`FakeSync`] took.
#[derive(Clone, Debug)]
enum SyncCall {
    Upsert(Box<Auth>),
    FileWritten(AuthFile),
    FileRemoved(PathBuf),
}

/// A [`CredentialSync`] that stands in for the service: it records each
/// call and applies it to its manager when the call is made, as the
/// service takes its revision then, without the model registry; once
/// stopped, it fails every call as a stopped service does, and records
/// nothing.
struct FakeSync {
    manager: Manager,
    calls: Mutex<Vec<SyncCall>>,
    stopped: AtomicBool,
    /// The state whose credential lock each call checks, once watched.
    watched: OnceLock<ManagementState>,
    /// For each call made while watched: whether the credential lock was
    /// held when the call was made, and when its future was awaited.
    lock_held: Mutex<Vec<(bool, bool)>>,
}

impl FakeSync {
    fn new(manager: Manager) -> Self {
        Self {
            manager,
            calls: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
            watched: OnceLock::new(),
            lock_held: Mutex::new(Vec::new()),
        }
    }

    /// Records, for each later call, whether the credential lock of
    /// `state` was held when the call was made and when it was awaited
    /// (see [`FakeSync::lock_held`]). The state holds this sync, so the two
    /// are never freed: watch only a test's own API.
    fn watch_lock(&self, state: ManagementState) {
        assert!(self.watched.set(state).is_ok(), "already watched");
    }

    /// For each call made while watched, whether the credential lock was
    /// held when it was made, and when it was awaited.
    fn lock_held(&self) -> Vec<(bool, bool)> {
        self.lock_held.lock().unwrap().clone()
    }

    /// Whether the watched credential lock is held now, if one is watched.
    fn lock_held_now(&self) -> Option<bool> {
        self.watched
            .get()
            .map(|state| state.credential_lock().try_lock().is_err())
    }

    /// The calls taken so far.
    fn calls(&self) -> Vec<SyncCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Fails every later call with [`SyncError::Stopped`].
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    fn call(&self, call: SyncCall) -> SyncFuture<'_> {
        let held_at_call = self.lock_held_now();
        let result = if self.stopped.load(Ordering::SeqCst) {
            Err(SyncError::Stopped)
        } else {
            self.apply(&call);
            self.calls.lock().unwrap().push(call);
            Ok(())
        };
        Box::pin(async move {
            if let Some(at_call) = held_at_call {
                let at_await = self.lock_held_now().unwrap_or_default();
                self.lock_held.lock().unwrap().push((at_call, at_await));
            }
            result
        })
    }

    fn apply(&self, call: &SyncCall) {
        match call {
            SyncCall::Upsert(auth) => self.upsert(Auth::clone(auth)),
            SyncCall::FileWritten(file) => {
                let dir = file.path.parent().unwrap_or(Path::new(""));
                let ctx = SynthesisContext::new(dir, Utc::now());
                match synthesize_auth_file(&ctx, &file.path, &file.data) {
                    Ok(Some(auth)) => self.upsert(auth),
                    _ => self.remove_file(&file.path),
                }
            }
            SyncCall::FileRemoved(path) => self.remove_file(path),
        }
    }

    fn upsert(&self, auth: Auth) {
        if self.manager.get(&auth.id).is_some() {
            self.manager.update_unsaved(auth).unwrap();
        } else {
            self.manager.register_unsaved(auth).unwrap();
        }
    }

    /// Removes every credential from the file at `path`.
    fn remove_file(&self, path: &Path) {
        for auth in self.manager.list() {
            if auth.attribute("path").map(Path::new) == Some(path) {
                self.manager.remove(&auth.id);
            }
        }
    }
}

impl CredentialSync for FakeSync {
    fn upsert(&self, auth: Auth) -> SyncFuture<'_> {
        self.call(SyncCall::Upsert(Box::new(auth)))
    }

    fn file_written(&self, file: AuthFile) -> SyncFuture<'_> {
        self.call(SyncCall::FileWritten(file))
    }

    fn file_removed(&self, path: PathBuf) -> SyncFuture<'_> {
        self.call(SyncCall::FileRemoved(path))
    }
}

/// A write [`FakeWriter`] was asked for.
#[derive(Clone, Debug, PartialEq)]
enum Written {
    /// [`ConfigWriter::save_preserving_comments`].
    Saved {
        config: Box<Config>,
        migrate_v8: bool,
    },
    /// [`ConfigWriter::write_file`].
    File(Vec<u8>),
    /// [`ConfigWriter::edit_v8`].
    V8(V8Edit),
}

/// A [`ConfigWriter`] that records what it is asked to write, saves
/// nothing, and refuses every write while set to fail.
#[derive(Default)]
struct FakeWriter {
    written: Mutex<Vec<Written>>,
    /// The error every write gives, once set.
    failure: Mutex<Option<String>>,
    /// What [`ConfigWriter::edit_v8`] gives, once set; else the config
    /// with the management key.
    v8_result: Mutex<Option<Result<Config, V8EditError>>>,
    /// The state whose config write lock each write checks, once watched.
    watched: OnceLock<ManagementState>,
    /// For each write made while watched: whether the lock was held.
    lock_held: Mutex<Vec<bool>>,
}

impl FakeWriter {
    /// The writes asked for so far.
    fn written(&self) -> Vec<Written> {
        self.written.lock().unwrap().clone()
    }

    /// The configs saved so far, with whether each was to migrate.
    fn saved(&self) -> Vec<(Config, bool)> {
        self.written()
            .into_iter()
            .filter_map(|written| match written {
                Written::Saved { config, migrate_v8 } => Some((*config, migrate_v8)),
                _ => None,
            })
            .collect()
    }

    /// The last config saved.
    fn last_saved(&self) -> Config {
        match self.saved().pop() {
            Some((config, _)) => config,
            None => panic!("nothing saved: {:?}", self.written()),
        }
    }

    /// Refuses every later write with `message`.
    fn fail(&self, message: &str) {
        *self.failure.lock().unwrap() = Some(message.to_owned());
    }

    /// Has [`ConfigWriter::edit_v8`] give `result`.
    fn set_v8_result(&self, result: Result<Config, V8EditError>) {
        *self.v8_result.lock().unwrap() = Some(result);
    }

    /// Records, for each later write, whether the config write lock of
    /// `state` was held. The state holds this writer, so the two are never
    /// freed: watch only a test's own API.
    fn watch_lock(&self, state: ManagementState) {
        assert!(self.watched.set(state).is_ok(), "already watched");
    }

    /// For each write made while watched, whether the lock was held.
    fn lock_held(&self) -> Vec<bool> {
        self.lock_held.lock().unwrap().clone()
    }

    /// Records `written`, and gives the error set, if any.
    fn record(&self, written: Written) -> Result<(), WriteError> {
        if let Some(state) = self.watched.get() {
            let held = state.config_write_lock().try_lock().is_err();
            self.lock_held.lock().unwrap().push(held);
        }
        self.written.lock().unwrap().push(written);
        match &*self.failure.lock().unwrap() {
            Some(message) => Err(WriteError::new(message.clone())),
            None => Ok(()),
        }
    }
}

impl ConfigWriter for FakeWriter {
    fn save_preserving_comments(
        &self,
        config: &Config,
        migrate_v8: bool,
    ) -> Result<(), WriteError> {
        self.record(Written::Saved {
            config: Box::new(config.clone()),
            migrate_v8,
        })
    }

    fn write_file(&self, data: &[u8]) -> Result<(), WriteError> {
        self.record(Written::File(data.to_vec()))
    }

    fn edit_v8(&self, edit: &V8Edit) -> Result<Config, V8EditError> {
        self.record(Written::V8(edit.clone()))
            .map_err(|error| V8EditError::WriteFailed(error.to_string()))?;
        match self.v8_result.lock().unwrap().clone() {
            Some(result) => result,
            None => Ok(keyed_config()),
        }
    }
}

/// A [`ConfigReload`] that counts the reloads asked for.
#[derive(Default)]
struct FakeReload {
    count: AtomicUsize,
}

impl FakeReload {
    /// The reloads asked for so far.
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl ConfigReload for FakeReload {
    fn reload(&self) -> ReloadFuture<'_> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// A [`ConfigReload`] that loads the config file again and gives the state
/// the config it holds, as the service does. A file that doesn't load fails
/// the test: every write is checked to load first.
struct FileReload {
    path: PathBuf,
    /// The state given the config, once watched.
    state: OnceLock<ManagementState>,
}

impl FileReload {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: OnceLock::new(),
        }
    }

    /// Gives each config loaded to `state`. The state holds this reload,
    /// so the two are never freed: watch only a test's own API.
    fn watch(&self, state: ManagementState) {
        assert!(self.state.set(state).is_ok(), "already watched");
    }
}

impl ConfigReload for FileReload {
    fn reload(&self) -> ReloadFuture<'_> {
        Box::pin(async {
            let config = Config::load(&self.path).unwrap();
            if let Some(state) = self.state.get() {
                state.set_config(Arc::new(config));
            }
        })
    }
}

/// A `multipart/form-data` body, built a part at a time, as Go's
/// `multipart.Writer` writes one.
struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    fn new() -> Self {
        Self {
            boundary: "open-ferry-test-boundary-6f1c2a".to_owned(),
            body: Vec::new(),
        }
    }

    /// Adds field `name` with `value` (`CreateFormField`).
    fn text(self, name: &str, value: &str) -> Self {
        let disposition = format!("form-data; name=\"{name}\"");
        self.part(&disposition, None, value.as_bytes())
    }

    /// Adds file `file_name` as field `name` (`CreateFormFile`).
    fn file(self, name: &str, file_name: &str, contents: &[u8]) -> Self {
        let disposition = format!("form-data; name=\"{name}\"; filename=\"{file_name}\"");
        self.part(&disposition, Some("application/octet-stream"), contents)
    }

    fn part(mut self, disposition: &str, content_type: Option<&str>, contents: &[u8]) -> Self {
        let mut head = format!(
            "--{}\r\nContent-Disposition: {disposition}\r\n",
            self.boundary
        );
        if let Some(content_type) = content_type {
            let _ = write!(head, "Content-Type: {content_type}\r\n");
        }
        head.push_str("\r\n");
        self.body.extend_from_slice(head.as_bytes());
        self.body.extend_from_slice(contents);
        self.body.extend_from_slice(b"\r\n");
        self
    }

    /// A request from [`LOCAL`] with the key, sending the body.
    fn request(mut self, method: Method, path: &str) -> Request<Body> {
        let end = format!("--{}--\r\n", self.boundary);
        self.body.extend_from_slice(end.as_bytes());
        let mut request = keyed(method, path, "");
        let content_type = format!("multipart/form-data; boundary={}", self.boundary);
        request
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        *request.body_mut() = Body::from(self.body);
        request
    }
}

/// A Codex credential with `id` and `attributes`.
fn auth(id: &str, attributes: &[(&str, &str)]) -> Auth {
    let mut auth = Auth {
        id: id.into(),
        provider: "codex".into(),
        ..Auth::default()
    };
    for (key, value) in attributes {
        auth.attributes
            .insert((*key).to_owned(), (*value).to_owned());
    }
    auth
}

/// A runtime-only Codex credential.
fn runtime_auth(id: &str) -> Auth {
    auth(id, &[("runtime_only", "true")])
}

/// An active Codex credential with `id`, from file `name` in `dir`, which
/// is written with `contents`.
fn file_auth(dir: &Path, id: &str, name: &str, contents: &str) -> Auth {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    let mut auth = auth(id, &[("path", path.to_str().unwrap())]);
    auth.file_name = name.into();
    auth.status = Status::Active;
    auth
}

/// A time as the API writes it.
fn time(value: &Value) -> Timestamp {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("not a time: {value}"));
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

/// The JSON object `value` holds.
fn object(value: &Value) -> &serde_json::Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
}

/// An HTTP/1.1 server on a 127.0.0.1 ephemeral port that stands in for an
/// upstream. It reads one request per connection, answers it and closes the
/// connection.
struct Upstream {
    /// `http://127.0.0.1:<port>`.
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    /// Answers the `n`th request, counting from 0, with `respond(n,
    /// request)`; the request is passed as text.
    async fn start(respond: impl Fn(usize, &str) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let Some(request) = read_request(&mut stream).await else {
                    continue;
                };
                let n = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(request.clone());
                    seen.len() - 1
                };
                let _ = stream.write_all(&respond(n, &request)).await;
                let _ = stream.shutdown().await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    /// Answers every request with `response`.
    async fn answering(response: Vec<u8>) -> Self {
        Self::start(move |_, _| response.clone()).await
    }

    /// The requests read so far.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One request: its head, then the body its `Content-Length` gives.
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 8192];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// An HTTP/1.1 response with `status` (such as `200 OK`), `headers`, and
/// `body` with its length.
fn http_response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    let _ = write!(
        head,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}
