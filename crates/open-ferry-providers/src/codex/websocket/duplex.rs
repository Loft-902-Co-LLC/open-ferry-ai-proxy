// Ported from CLIProxyAPI internal/runtime/executor/codex_websockets_duplex.go
// (streamCodexDuplex, codexDuplexConnectionError) and the handoff in
// codex_websockets_stream.go (ExecuteStream) (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Response steering: one upstream connection for the client's whole
//! Responses WebSocket, full duplex.
//!
//! With `codex.response-steering` on and the client's frames given in
//! [`Options::websocket_input`], a streaming call keeps its connection once
//! its `response.create` is sent. Codex's events reach the client as they
//! come, a response's end included, while the client's next frames go to
//! Codex on the same connection:
//! - a `response.steer` goes as the client sent it, but for the config's
//!   payload rules, while a response runs, so Codex can steer it; for a
//!   SIWC credential its `configuration_update` items are removed first,
//!   without promoting their effort. Codex's `response.steer.*` answers
//!   reach the client byte for byte;
//! - a `response.create` or `response.append` is prepared as the first
//!   request was and sent once no steering waits on Codex and no automatic
//!   successor runs; until then up to [`MAX_QUEUED_CREATES`] wait in turn;
//! - anything else, or a frame that isn't JSON, gets a local `error` event
//!   (a 400 `invalid_request_error`) and the socket goes on.
//!
//! The client's frames are only read once the first `response.created` has
//! reached the client, so a request Codex refuses at once may still fail
//! over to another credential with nothing read. After that nothing is sent
//! again or elsewhere: a create for another model or URL ends the stream with
//! [`ExecError::replay_required`], and a broken connection, a credential
//! turned off, or a failure that can't be told apart ends it with an error
//! that leaves the credential usable (`codexDuplexConnectionError`). Codex
//! refusing the credential (401, 403 or 429) later on is passed on and then
//! ends the stream with that error, which the credential answers for.
//!
//! Each response keeps the settings of the request that made it (whether a
//! native client sent it, the turn's replay scope, multi-agent v2): an
//! explicit create's own, or for an automatic successor the steered
//! response's. The settings of the last [`RESPONSE_WINDOW`] responses are
//! kept, without the request or its headers.
//!
//! Deviations from upstream:
//! - One task reads Codex and the client and writes to Codex, where
//!   upstream runs a writer and a reader goroutine; a send runs while reads
//!   go on, as upstream's does, and the client's next frame is read once it
//!   is done.
//! - At most [`MAX_OUTSTANDING_STEERS`] steering messages may wait on Codex
//!   (sent and not yet answered, or accepted and not yet followed); one
//!   more ends the stream with a connection error. Upstream keeps any
//!   number, so a client could grow them without bound.
//! - The call's taps are told the first request only, then each message
//!   read; later requests, and each response's usage, aren't told
//!   separately as upstream records them.
//! - A frame that isn't UTF-8, or that `serde_json` can't read, gets the
//!   invalid JSON error; Go's `json.Valid` may take one.
//! - The client's frames ending (the client went away) ends the stream with
//!   no error; upstream ends it with `context.Canceled`, which its handler
//!   drops as the client is gone.
//! - Each message has the secrets the call sent redacted before it is
//!   read, as in [`super::stream`]; an empty message is skipped.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{
    ChunkStream, ErrorKind, ExecError, Format, InputFrame, Options, Request, StreamResponse,
    WebsocketInput,
};
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::codex_client::multi_agent_v2;
use open_ferry_translate::go;
use open_ferry_translate::json::exact;
use serde_json::Value;

use super::errors::{self, Failure};
use super::execute::Call;
use super::request::{Prepared, prepare};
use super::session::Hold;
use crate::codex::executor::CodexExecutor;
use crate::codex::ext::{self, Turn};
use crate::codex::request::{Context, Kind, base_model, original_request};
use crate::codex::terminal::{OutputItems, normalize_completion, terminal_failure};
use crate::codex::usage::ensure_responses_usage_details;
use crate::json::{self, str_at};
use crate::payload;
use crate::redact::{Policy, Secrets};

/// How many explicit creates may wait, sent and not yet started, or not
/// yet sent.
pub(super) const MAX_QUEUED_CREATES: usize = 16;

/// How many responses' settings are kept for steering and appends.
pub(super) const RESPONSE_WINDOW: usize = 16;

/// How many steering messages may wait on Codex. Not upstream's.
pub(super) const MAX_OUTSTANDING_STEERS: usize = 64;

/// The error for a frame on a credential no longer enabled.
const CREDENTIAL_DISABLED: &str = "websocket credential is no longer enabled";

/// A send to Codex under way.
type Sending = Pin<Box<dyn Future<Output = Result<(), Failure>> + Send>>;

