//! [`ClaudeCliExecutor`], which answers each call by running the entry's
//! Claude Code once (see the [module docs](super)).
//!
//! The call's taps see Claude Code's run as an attempt at
//! `claude-cli://<entry>`: the line it was given, then, once its answer
//! begins, a success with the account's rate-limit headers and its answer
//! as Claude's SSE events, before any translation. So usage is reported
//! from them as for the Claude provider.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use bytes::Bytes;
use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use http::{HeaderMap, Method};
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::synthesizer::claude_cli::CLAUDE_CLI_PROVIDER;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response, StreamResponse};
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::AttemptKind;
use open_ferry_translate::registry::{Registry, ResponseContext, ResponseStream};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::aggregate;
use super::events::{Events, Step};
use super::process::{self, Invocation, Running};
use super::prompt;
use super::settings::{Entry, default_work_root};
use crate::claude::executor::{
    COMPACT_ALT, compact_error, original_request, response_format, translate_request,
};
use crate::claude::stream::apply_patch_error;
use crate::claude::usage::ensure_responses_usage_details;
use crate::json;
use crate::observe_send::{self, Attempt, BodyTap};
use crate::redact::Secrets;
use crate::thinking::parse_suffix;

/// Each entry's concurrency slots, by its credential and name: the limit
/// and its semaphore. Shared by every executor, so one made again on a
/// config reload doesn't double them. A changed limit takes a new
/// semaphore; runs holding the old one finish with it.
static SLOTS: LazyLock<Mutex<HashMap<String, Slots>>> = LazyLock::new(Mutex::default);

/// An entry's concurrency limit and the semaphore that keeps it.
type Slots = (usize, Arc<Semaphore>);

fn slots(auth: &Auth, entry: &Entry) -> Arc<Semaphore> {
    let key = format!("{}\0{}", auth.id, entry.name);
    let limit = entry.max_concurrency.max(1);
    let mut slots = SLOTS.lock().unwrap_or_else(PoisonError::into_inner);
    match slots.get(&key) {
        Some((max, semaphore)) if *max == limit => Arc::clone(semaphore),
        _ => {
            let semaphore = Arc::new(Semaphore::new(limit));
            slots.insert(key, (limit, Arc::clone(&semaphore)));
            semaphore
        }
    }
}

/// Serves Claude models through the user's own installed Claude Code.
#[derive(Debug)]
pub struct ClaudeCliExecutor {
    config: Option<Arc<Config>>,
    work_root: PathBuf,
}

impl Default for ClaudeCliExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCliExecutor {
    /// An executor keeping its entries' files under [`default_work_root`].
    pub fn new() -> Self {
        Self {
            config: None,
            work_root: default_work_root(),
        }
    }

    /// Readies Codex clients' requests as `config` says before translating
    /// them, as the Claude executor does.
    pub fn with_config(mut self, config: Arc<Config>) -> Self {
        self.config = Some(config);
        self
    }