/// What a response was asked for with (upstream's `codexWebsocketPrepared`,
/// as far as the duplex reads it).
#[derive(Clone, Debug)]
struct Settings {
    /// Whether a native client sent the request, so the completed response
    /// is kept as Codex sent it.
    native: bool,
    event_id: String,
    /// Whether the request renamed the collaboration namespace.
    optimize: bool,
    /// Whether the request already used the renamed namespace.
    conflict: bool,
    /// The turn's hooks.
    turn: Turn,
    /// The URL the request went to.
    url: String,
    /// The prepared body's `instructions`.
    instructions: Option<Value>,
    /// The client's own `instructions`; a kept response's settings drop it.
    original_instructions: Option<Value>,
}

impl Settings {
    fn new(prepared: &Prepared, original: &Value) -> Self {
        Self {
            native: prepared.native,
            event_id: str_at(original, "event_id"),
            optimize: prepared.optimize,
            conflict: prepared.conflict,
            turn: prepared.turn.clone(),
            url: prepared.url.clone(),
            instructions: json::get(&prepared.body, "instructions").cloned(),
            original_instructions: json::get(original, "instructions").cloned(),
        }
    }

    /// The settings kept for a response: not the request itself.
    fn snapshot(&self) -> Self {
        Self {
            event_id: String::new(),
            original_instructions: None,
            ..self.clone()
        }
    }

    /// The `instructions` an append inherits.
    fn inherited_instructions(&self) -> Option<&Value> {
        self.instructions
            .as_ref()
            .or(self.original_instructions.as_ref())
    }
}

/// A steering message waiting for the explicit creates sent before it to
/// start.
#[derive(Clone)]
struct Submission {
    parent: String,
    event_id: String,
    successor: String,
}

struct Steer {
    message: String,
    parent: String,
}

/// What woke the duplex.
enum Wake {
    Sent(Result<(), Failure>),
    Read(Result<String, Failure>),
    Input(Option<InputFrame>),
}

/// The duplex over one connection (`streamCodexDuplex`).
struct Duplex {
    config: Option<Arc<Config>>,
    models: Option<Arc<dyn ModelCatalog>>,
    base_url: String,
    model_level_cooling: bool,
    auth: Auth,
    request: Request,
    options: Options,
    input: WebsocketInput,
    hold: Hold,
    /// The secrets the call sent and those its connection's handshake sent.
    secrets: Secrets,

    initial: Arc<Settings>,
    /// The requested client model, before any payload rewrite or routing.
    bound_model: String,
    /// The settings of the response running, or last run.
    current: Arc<Settings>,
    /// The explicit creates sent and not yet started.
    pending: VecDeque<Arc<Settings>>,
    /// The parents of steering messages sent and not yet answered.
    unacknowledged: Vec<Submission>,
    /// Accepted steering messages, by ID, with their parent, until a
    /// successor of the parent starts.
    accepted: HashMap<String, Submission>,
    response_id: String,
    response_settings: HashMap<String, Arc<Settings>>,
    response_order: VecDeque<String>,
    /// The settings of a steered response, while its steering waits.
    steering_settings: HashMap<String, Arc<Settings>>,
    /// The response waiting for the client's tool results.
    waiting_parent: String,
    /// Whether a response Codex started on its own runs.
    automatic_active: bool,
    first_response: bool,
    business_rejection: bool,
    response_active: bool,
    items: OutputItems,

    /// What goes to the client next.
    out: VecDeque<Result<Bytes, ExecError>>,
    /// Explicit creates waiting to be sent (`pendingCreates`).
    queued_creates: VecDeque<Value>,
    /// A steering message waiting for `pending` to empty.
    deferred: Option<Steer>,
    /// A consumed client frame rejected before its write.
    consumed: Option<ExecError>,
    /// Writer error and recoverability, held behind queued upstream reads.
    write_failure: Option<(ExecError, bool)>,
    sending: Option<Sending>,
    /// Whether the client's frames are read.
    input_ready: bool,
    /// Whether they are read once what is in `out` reached the client.
    opening: bool,
    done: bool,
}

/// Hands the call's connection over to the duplex once its first request
/// is sent (the handoff in `ExecuteStream`).
pub(super) fn start(
    executor: &CodexExecutor,
    auth: &Auth,
    request: Request,
    options: Options,
    input: WebsocketInput,
    prepared: &Prepared,
    call: Call,
) -> StreamResponse {
    let Call {
        hold,
        headers,
        secrets,
    } = call;
    let original = original_request(&request, &options);
    let initial = Arc::new(Settings::new(prepared, &original));
    let original_model = str_at(&original, "model");
    let bound_model = [
        options.metadata.requested_model.trim(),
        original_model.trim(),
        request.model.as_str(),
    ]
    .into_iter()
    .find(|model| !model.is_empty())
    .unwrap_or_default()
    .to_owned();
    let duplex = Duplex {
        config: executor.shared_config(),
        models: executor.shared_models(),
        base_url: executor.base_url().to_owned(),
        model_level_cooling: executor.model_level_cooling(),
        auth: auth.clone(),
        bound_model,
        request,
        options,
        input,
        hold,
        secrets,
        current: Arc::clone(&initial),
        pending: VecDeque::from([Arc::clone(&initial)]),
        initial,
        unacknowledged: Vec::new(),
        accepted: HashMap::new(),
        response_id: String::new(),
        response_settings: HashMap::new(),
        response_order: VecDeque::new(),
        steering_settings: HashMap::new(),
        waiting_parent: String::new(),
        automatic_active: false,
        first_response: true,
        business_rejection: false,
        response_active: false,
        items: OutputItems::default(),
        out: VecDeque::new(),
        queued_creates: VecDeque::new(),
        deferred: None,
        consumed: None,
        write_failure: None,
        sending: None,
        input_ready: false,
        opening: false,
        done: false,
    };
    StreamResponse {
        headers: headers.unwrap_or_default(),
        chunks: duplex.into_stream(),
    }
}

/// A failure of the connection, not the credential's
/// (`codexDuplexConnectionError`).
fn connection_error(mut error: ExecError) -> ExecError {
    error.request_scoped = true;
    error
}

/// A connection error with `message` alone.
fn connection_failure(message: &str) -> ExecError {
    connection_error(ExecError::new(ErrorKind::Upstream, message))
}

/// The local `error` event for a frame Codex never sees: Go's marshalling
/// of upstream's map, keys sorted.
fn rejection(message: &str) -> Bytes {
    Bytes::from(format!(
        r#"{{"error":{{"message":{},"type":"invalid_request_error"}},"status":400,"type":"error"}}"#,
        go::json_string(message)
    ))
}

/// Waits for the send under way, if any.
async fn sent(sending: &mut Option<Sending>) -> Result<(), Failure> {
    match sending {
        Some(sending) => sending.await,
        None => std::future::pending().await,
    }
}

impl Duplex {
    fn into_stream(self) -> ChunkStream {
        futures_util::stream::unfold(self, |mut duplex| async move {
            duplex.next().await.map(|item| (item, duplex))
        })
        .boxed()
    }

    async fn next(&mut self) -> Option<Result<Bytes, ExecError>> {
        loop {
            if let Some(item) = self.out.pop_front() {
                return Some(item);
            }
            if self.done {
                return None;
            }
            if self.opening {
                // What opened the input reached the client.
                self.opening = false;
                self.input_ready = true;
            }
            self.advance();
            if !self.out.is_empty() || self.done {
                continue;
            }
            self.step().await;
        }
    }

    /// Quiesce the single writer before enumerating owned operations.
    fn transport_lost(&mut self) {
        self.hold.session().invalidate(
            self.hold.conn(),
            "upstream_disconnected",
            Some(&Failure::closed()),
        );
        // Dropping this future stops the only writer. Pending already owns
        // a possibly partial write, so it is never classified as not sent.
        self.sending = None;
        let loss = |id: &str| {
            let body = serde_json::json!({"event_id": id}).to_string();
            WebsocketInput::lost(body.as_bytes())
        };
        let running = self.first_response || self.response_active;
        if running {
            self.out.push_back(Err(loss(&self.current.event_id)));
        }
        for settings in &self.pending {
            if !(settings.event_id.is_empty()
                || self.first_response && Arc::ptr_eq(settings, &self.initial))
            {
                self.out.push_back(Err(loss(&settings.event_id)));
            }
        }
        for target in &self.unacknowledged {
            if !target.event_id.is_empty() {
                self.out.push_back(Err(loss(&target.event_id)));
            }
        }
        let mut accepted: Vec<_> = self.accepted.iter().collect();
        accepted.sort_by_key(|(id, _)| *id);
        for (_, target) in accepted {
            if !target.event_id.is_empty() {
                self.out.push_back(Err(loss(&target.event_id)));
            }
        }
        for payload in self
            .queued_creates
            .iter()
            .filter(|payload| !str_at(payload, "event_id").is_empty())
        {
            self.out.push_back(Err(WebsocketInput::not_sent(
                payload.to_string().as_bytes(),
            )));
        }
        if let Some(steer) = &self.deferred
            && !str_at(
                &crate::codex::request::parse_object(steer.message.as_bytes()),
                "event_id",
            )
            .is_empty()
        {
            self.out
                .push_back(Err(WebsocketInput::not_sent(steer.message.as_bytes())));
        }
        let consumed = self.consumed.take();
        let had_consumed = consumed.is_some();
        if let Some(error) = consumed {
            let body: Value =
                serde_json::from_str(&error.message).expect("scoped input error JSON");
            if !str_at(&body, "error.event_id").is_empty() {
                self.out.push_back(Err(error));
            }
        }
        if !running
            && self.out.is_empty()
            && (!self.pending.is_empty()
                || !self.unacknowledged.is_empty()
                || !self.accepted.is_empty()
                || !self.queued_creates.is_empty()
                || self.deferred.is_some()
                || had_consumed)
        {
            // Upstream can correlate only nonempty event IDs. Several
            // anonymous operations share one unscoped loss, never one each.
            self.out.push_back(Err(loss("")));
        }
        // The manager must see one scoped failure, while the handler must
        // answer every outstanding operation. Carry that bounded batch in
        // the scoped error; it is unpacked only by the websocket handler.
        if !self.first_response {
            let mut events = Vec::new();
            while let Some(item) = self.out.pop_front() {
                if let Err(error) = item {
                    let mut event: Value = serde_json::from_str(&error.message).expect("loss JSON");
                    event["type"] = "error".into();
                    event["status"] = error.http_status().into();
                    events.push(event);
                }
            }
            let body = serde_json::json!({"error":{"type":"server_error","code":"upstream_websocket_lost","message":"Upstream websocket disconnected"}, "websocket_loss_events": events});
            self.out.push_back(Err(
                ExecError::upstream(502, body.to_string()).with_websocket_turn()
            ));
        }
        self.finish();
    }