    /// Keeps the entries' working directories and prompt files under
    /// `root`.
    pub fn with_work_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.work_root = root.into();
        self
    }

    /// Readies the call and starts Claude Code for it, once a slot is
    /// free.
    async fn start(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        kind: AttemptKind,
        upstream_stream: bool,
    ) -> Result<(Run, Value), ExecError> {
        if options.alt == COMPACT_ALT {
            return Err(compact_error());
        }
        let base_model = parse_suffix(&request.model).0;
        if base_model.trim().is_empty() || base_model.starts_with('-') {
            return Err(prompt::invalid(format!(
                "claude-cli: {base_model:?} isn't a model name"
            )));
        }
        let mut translation = translate_request(
            self.config.as_deref(),
            request,
            options,
            base_model,
            upstream_stream,
        )?;
        if translation.get("stream") != Some(&Value::Bool(upstream_stream)) {
            json::set(&mut translation, "stream", Value::Bool(upstream_stream));
        }
        let prepared = prompt::prepare(&translation)?;

        let entry = Entry::from_auth(auth);
        let deadline = Instant::now() + entry.timeout;
        let mut run = Run {
            name: entry.name.clone(),
            hint: match entry.config_dir_path() {
                Some(dir) => format!("CLAUDE_CONFIG_DIR={} claude auth status", dir.display()),
                None => "claude auth status".to_owned(),
            },
            timeout_secs: entry.timeout.as_secs(),
            deadline,
            running: None,
            events: Events::new(prepared.thinking),
            tap: None,
        };
        let slot =
            match tokio::time::timeout_at(deadline, slots(auth, &entry).acquire_owned()).await {
                Ok(Ok(slot)) => slot,
                Ok(Err(_)) => {
                    return Err(ExecError::upstream(
                        502,
                        format!("claude-cli {}: the entry's slots are gone", run.name),
                    ));
                }
                Err(_) => {
                    return Err(ExecError::upstream(
                        504,
                        format!(
                            "claude-cli {}: no slot came free within the entry's timeout of {}s",
                            run.name, run.timeout_secs
                        ),
                    ));
                }
            };

        let claude = Format::CLAUDE;
        let attempt = Attempt::new(
            options,
            kind,
            CLAUDE_CLI_PROVIDER,
            base_model,
            &claude,
            auth,
        );
        run.tap = attempt.observation.map(|observation| {
            let url = format!("claude-cli://{}", entry.name);
            let body = Bytes::from(prepared.input.clone());
            observe_send::announce(
                observation,
                &attempt.request(
                    &Method::POST,
                    &url,
                    &HeaderMap::new(),
                    &body,
                    &Secrets::new(),
                ),
            )
        });
        let invocation = Invocation {
            entry: &entry,
            model: base_model,
            system: &prepared.system,
            effort: prepared.effort.as_deref(),
            max_output_tokens: prepared.max_output_tokens,
            thinking_budget: prepared.thinking_budget,
            input: prepared.input,
        };
        let running = process::spawn(&self.work_root, invocation, slot).map_err(|error| {
            ExecError::upstream(502, format!("claude-cli {}: {error}", run.name))
        })?;
        run.running = Some(running);
        Ok((run, translation))
    }

    async fn execute_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let format = response_format(options);
        // A client that needs translating gets it from Claude's stream.
        let upstream_stream = format != Format::CLAUDE;
        let (mut run, translation) = self
            .start(
                auth,
                request,
                options,
                AttemptKind::Execute,
                upstream_stream,
            )
            .await?;
        let mut events = Vec::new();
        loop {
            match run.next().await? {
                Next::Nothing => {}
                Next::Events(more) => events.extend(more),
                Next::Restart(again) => events = again,
                Next::Done(last) => {
                    events.extend(last);
                    break;
                }
            }
        }
        let headers = run.events.rate_headers().clone();
        let sse = sse(&events);
        if let Some(tap) = &run.tap {
            tap.response_head(200, &headers);
            tap.chunk(&Bytes::from(sse.clone()));
        }
        let out = if format == Format::CLAUDE {
            let message = aggregate::message(&events).ok_or_else(|| {
                ExecError::upstream(502, "claude-cli: Claude Code's answer had no message")
            })?;
            message.to_string().into_bytes()
        } else {
            let original = original_request(request, options);
            let context = ResponseContext {
                model: &request.model,
                original_request: &original,
                request: &translation,
            };
            let mut out = Registry::global()
                .translate_non_stream(&Format::CLAUDE, &format, &context, sse.into_bytes())
                .filter(|out| !out.is_empty())
                .ok_or_else(apply_patch_error)?;
            if format == Format::OPENAI_RESPONSE {
                out = ensure_responses_usage_details(out);
            }
            out
        };
        Ok(Response {
            payload: Bytes::from(out),
            headers,
        })
    }

    async fn execute_stream_inner(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        let format = response_format(options);
        let (mut run, translation) = self
            .start(auth, request, options, AttemptKind::Stream, true)
            .await?;
        // The stream starts with the answer's first events; until then a
        // failure is the call's.
        let (first, done) = loop {
            match run.next().await? {
                Next::Nothing => {}
                Next::Events(events) | Next::Restart(events) => break (events, false),
                Next::Done(events) => break (events, true),
            }
        };
        run.events.mark_sent();
        let headers = run.events.rate_headers().clone();
        if let Some(tap) = &run.tap {
            tap.response_head(200, &headers);
        }
        let translator = (format != Format::CLAUDE).then(|| {
            let original = original_request(request, options);
            let context = ResponseContext {
                model: &request.model,
                original_request: &original,
                request: &translation,
            };
            Registry::global().response_stream(&Format::CLAUDE, &format, &context)
        });
        let mut state = Stream {
            run,
            translator,
            responses: format == Format::OPENAI_RESPONSE,
            pending: VecDeque::new(),
            finished: false,
        };
        state.queue_events(first);
        if done {
            state.end();
        }
        let chunks = futures_util::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.finished {
                    return None;
                }
                state.step().await;
            }
        })
        .boxed();
        Ok(StreamResponse { headers, chunks })
    }
}