    /// Ends the duplex: the connection is closed and the session's next
    /// call may go. What is in `out` still goes to the client.
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.write_failure = None;
        self.sending = None;
        self.deferred = None;
        self.hold.invalidate("duplex_closed");
        self.hold.release();
    }

    /// Ends the stream with `error`.
    fn fail(&mut self, error: ExecError) {
        if self.done {
            return;
        }
        self.out.push_back(Err(error));
        self.finish();
    }

    /// Ends the stream with a connection error.
    fn fail_connection(&mut self, error: ExecError) {
        self.fail(connection_error(error));
    }

    /// Answers a frame with a local error.
    fn reject(&mut self, message: &str) {
        self.out.push_back(Ok(rejection(message)));
    }

    /// Whether an explicit create may go now: no steering waits on Codex,
    /// no automatic successor runs, and every accepted steering message is
    /// for the response waiting for the client's tool results.
    fn ready_for_create(&self) -> bool {
        self.unacknowledged.is_empty()
            && !self.automatic_active
            && self
                .accepted
                .values()
                .all(|target| !target.successor.is_empty() || target.parent == self.waiting_parent)
    }

    /// Sends what may go now, while no send is under way.
    fn advance(&mut self) {
        if !self.input_ready
            || self.done
            || self.sending.is_some()
            || self.consumed.is_some()
            || self.write_failure.is_some()
        {
            return;
        }
        if self.input.turn_is_stale() {
            // Stop writes, but drain upstream data queued before its loss.
            return;
        }
        if let Some(steer) = self.deferred.take() {
            if self.pending.is_empty() {
                let payload = crate::codex::request::parse_object(steer.message.as_bytes());
                if self.authorize(&payload, true) {
                    self.send_steer(steer);
                }
            } else {
                self.deferred = Some(steer);
            }
            return;
        }
        while self.sending.is_none()
            && !self.done
            && self.consumed.is_none()
            && self.write_failure.is_none()
            && self.ready_for_create()
        {
            let Some(create) = self.queued_creates.pop_front() else {
                break;
            };
            self.process_create(create);
        }
    }

    async fn step(&mut self) {
        let can_read_input = self.input_ready
            && self.sending.is_none()
            && self.deferred.is_none()
            && self.consumed.is_none()
            && self.write_failure.is_none()
            && !self.input.turn_is_stale();
        let sending = self.sending.is_some();
        let wake = tokio::select! {
            biased;
            result = sent(&mut self.sending), if sending => Wake::Sent(result),
            read = self.hold.recv() => Wake::Read(read),
            frame = self.input.recv_duplex(), if can_read_input => Wake::Input(frame),
        };
        match wake {
            Wake::Sent(result) => {
                self.sending = None;
                if let Err(failure) = result {
                    let failure = failure.redacted(&self.secrets);
                    let error = errors::write_error(self.hold.conn().disconnect_code(), &failure);
                    let lost = errors::is_turn_loss(&failure) && error.status != 413;
                    if lost {
                        self.hold.session().invalidate(
                            self.hold.conn(),
                            "send_error",
                            Some(&failure),
                        );
                    }
                    self.write_failure = Some((error, lost));
                    self.hold.invalidate("send_error");
                }
            }
            Wake::Read(read) => self.on_read(read),
            Wake::Input(frame) => self.on_input(frame),
        }
    }

    fn start_send(&mut self, message: String) {
        let conn = Arc::clone(self.hold.conn());
        self.sending = Some(Box::pin(async move { conn.send(message).await }));
    }

    fn live_check(&self) -> Result<Arc<Auth>, ExecError> {
        let auth = self
            .input
            .live_auth(&self.auth)
            .ok_or_else(|| connection_failure(CREDENTIAL_DISABLED))?;
        crate::codex::siwc_request::ensure_credentials(&auth, &self.options.headers)?;
        if auth.use_siwc_for_request(&self.options.headers)
            != self.auth.use_siwc_for_request(&self.options.headers)
            || (auth.use_siwc_for_request(&self.options.headers)
                && auth.access_token_for_request(&self.options.headers)
                    != self.auth.access_token_for_request(&self.options.headers))
        {
            return Err(connection_error(ExecError::replay_required()));
        }
        Ok(auth)
    }

    /// One of the client's frames (the writer's loop).
    fn on_input(&mut self, frame: Option<InputFrame>) {
        let payload = match frame {
            None => {
                // The client went away.
                self.finish();
                return;
            }
            Some(InputFrame::Err(error)) => {
                if error.failure_class
                    == Some(open_ferry_core::exec::FailureClass::UpstreamWebsocketTurn)
                {
                    self.consumed = Some(error);
                    self.hold.invalidate("input_loss");
                } else {
                    self.fail_connection(error);
                }
                return;
            }
            Some(InputFrame::Payload(payload) | InputFrame::Stamped(payload, _)) => payload,
        };
        if let Err(error) = self.live_check() {
            // A new socket may pick an enabled credential; this frame is
            // neither sent on this one nor replayed elsewhere.
            self.fail(error);
            return;
        }
        let parsed = std::str::from_utf8(&payload)
            .ok()
            .filter(|_| go::json_valid(&payload))
            .and_then(|text| exact::from_str(text).ok().map(|value| (text, value)));
        let Some((text, value)) = parsed else {
            self.reject("invalid websocket request JSON");
            return;
        };
        match str_at(&value, "type").as_str() {
            "response.steer" => self.on_steer(text, value),
            "response.create" | "response.append" => {
                if self.queued_creates.is_empty() && self.ready_for_create() {
                    self.process_create(value);
                } else if self.queued_creates.len() >= MAX_QUEUED_CREATES {
                    self.fail_connection(connection_failure(
                        "too many outstanding response.create requests",
                    ));
                } else {
                    self.queued_creates.push_back(value);
                }
            }
            other => {
                let message = format!("unsupported websocket request type: {other}");
                self.reject(&message);
            }
        }
    }

    /// A steering message: as the client sent it, but for the payload
    /// rules, never with a create's defaults. For a SIWC credential its
    /// `configuration_update` items are removed first, without promoting
    /// their effort, and the rules see what is left.
    fn on_steer(&mut self, text: &str, mut body: Value) {
        if !self.authorize(&body, true) {
            return;
        }
        let original = body.clone();
        let steer_request = Request {
            model: self.request.model.clone(),
            payload: Bytes::copy_from_slice(text.as_bytes()),
        };
        if self.auth.use_siwc_for_request(&self.options.headers) {
            crate::thinking::strip_configuration_updates(&mut body);
        }
        let stripped = body.clone();
        let source = |_: Value| stripped.clone();
        let target = payload::Target {
            executor: "codex-websockets",
            protocol: &Format::CODEX,
            model: base_model(&self.request.model),
            root: "",
            stream: true,
            tracked: &[],
            translate: Some(&source),
        };
        payload::apply(
            self.config.as_deref(),
            &target,
            &steer_request,
            &self.options,
            &mut body,
        );
        json::set(&mut body, "type", Value::from("response.steer"));
        let message = if body == original {
            text.to_owned()
        } else {
            body.to_string()
        };
        if self.unacknowledged.len() + self.accepted.len() >= MAX_OUTSTANDING_STEERS {
            self.fail_connection(connection_failure(
                "too many outstanding response.steer requests",
            ));
            return;
        }
        let steer = Steer {
            parent: str_at(&body, "previous_response_id"),
            message,
        };
        if !self.response_settings.contains_key(&steer.parent) && !self.pending.is_empty() {
            // Its parent may be a create sent before it.
            self.deferred = Some(steer);
            return;
        }
        self.send_steer(steer);
    }

    fn send_steer(&mut self, steer: Steer) {
        if self.input.turn_is_stale() {
            self.consumed = Some(WebsocketInput::not_sent(steer.message.as_bytes()));
            self.hold.invalidate("input_loss");
            return;
        }
        let settings = self.response_settings.get(&steer.parent).cloned();
        self.unacknowledged.push(Submission {
            parent: steer.parent.clone(),
            event_id: str_at(
                &crate::codex::request::parse_object(steer.message.as_bytes()),
                "event_id",
            ),
            successor: String::new(),
        });
        if let Some(settings) = settings {
            self.steering_settings.insert(steer.parent, settings);
        }
        if let Err(error) = self.live_check() {
            self.fail(error);
            return;
        }
        self.start_send(steer.message);
    }

    /// An explicit create or append, prepared as the first request was
    /// (`processCreatePayload`).
    fn process_create(&mut self, mut payload: Value) {
        if self.input.turn_is_stale() {
            self.consumed = Some(WebsocketInput::not_sent(payload.to_string().as_bytes()));
            self.hold.invalidate("input_loss");
            return;
        }
        if !self.authorize(&payload, false) {
            return;
        }
        if self.input.turn_is_stale() {
            self.consumed = Some(WebsocketInput::not_sent(payload.to_string().as_bytes()));
            self.hold.invalidate("input_loss");
            return;
        }
        let auth = match self.live_check() {
            Ok(auth) => auth,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        let is_append = str_at(&payload, "type") == "response.append";
        let mut previous = str_at(&payload, "previous_response_id").trim().to_owned();
        if is_append && previous.is_empty() && !self.response_id.is_empty() {
            previous.clone_from(&self.response_id);
            json::set(
                &mut payload,
                "previous_response_id",
                Value::from(previous.clone()),
            );
        }
        if self
            .accepted
            .values()
            .any(|accepted| accepted.successor.is_empty() && previous != self.waiting_parent)
        {
            self.reject("response.create must continue the response waiting for required input");
            return;
        }
        let model = str_at(&payload, "model").trim().to_owned();
        if !model.is_empty() && model != self.request.model && model != self.bound_model {
            self.fail_connection(ExecError::replay_required());
            return;
        }
        if model.is_empty() {
            let model = if self.request.model.is_empty() {
                self.bound_model.trim().to_owned()
            } else {
                self.request.model.clone()
            };
            json::set(&mut payload, "model", Value::from(model));
        }
        if is_append && !json::exists(&payload, "instructions") {
            let inherited = self
                .response_settings
                .get(&previous)
                .unwrap_or(&self.initial)
                .inherited_instructions()
                .or_else(|| self.initial.inherited_instructions())
                .cloned();
            if let Some(instructions) = inherited {
                json::set(&mut payload, "instructions", instructions);
            }
        }

        let body = Bytes::from(payload.to_string());
        let request = Request {
            model: self.request.model.clone(),
            payload: body.clone(),
        };
        let mut options = self.options.clone();
        options.original_request = body;
        let context = Context {
            auth: Some(&auth),
            config: self.config.as_deref(),
            models: self.models.as_deref(),
        };
        let prepared = match prepare(
            Kind::Stream,
            context,
            &auth,
            &self.base_url,
            &request,
            &options,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail_connection(error);
                return;
            }
        };
        if prepared.url != self.initial.url {
            self.fail_connection(ExecError::replay_required());
            return;
        }
        if self.pending.len() >= MAX_QUEUED_CREATES {
            self.fail_connection(connection_failure(
                "too many outstanding response.create requests",
            ));
            return;
        }
        if self.input.turn_is_stale() {
            self.consumed = Some(WebsocketInput::not_sent(payload.to_string().as_bytes()));
            self.hold.invalidate("input_loss");
            return;
        }
        let original = original_request(&request, &options);
        self.pending
            .push_back(Arc::new(Settings::new(&prepared, &original)));
        if prepared.optimize || prepared.conflict {
            self.hold.session().set_multi_agent_optimized(
                self.hold.conn().id(),
                prepared.optimize && !prepared.conflict,
            );
        }
        if let Err(error) = self.live_check() {
            self.fail(error);
            return;
        }
        self.start_send(prepared.message);
    }

    /// Authorize the frontend turn using the initial client's model when
    /// this is a continuation of the bound request. Only the access check
    /// sees the adjusted copy; steering keeps its own payload defaults.
    fn authorize(&mut self, payload: &Value, force_bound_model: bool) -> bool {
        let bound = match self.bound_model.trim() {
            "" => self.request.model.as_str(),
            model => model,
        };
        let model = str_at(payload, "model");
        let mut body = payload.clone();
        if !bound.is_empty()
            && (force_bound_model || model.trim().is_empty() || model.trim() == self.request.model)
        {
            json::set(&mut body, "model", Value::from(bound));
        }
        match self.input.authorize_turn(body.to_string().as_bytes()) {
            Ok(None) => true,
            Ok(Some(error_payload)) => {
                self.out.push_back(Ok(error_payload));
                false
            }
            Err(error) => {
                self.fail_connection(error);
                false
            }
        }
    }

    /// One of Codex's messages (the reader's loop).
    fn on_read(&mut self, read: Result<String, Failure>) {
        let payload = match read {
            Ok(payload) => payload,
            Err(failure) => {
                let lost = errors::is_turn_loss(&failure);
                let read_error = errors::error(&failure.redacted(&self.secrets));
                let (error, lost) = self.write_failure.take().unwrap_or((read_error, lost));
                let known = self.business_rejection
                    && !self.response_active
                    && self.pending.is_empty()
                    && self.unacknowledged.is_empty()
                    && self.accepted.is_empty()
                    && self.queued_creates.is_empty()
                    && self.deferred.is_none()
                    && self.consumed.is_none();
                if lost && error.status != 413 && !known {
                    self.transport_lost();
                } else {
                    self.fail_connection(error);
                }
                return;
            }
        };
        // Each message, as it is read; the taps read it as it came.
        let payload = self.secrets.text(payload, Policy::Client);
        let payload = go::trim_space(payload.as_bytes());
        if payload.is_empty() {
            return;
        }
        let event: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let event_type = str_at(&event, "type");
        let establishing = self.first_response && event_type == "response.created";
        if event_type == "response.created" && !self.on_created(&event) {
            return;
        }
        if event_type.starts_with("response.steer.") {
            self.on_steer_event(&event, &event_type);
            // Opaque to the proxy: passed on byte for byte.
            self.out.push_back(Ok(Bytes::copy_from_slice(payload)));
            return;
        }
        let failure = event_type == "error" || event_type == "response.failed";
        if !self.first_response
            && failure
            && let Some(error) = self.credential_error(&event)
        {
            // The credential's health, whichever request failed; the
            // stream isn't replayed elsewhere.
            self.out.push_back(Ok(Bytes::copy_from_slice(payload)));
            self.fail(error);
            return;
        }
        let mut settings = Arc::clone(&self.current);
        if !self.first_response && failure {
            let mut failed_id = str_at(&event, "response.id");
            if failed_id.is_empty() {
                failed_id = str_at(&event, "response_id");
            }
            // A failure of the running response leaves the queued creates
            // alone; one before `response.created` is the oldest create's.
            let current_failure = !failed_id.is_empty() && failed_id == self.response_id;
            let ambiguous = failed_id.is_empty()
                && ((!self.pending.is_empty() && self.response_active)
                    || !self.unacknowledged.is_empty());
            if ambiguous {
                // Guessing could hand the failure to the wrong request.
                self.out.push_back(Ok(Bytes::copy_from_slice(payload)));
                self.fail_connection(connection_failure(
                    "cannot associate websocket failure with a response or pending create",
                ));
                return;
            }
            if !current_failure && let Some(rejected) = self.pending.pop_front() {
                settings = rejected;
            } else {
                self.response_active = false;
                self.automatic_active = false;
            }
        }

        let restore = !settings.conflict
            && (settings.optimize
                || self
                    .hold
                    .session()
                    .is_multi_agent_optimized(self.hold.conn().id()));
        let data = multi_agent_v2::restore_response(payload, restore);
        let event = match &data {
            std::borrow::Cow::Owned(restored) => {
                serde_json::from_slice(restored).unwrap_or(Value::Null)
            }
            std::borrow::Cow::Borrowed(_) => event,
        };
        // Each refused request clears its own turn's replay; only the first
        // can still fail over.
        let terminal = if let Some((error, status, raw)) = errors::parse_ws_error(
            &event,
            self.model_level_cooling,
            &self.secrets,
            SystemTime::now(),
        ) {
            ext::on_failure(&settings.turn, status, raw.as_bytes());
            Some(error)
        } else if let Some((error, body)) = terminal_failure(&event, self.model_level_cooling) {
            ext::on_failure(&settings.turn, error.status, body.as_bytes());
            Some(error.redacted(&self.secrets).into())
        } else {
            None
        };
        if let Some(error) = terminal {
            self.business_rejection = error.http_status() == 400;
            if self.first_response {
                self.fail(error);
                return;
            }
        }

        if event_type == "response.output_item.done" {
            self.items.collect(&event);
        }
        if super::session::is_terminal_event(&event_type) {
            let mut ended = str_at(&event, "response.id");
            if ended.is_empty() {
                ended = str_at(&event, "response_id");
            }
            if ended.is_empty() && !self.response_active {
                ended.clone_from(&self.response_id);
            }
            let parents: Vec<_> = self
                .accepted
                .values()
                .filter(|accepted| !ended.is_empty() && accepted.successor == ended)
                .map(|accepted| accepted.parent.clone())
                .collect();
            self.accepted
                .retain(|_, accepted| ended.is_empty() || accepted.successor != ended);
            for parent in parents {
                self.release_steering_settings(&parent);
            }
        }
        let chunk = if matches!(
            event_type.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        ) {
            self.business_rejection = false;
            self.response_active = false;
            self.automatic_active = false;
            let mut completed = event;
            let normalized = normalize_completion(&mut completed);
            let patched = !self.current.native && self.items.patch(&mut completed);
            if event_type != "response.incomplete" {
                ext::on_completed(&self.current.turn, &completed);
            }
            if normalized || patched {
                completed.to_string().into_bytes()
            } else {
                data.into_owned()
            }
        } else {
            data.into_owned()
        };
        self.out
            .push_back(Ok(Bytes::from(ensure_responses_usage_details(chunk))));
        if establishing {
            // `response.created` reaches the client before any local error.
            self.opening = true;
        }
    }

    /// A response started: an explicit create's, or an automatic successor
    /// of a steered one. Returns whether the stream goes on.
    fn on_created(&mut self, event: &Value) -> bool {
        let mut parent = str_at(event, "response.previous_response_id");
        if parent.is_empty() {
            parent.clone_from(&self.response_id);
        }
        let automatic = !self.first_response && self.pending.is_empty();
        if automatic {
            let Some(settings) = self
                .steering_settings
                .get(&parent)
                .or_else(|| self.response_settings.get(&parent))
            else {
                self.fail_connection(connection_failure(
                    "automatic successor has no retained parent settings",
                ));
                return false;
            };
            self.current = Arc::clone(settings);
        }
        let successor = str_at(event, "response.id");
        for target in self
            .accepted
            .values_mut()
            .filter(|target| target.parent == parent && target.successor.is_empty())
        {
            target.successor.clone_from(&successor);
        }
        self.waiting_parent.clear();
        self.automatic_active = automatic;
        if let Some(next) = self.pending.pop_front() {
            self.current = next;
        }
        self.response_id = str_at(event, "response.id");
        // The response's settings, not its request or headers. Steering
        // under way keeps its parent's own.
        self.response_settings
            .insert(self.response_id.clone(), Arc::new(self.current.snapshot()));
        self.response_order.push_back(self.response_id.clone());
        if self.response_order.len() > RESPONSE_WINDOW
            && let Some(oldest) = self.response_order.pop_front()
        {
            self.response_settings.remove(&oldest);
        }
        self.release_steering_settings(&parent);
        self.first_response = false;
        self.response_active = true;
        self.items = OutputItems::default();
        true
    }

    /// Codex's answer to a steering message.
    fn on_steer_event(&mut self, event: &Value, event_type: &str) {
        let id = str_at(event, "steer.id");
        let mut parent = str_at(event, "steer.previous_response_id");
        if parent.is_empty() {
            parent.clone_from(&self.response_id);
        }
        match event_type {
            "response.steer.accepted" => {
                let event_id = self
                    .consume_submission(&parent)
                    .map(|submission| submission.event_id)
                    .unwrap_or_default();
                self.accepted.insert(
                    id,
                    Submission {
                        parent,
                        event_id,
                        successor: String::new(),
                    },
                );
            }
            "response.steer.failed" => {
                if self.accepted.remove(&id).is_none() {
                    self.consume_submission(&parent);
                }
                self.release_steering_settings(&parent);
            }
            "response.steer.pending" => {
                // The client's tool results may already wait to be sent; no
                // successor starts before they go.
                self.waiting_parent = parent;
            }
            _ => {}
        }
    }

    /// Takes the first unanswered steering message for `parent`.
    fn consume_submission(&mut self, parent: &str) -> Option<Submission> {
        if let Some(index) = self
            .unacknowledged
            .iter()
            .position(|target| target.parent == parent || target.parent.is_empty())
        {
            return Some(self.unacknowledged.remove(index));
        }
        None
    }

    /// Drops `parent`'s steering settings once no steering waits on it.
    fn release_steering_settings(&mut self, parent: &str) {
        let waiting = self
            .unacknowledged
            .iter()
            .any(|target| target.parent == parent)
            || self.accepted.values().any(|target| target.parent == parent);
        if !waiting {
            self.steering_settings.remove(parent);
        }
    }

    /// Codex refusing the credential (401, 403 or 429) in an `error` or
    /// `response.failed` event.
    fn credential_error(&self, event: &Value) -> Option<ExecError> {
        let error = if let Some((error, _, _)) = errors::parse_ws_error(
            event,
            self.model_level_cooling,
            &self.secrets,
            SystemTime::now(),
        ) {
            error
        } else {
            let (error, _) = terminal_failure(event, self.model_level_cooling)?;
            error.redacted(&self.secrets).into()
        };
        matches!(error.status, 401 | 403 | 429).then_some(error)
    }
}