/// What one read of Claude Code's output gave.
enum Next {
    Nothing,
    Events(Vec<Value>),
    Restart(Vec<Value>),
    Done(Vec<Value>),
}

/// One run of Claude Code. Dropping it kills the process.
struct Run {
    name: String,
    /// The command that checks the entry's sign-in.
    hint: String,
    timeout_secs: u64,
    deadline: Instant,
    running: Option<Running>,
    events: Events,
    tap: Option<BodyTap>,
}

impl Run {
    /// Reads the next line of Claude Code's output. Its result ends the
    /// run, as do its end, the deadline, and a failure retrying won't mend,
    /// for which the process is stopped.
    async fn next(&mut self) -> Result<Next, ExecError> {
        let Some(running) = self.running.as_mut() else {
            return Err(ExecError::upstream(
                502,
                format!("claude-cli {}: Claude Code isn't running", self.name),
            ));
        };
        let line = match tokio::time::timeout_at(self.deadline, running.stdout.next_line()).await {
            Ok(Some(Ok(line))) => line,
            Ok(Some(Err(error))) => {
                self.kill().await;
                return Err(ExecError::upstream(
                    502,
                    format!("claude-cli {}: {error}", self.name),
                ));
            }
            Ok(None) => return Err(self.no_result().await),
            Err(_) => {
                self.kill().await;
                return Err(ExecError::upstream(
                    504,
                    format!(
                        "claude-cli {}: Claude Code didn't answer within the entry's timeout of \
                         {}s, and was stopped",
                        self.name, self.timeout_secs
                    ),
                ));
            }
        };
        match self.events.feed(&line) {
            Step::Nothing => Ok(Next::Nothing),
            Step::Events(events) => Ok(Next::Events(events)),
            Step::Restart(events) => Ok(Next::Restart(events)),
            Step::Stop(error) => {
                self.kill().await;
                Err(error)
            }
            Step::Done(result) => {
                if let Some(running) = self.running.take() {
                    running.finish(self.name.clone());
                }
                result.map(Next::Done)
            }
        }
    }

    async fn kill(&mut self) {
        if let Some(mut running) = self.running.take() {
            running.kill(&self.name).await;
        }
    }

    /// The error for output that ended without a result.
    async fn no_result(&mut self) -> ExecError {
        let mut exit = String::new();
        if let Some(mut running) = self.running.take() {
            if let Some(status) = running.exit_status().await {
                exit = format!(" ({status})");
            }
            running.log_stderr(&self.name).await;
        }
        ExecError::upstream(
            502,
            format!(
                "claude-cli {}: Claude Code ended without an answer{exit}; check that it is \
                 signed in with `{}`",
                self.name, self.hint
            ),
        )
    }
}

/// A streamed call's state.
struct Stream {
    run: Run,
    translator: Option<ResponseStream>,
    responses: bool,
    pending: VecDeque<Result<Bytes, ExecError>>,
    finished: bool,
}

impl Stream {
    async fn step(&mut self) {
        match self.run.next().await {
            Ok(Next::Nothing) => {}
            Ok(Next::Events(events)) => self.queue_events(events),
            Ok(Next::Restart(_)) => self.fail(ExecError::upstream(
                502,
                "claude-cli: Claude Code began its answer again after part of it was sent",
            )),
            Ok(Next::Done(events)) => {
                self.queue_events(events);
                self.end();
            }
            Err(error) => self.fail(error),
        }
    }

    /// Passes events on: as Claude's SSE to a Claude client, else
    /// translated a line at a time.
    fn queue_events(&mut self, events: Vec<Value>) {
        for event in events {
            if self.finished {
                return;
            }
            let text = sse_event(&event);
            let bytes = Bytes::from(text.clone());
            if let Some(tap) = &self.run.tap {
                tap.chunk(&bytes);
            }
            let Some(translator) = self.translator.as_mut() else {
                self.pending.push_back(Ok(bytes));
                continue;
            };
            let mut chunks = Vec::new();
            for line in text.trim_end_matches('\n').split('\n').chain([""]) {
                chunks.extend(translator.translate(line.as_bytes()));
            }
            let failed = translator.tool_input_error().is_some();
            self.queue(chunks, !failed);
            if failed {
                self.pending.push_back(Err(apply_patch_error()));
                self.finished = true;
            }
        }
    }

    fn queue(&mut self, chunks: Vec<Vec<u8>>, usage_details: bool) {
        for mut chunk in chunks {
            if chunk.is_empty() {
                continue;
            }
            if self.responses && usage_details {
                chunk = ensure_responses_usage_details(chunk);
            }
            self.pending.push_back(Ok(Bytes::from(chunk)));
        }
    }

    /// Ends the stream, with what the translator has left.
    fn end(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(translator) = self.translator.as_mut() {
            let chunks = translator.finish();
            let failed = translator.tool_input_error().is_some();
            self.queue(chunks, !failed);
            if failed {
                self.pending.push_back(Err(apply_patch_error()));
            }
        }
    }

    /// Ends the stream with `error`.
    fn fail(&mut self, error: ExecError) {
        tracing::debug!("claude-cli: the stream failed: {}", error.message);
        if let Some(tap) = &self.run.tap {
            tap.error(&error.message);
        }
        self.end();
        self.pending.push_back(Err(error));
    }
}

/// An event as Claude's SSE: its `event:` and `data:` lines and a blank
/// line.
fn sse_event(event: &Value) -> String {
    format!("event: {}\ndata: {event}\n\n", json::str_at(event, "type"))
}

/// Events as Claude's SSE.
fn sse(events: &[Value]) -> String {
    events.iter().map(sse_event).collect()
}

impl ProviderExecutor for ClaudeCliExecutor {
    fn id(&self) -> &str {
        CLAUDE_CLI_PROVIDER
    }

    fn execute(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move { self.execute_inner(&auth, &request, &options).await }.boxed()
    }

    fn execute_stream(
        &self,
        auth: Arc<Auth>,
        request: Request,
        options: Options,
    ) -> BoxFuture<'_, Result<StreamResponse, ExecError>> {
        async move { self.execute_stream_inner(&auth, &request, &options).await }.boxed()
    }

    fn count_tokens(
        &self,
        _auth: Arc<Auth>,
        _request: Request,
        _options: Options,
    ) -> BoxFuture<'_, Result<Response, ExecError>> {
        async move {
            Err(ExecError::upstream(
                501,
                "claude-cli: counting tokens isn't supported; Claude Code has no token count",
            ))
        }
        .boxed()
    }

    /// Returns the credential as it is: Claude Code keeps its own sign-in,
    /// and open-ferry holds no token for it.
    fn refresh(&self, auth: Arc<Auth>) -> BoxFuture<'_, Result<Auth, ExecError>> {
        async move { Ok((*auth).clone()) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_events_as_sse() {
        let events = [
            serde_json::json!({"type": "message_start", "message": {"id": "m"}}),
            serde_json::json!({"type": "message_stop"}),
        ];
        assert_eq!(
            sse(&events),
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }

    #[test]
    fn shares_slots_by_entry_and_limit() {
        let auth = Auth {
            id: "slots-test".into(),
            ..Auth::default()
        };
        let entry = Entry {
            name: "a".into(),
            max_concurrency: 2,
            ..Entry::from_auth(&auth)
        };
        let first = slots(&auth, &entry);
        assert!(Arc::ptr_eq(&first, &slots(&auth, &entry)));
        assert_eq!(first.available_permits(), 2);
        let other = Entry {
            name: "b".into(),
            ..entry.clone()
        };
        assert!(!Arc::ptr_eq(&first, &slots(&auth, &other)));
        let wider = Entry {
            max_concurrency: 3,
            ..entry
        };
        let changed = slots(&auth, &wider);
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(changed.available_permits(), 3);
    }

    #[tokio::test]
    async fn refuses_what_it_cant_do() {
        let executor = ClaudeCliExecutor::new();
        let auth = Arc::new(Auth::default());
        let request = Request {
            model: "claude-sonnet-5-5".into(),
            payload: Bytes::from_static(b"{}"),
        };
        let error = executor
            .count_tokens(
                Arc::clone(&auth),
                request.clone(),
                Options::new(Format::CLAUDE),
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 501);

        let mut compact = Options::new(Format::OPENAI_RESPONSE);
        compact.alt = COMPACT_ALT.into();
        let error = executor
            .execute(Arc::clone(&auth), request.clone(), compact)
            .await
            .unwrap_err();
        assert_eq!(error.status, 501);

        let flag = Request {
            model: "--help".into(),
            payload: Bytes::from_static(br#"{"messages":[{"role":"user","content":"Hi"}]}"#),
        };
        let error = executor
            .execute(Arc::clone(&auth), flag, Options::new(Format::CLAUDE))
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);

        let refreshed = executor.refresh(Arc::clone(&auth)).await.unwrap();
        assert_eq!(refreshed.id, auth.id);
    }
}
