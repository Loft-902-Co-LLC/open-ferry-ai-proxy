//! Random input for the OpenAI Responses and Interactions response
//! translators' suites (P4 WP4-C2):
//! - [`to_responses_cases`]: Interactions event streams and whole responses
//!   for an OpenAI Responses client, whose request declares tools of every
//!   kind: functions, custom tools, namespaces and `apply_patch`, directly
//!   or in a namespace. Half are the shared generator's interactions (see
//!   `crate::generate::interactions`), their calls renamed to the declared
//!   tools; half are built around a call to `apply_patch`, its arguments
//!   valid or not, streamed in pieces, named late, repeated in snapshots,
//!   and the interaction completed, failed or cut short.
//! - [`tool_input_cases`]: streams of the second kind, often cut short, for
//!   FinalizeToolInput.
//! - [`to_interactions_cases`]: OpenAI Responses event streams and whole
//!   responses for an Interactions client: text, reasoning and function
//!   calls, their deltas sent, or left for the finished item or the
//!   completed response to carry, with or without the keys that tie a delta
//!   to its item, and every way a stream can end.
//!
//! Inputs the port reads differently by design (see its module docs) are
//! left out: events and bodies that aren't valid JSON, and `apply_patch`
//! input with an unpaired surrogate escape. So are token counts out of
//! `i64`'s range, which Go converts by the CPU's rules: the comparison
//! allows for a count saturated where Go's wrapped, but not for a total
//! made from one. A whole
//! Interactions response may write a call's arguments object with spaces
//! or escapes, where the port writes it compactly; the suite allows for
//! that.

use serde_json::{Map, Value, json};

use crate::cases::Case;
use crate::generate::interactions::{
    End, Interaction, MODELS, Step, TOOL_NAMES, pieces, render, request, rng, text, token_count,
};
use crate::generate::{Rng, escape_text, to_object};

/// Models the translators are called with: the shared ones, and models of
/// other providers, none naming antigravity.
const STREAM_MODELS: &[&str] = &["devin/swe-2", "gpt-5.1-codex", ""];

/// A patch as `apply_patch` takes it.
const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// `apply_patch` arguments as an upstream may stream them: one `input`
/// string, padded or not, and arguments the bridge rejects.
const PATCH_ARGUMENTS: &[&str] = &[
    r#"{"input":"*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch"}"#,
    r#"{"input":"*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch\n"}"#,
    "{\"input\":\"  *** Begin Patch\\n*** Add File: 中.txt\\n+😀\\n*** End Patch\\n \"}",
    r#"{"input":"quote \" backslash \\ tab \t slash \/"}"#,
    r#" { "input" : "p" } "#,
    r#"{"input":""}"#,
    r#"{"input":5}"#,
    r#"{"input":null}"#,
    "{}",
    r#""text""#,
    r#"{"input":"p","extra":1}"#,
    r#"{"input":"p"}{"input":"q"}"#,
    r#"{"input":"cut short"#,
    r#"{"input":"bad \x escape"}"#,
    "",
];

/// Function call arguments as a Responses upstream sends them.
const ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"city":"Paris"}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    r#"{"nested":{"a":[1,2]},"b":null}"#,
    " {\"padded\": true} ",
    "{}",
    "",
    "[1,2]",
    "\"text\"",
    "5",
    "not json",
    r#"{"a":1}{"b":2}"#,
];

/// Namespaces a Responses call may name.
const NAMESPACES: &[&str] = &["mcp__github", "functions", "tools__", " spaced "];

/// Item IDs and call IDs, some reused across items.
const ITEM_IDS: &[&str] = &["item_1", "item_2", "fc_1", "msg_1", "rs_1"];
const CALL_IDS: &[&str] = &["call_1", "call_2", "call_abc"];

/// Responses response statuses.
const RESPONSE_STATUSES: &[&str] = &["completed", "incomplete", "in_progress", "failed", ""];

/// Signatures a reasoning item may carry.
const ENCRYPTED: &[&str] = &["gAAAAABo", "c2lnbmF0dXJl", ""];

/// A token count from the shared generator, unless it is out of `i64`'s
/// range: upstream converts that by the CPU's rules, and a total it makes
/// from one then differs by more than the comparison allows for.
fn count(rng: &mut Rng) -> Value {
    let count = token_count(rng);
    if in_int64_range(&count) {
        count
    } else {
        json!(rng.below(5000))
    }
}

/// Whether `value` isn't a number, or is one in `i64`'s range.
fn in_int64_range(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.as_i64().is_some() || number.as_f64().is_some_and(|f| f.abs() < 9.2e18)
        }
        _ => true,
    }
}

/// Replaces each count in `usage` that is out of `i64`'s range (see
/// [`count`]).
fn counts_in_range(usage: &mut Value) {
    if let Value::Object(fields) = usage {
        for value in fields.values_mut() {
            if !in_int64_range(value) {
                *value = json!(1);
            }
        }
    }
}

/// The tools a client's request declares, and what upstream calls each.
struct Tools {
    declared: Vec<Value>,
    /// The name upstream calls each tool by, and whether it is `apply_patch`.
    names: Vec<(&'static str, bool)>,
}

impl Tools {
    /// Up to four tools, with `apply_patch` among them when `patch`.
    fn random(rng: &mut Rng, patch: bool) -> Self {
        let mut tools = Self {
            declared: Vec::new(),
            names: Vec::new(),
        };
        if patch {
            tools.add(if rng.chance(50) { 2 } else { 3 });
        }
        for _ in 0..rng.below(4) {
            tools.add(rng.below(8));
        }
        tools
    }

    fn add(&mut self, which: usize) {
        let patch_tool = || json!({ "type": "custom", "name": "apply_patch", "format": { "type": "grammar", "syntax": "lark", "definition": "start: patch" } });
        let (tool, name, patch) = match which {
            0 => (
                json!({ "type": "function", "name": "get_weather", "description": "Weather.", "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } }),
                "get_weather",
                false,
            ),
            1 => (
                json!({ "type": "function", "name": "exec_command", "parameters": { "type": "object" } }),
                "exec_command",
                false,
            ),
            2 => (patch_tool(), "apply_patch", true),
            3 => (
                json!({ "type": "namespace", "name": "functions", "tools": [patch_tool()] }),
                "functions__apply_patch",
                true,
            ),
            4 => (
                json!({ "type": "custom", "name": "run_sql", "description": "Runs SQL." }),
                "run_sql",
                false,
            ),
            5 => (
                json!({ "type": "namespace", "name": "mcp__github", "tools": [{ "type": "function", "name": "search_code", "parameters": {} }] }),
                "mcp__github__search_code",
                false,
            ),
            6 => (
                json!({ "type": "namespace", "name": "browser", "tools": [{ "type": "custom", "name": "type_text" }] }),
                "browser__type_text",
                false,
            ),
            _ => (json!({ "type": "web_search" }), "web_search", false),
        };
        self.declared.push(tool);
        self.names.push((name, patch));
    }

    /// The name of a declared `apply_patch`, else `apply_patch` undeclared.
    fn patch_name(&self, rng: &mut Rng) -> &'static str {
        let patches: Vec<&'static str> = self
            .names
            .iter()
            .filter(|(_, patch)| *patch)
            .map(|(name, _)| *name)
            .collect();
        if patches.is_empty() {
            "apply_patch"
        } else {
            rng.pick(&patches)
        }
    }

    /// The name of a declared tool other than `apply_patch`, or one of the
    /// shared names.
    fn other_name(&self, rng: &mut Rng) -> String {
        let others: Vec<&'static str> = self
            .names
            .iter()
            .filter(|(_, patch)| !*patch)
            .map(|(name, _)| *name)
            .collect();
        if others.is_empty() || rng.chance(20) {
            rng.pick(TOOL_NAMES).to_owned()
        } else {
            rng.pick(&others).to_owned()
        }
    }
}

/// The client's request declaring `tools`, and the request as sent
/// upstream: either may be absent, and the client's may be wrapped in a
/// `request` object.
fn requests(rng: &mut Rng, tools: &Tools) -> (String, String) {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(50) {
        fields.push((
            "model",
            rng.pick(&["gpt-5.1-codex", "client-model", ""]).into(),
        ));
    }
    fields.push(("input", text(rng).into()));
    if !tools.declared.is_empty() {
        fields.push(("tools", Value::Array(tools.declared.clone())));
    }
    if rng.chance(20) {
        fields.push(("stream", json!(true)));
    }
    let mut client = to_object(fields);
    if rng.chance(10) {
        client = json!({ "request": client });
    }
    let client = render(rng, &client);
    match rng.below(10) {
        0 => (String::new(), client),
        1 => (String::new(), String::new()),
        2 | 3 => {
            let upstream = request(rng);
            (client, render(rng, &upstream))
        }
        _ => (client, String::new()),
    }
}

/// `count` Interactions streams, each as a stream case and a non-streaming
/// case, for an OpenAI Responses client.
pub fn to_responses_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed ^ 0x52_4553_5030_4E53, index);
            let model = pick_model(&mut rng);
            let patch = rng.chance(60);
            let tools = Tools::random(&mut rng, patch);
            let (request, translated) = requests(&mut rng, &tools);
            let (events, body) = if rng.chance(50) {
                shared(&mut rng, &tools)
            } else {
                let call = PatchCall::random(&mut rng, &tools);
                (call.lines(&mut rng), call.body(&mut rng))
            };
            let name = format!("to-responses-{seed}-{index}");
            let case = |name: String, events| Case {
                model: model.clone(),
                translated_request: translated.clone(),
                ..Case::response(name, request.clone(), events)
            };
            (
                case(format!("{name}-stream"), events),
                case(format!("{name}-final"), vec![body]),
            )
        })
        .unzip()
}

/// `count` Interactions streams around an `apply_patch` call, half of them
/// cut short, for FinalizeToolInput.
pub fn tool_input_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed ^ 0x544F_4F4C_494E_5054, index);
            let model = pick_model(&mut rng);
            let patch = rng.chance(90);
            let tools = Tools::random(&mut rng, patch);
            let (request, translated) = requests(&mut rng, &tools);
            let call = PatchCall::random(&mut rng, &tools);
            let mut lines = call.lines(&mut rng);
            if rng.chance(50) && !lines.is_empty() {
                lines.truncate(1 + rng.below(lines.len()));
            }
            Case {
                model,
                translated_request: translated,
                ..Case::response(format!("tool-input-{seed}-{index}"), request, lines)
            }
        })
        .collect()
}

fn pick_model(rng: &mut Rng) -> String {
    if rng.chance(70) {
        rng.pick(MODELS).to_owned()
    } else {
        rng.pick(STREAM_MODELS).to_owned()
    }
}

/// One of the shared generator's interactions, its calls mostly renamed to
/// the tools the client declared, as a stream and whole.
fn shared(rng: &mut Rng, tools: &Tools) -> (Vec<String>, String) {
    let mut interaction = Interaction::random(rng);
    if let Some(usage) = &mut interaction.usage {
        counts_in_range(usage);
    }
    for step in &mut interaction.steps {
        if let Step::Call {
            name, arguments, ..
        } = step
            && !tools.names.is_empty()
            && rng.chance(75)
        {
            let (upstream, patch) = rng.pick(&tools.names);
            *name = upstream.to_owned();
            if patch {
                *arguments = match rng.below(6) {
                    0..=2 => json!({ "input": PATCH }),
                    3 => json!({ "input": text(rng) }),
                    4 => json!({ "input": 5 }),
                    _ => json!({}),
                };
            }
        }
    }
    let events = interaction.events(rng);
    let body = interaction.body(rng);
    (events, render(rng, &body))
}

/// An interaction around a call to `apply_patch`: maybe a text step before
/// it and an ordinary call after it.
struct PatchCall {
    id: Option<String>,
    model: Option<String>,
    environment: bool,
    /// The text of a step before the call, which is then at index 1.
    before: Option<String>,
    name: &'static str,
    item_id: Option<String>,
    call_id: Option<String>,
    /// The arguments as streamed.
    arguments: String,
    /// Whether `step.start` leaves out the name, which a later delta's
    /// `step` or a snapshot then gives.
    late_name: bool,
    /// An ordinary call after the patch call: its name and arguments.
    after: Option<(String, String)>,
    usage: Option<Value>,
    end: End,
}

impl PatchCall {
    fn random(rng: &mut Rng, tools: &Tools) -> Self {
        let mut arguments = rng.pick(PATCH_ARGUMENTS).to_owned();
        if rng.chance(15) {
            arguments = escape_text(&arguments);
        }
        let end = if rng.chance(85) {
            End::Completed {
                status: rng
                    .pick(&[
                        "completed",
                        "completed",
                        "incomplete",
                        "requires_action",
                        "",
                    ])
                    .to_owned(),
                finish_reason: rng.chance(40).then(|| {
                    rng.pick(&["stop", "length", "tool_calls", "content_filter"])
                        .to_owned()
                }),
            }
        } else {
            End::Failed {
                message: text(rng),
                code: json!(500),
            }
        };
        Self {
            id: rng
                .chance(85)
                .then(|| format!("interaction_{}", rng.below(1000))),
            model: rng.chance(60).then(|| rng.pick(MODELS).to_owned()),
            environment: rng.chance(15),
            before: rng.chance(40).then(|| text(rng)),
            name: tools.patch_name(rng),
            item_id: rng.chance(80).then(|| rng.pick(ITEM_IDS).to_owned()),
            call_id: rng.chance(80).then(|| rng.pick(CALL_IDS).to_owned()),
            arguments,
            late_name: rng.chance(15),
            after: rng
                .chance(30)
                .then(|| (tools.other_name(rng), rng.pick(ARGUMENTS).to_owned())),
            usage: rng.chance(60).then(
                || json!({ "total_input_tokens": count(rng), "total_output_tokens": count(rng) }),
            ),
            end,
        }
    }

    /// The patch call's index.
    fn index(&self) -> usize {
        usize::from(self.before.is_some())
    }

    /// The call as a step: its identity, and its arguments if `arguments`.
    fn step(&self, arguments: bool) -> Value {
        let mut step = Map::new();
        step.insert("index".into(), self.index().into());
        step.insert("type".into(), json!("function_call"));
        if let Some(id) = &self.item_id {
            step.insert("id".into(), id.as_str().into());
        }
        if let Some(id) = &self.call_id {
            step.insert("call_id".into(), id.as_str().into());
        }
        step.insert("name".into(), self.name.into());
        if arguments {
            let value = serde_json::from_str(&self.arguments)
                .unwrap_or_else(|_| Value::String(self.arguments.clone()));
            step.insert("arguments".into(), value);
        }
        Value::Object(step)
    }

    /// The events of the interaction, in order, now and then one left out
    /// or repeated, and the interaction sometimes ended early.
    fn events(&self, rng: &mut Rng) -> Vec<Value> {
        let mut events = Vec::new();
        if rng.chance(90) {
            let mut interaction = Map::new();
            if let Some(id) = &self.id {
                interaction.insert("id".into(), id.as_str().into());
            }
            if let Some(model) = &self.model {
                interaction.insert("model".into(), model.as_str().into());
            }
            if self.environment {
                interaction.insert("environment_id".into(), json!("env_1"));
            }
            events.push(json!({ "event_type": "interaction.created", "interaction": interaction }));
        }
        if let Some(text) = &self.before {
            events.push(json!({ "event_type": "step.start", "index": 0, "step": { "type": "model_output" } }));
            for piece in pieces(rng, text) {
                events.push(json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": piece } }));
            }
            events.push(json!({ "event_type": "step.stop", "index": 0 }));
        }
        let index = self.index();
        let mut start = self.step(rng.chance(20));
        if let Value::Object(fields) = &mut start {
            fields.shift_remove("index");
            if self.late_name {
                fields.shift_remove("name");
            }
            if rng.chance(15) {
                fields.shift_remove("id");
                fields.shift_remove("call_id");
            }
        }
        events.push(json!({ "event_type": "step.start", "index": index, "step": start }));
        for (n, piece) in pieces(rng, &self.arguments).into_iter().enumerate() {
            let mut delta = json!({ "event_type": "step.delta", "index": index, "delta": { "type": "arguments_delta", "arguments": piece } });
            if (self.late_name && n == 0 && rng.chance(50)) || rng.chance(5) {
                delta["step"] = self.step(false);
            }
            events.push(delta);
        }
        if rng.chance(90) {
            events.push(json!({ "event_type": "step.stop", "index": index }));
        }
        let mut snapshot = vec![self.step(true)];
        if let Some((name, arguments)) = &self.after {
            let after = index + 1;
            let id = format!("call_after_{after}");
            events.push(json!({ "event_type": "step.start", "index": after, "step": { "type": "function_call", "id": id, "name": name } }));
            for piece in pieces(rng, arguments) {
                events.push(json!({ "event_type": "step.delta", "index": after, "delta": { "type": "arguments_delta", "arguments": piece } }));
            }
            events.push(json!({ "event_type": "step.stop", "index": after }));
            snapshot.push(json!({ "index": after, "type": "function_call", "id": id, "name": name, "arguments": arguments }));
        }
        match &self.end {
            End::Completed {
                status,
                finish_reason,
            } => match rng.below(10) {
                0 => {}
                1 => events.push(
                    json!({ "event_type": "finish", "metadata": { "total_usage": self.usage } }),
                ),
                _ => {
                    let mut interaction = Map::new();
                    if let Some(id) = &self.id {
                        interaction.insert("id".into(), id.as_str().into());
                    }
                    interaction.insert("status".into(), status.as_str().into());
                    if let Some(reason) = finish_reason {
                        interaction.insert("finish_reason".into(), reason.as_str().into());
                    }
                    if let Some(usage) = &self.usage {
                        interaction.insert("usage".into(), usage.clone());
                    }
                    let mut completed = json!({ "event_type": "interaction.completed" });
                    match rng.below(5) {
                        0 => completed["steps"] = snapshot.into(),
                        1 => {
                            interaction.insert("steps".into(), snapshot.into());
                        }
                        _ => {}
                    }
                    completed["interaction"] = Value::Object(interaction);
                    events.push(completed);
                }
            },
            End::Failed { message, code } => {
                let error = json!({ "message": message, "code": code });
                events.push(if rng.chance(50) {
                    json!({ "event_type": "interaction.failed", "error": error })
                } else {
                    json!({ "event_type": "response.failed", "error": error })
                });
            }
        }
        if rng.chance(30) {
            events.push(json!({ "event_type": "done" }));
        }
        if !events.is_empty() && rng.chance(10) {
            let at = rng.below(events.len());
            if rng.chance(50) {
                events.remove(at);
            } else {
                let event = events[at].clone();
                events.insert(at, event);
            }
        }
        events
    }

    /// The events as lines (see [`lines`]).
    fn lines(&self, rng: &mut Rng) -> Vec<String> {
        let events = self.events(rng);
        lines(rng, &events, "event_type")
    }

    /// The interaction whole, as a non-streaming response.
    fn body(&self, rng: &mut Rng) -> String {
        let (status, finish_reason) = match &self.end {
            End::Failed { message, code } => {
                let error = json!({ "message": message, "code": code });
                let body = if rng.chance(50) {
                    json!({ "error": error })
                } else {
                    json!({ "id": self.id, "status": "failed", "steps": [self.step(true)] })
                };
                return render(rng, &body);
            }
            End::Completed {
                status,
                finish_reason,
            } => (status, finish_reason),
        };
        let mut steps = Vec::new();
        if let Some(text) = &self.before {
            steps.push(
                json!({ "type": "model_output", "content": [{ "type": "text", "text": text }] }),
            );
        }
        let mut call = self.step(true);
        if let Value::Object(fields) = &mut call {
            fields.shift_remove("index");
            if self.late_name && rng.chance(50) {
                fields.insert("name".into(), json!(""));
            }
        }
        steps.push(call);
        if let Some((name, arguments)) = &self.after {
            steps.push(json!({ "type": "function_call", "id": "call_after", "name": name, "arguments": arguments }));
        }
        let mut fields: Vec<(&str, Value)> = Vec::new();
        if let Some(id) = &self.id {
            fields.push(("id", id.as_str().into()));
        }
        if let Some(model) = &self.model {
            fields.push(("model", model.as_str().into()));
        }
        fields.push(("status", status.as_str().into()));
        if let Some(reason) = finish_reason {
            fields.push(("finish_reason", reason.as_str().into()));
        }
        if self.environment {
            fields.push(("environment_id", json!("env_1")));
        }
        fields.push(("steps", steps.into()));
        if let Some(usage) = &self.usage {
            fields.push(("usage", usage.clone()));
        }
        let body = to_object(fields);
        let body = if rng.chance(20) {
            json!({ "interaction": body })
        } else {
            body
        };
        render(rng, &body)
    }
}

/// Events as lines, as an executor passes them to a translator: mostly
/// each event's JSON, sometimes as a `data:` line or a whole SSE frame
/// named by the event's `type_key`, and sometimes `[DONE]` after them.
fn lines(rng: &mut Rng, events: &[Value], type_key: &str) -> Vec<String> {
    let mut lines: Vec<String> = events
        .iter()
        .map(|event| match rng.below(10) {
            0..=2 => format!("data: {event}"),
            3 => format!(
                "event: {}\ndata: {event}",
                event[type_key].as_str().unwrap_or("")
            ),
            _ => event.to_string(),
        })
        .collect();
    if rng.chance(40) {
        lines.push(rng.pick(&["[DONE]", "data: [DONE]"]).to_owned());
    }
    lines
}

/// `count` OpenAI Responses streams, each as a stream case and a
/// non-streaming case, for an Interactions client.
pub fn to_interactions_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut rng = rng(seed ^ 0x544F_494E_5445_5241, index);
            let model = if rng.chance(50) {
                String::new()
            } else {
                pick_model(&mut rng)
            };
            let client = request(&mut rng);
            let client = render(&mut rng, &client);
            let answer = Answer::random(&mut rng);
            let events = answer.events(&mut rng);
            let events = lines(&mut rng, &events, "type");
            let body = answer.body(&mut rng);
            let body = render(&mut rng, &body);
            let name = format!("to-interactions-{seed}-{index}");
            let case = |name: String, events| Case {
                model: model.clone(),
                ..Case::response(name, client.clone(), events)
            };
            (
                case(format!("{name}-stream"), events),
                case(format!("{name}-final"), vec![body]),
            )
        })
        .unzip()
}

/// One output item of a Responses answer.
enum Output {
    Message {
        id: Option<String>,
        parts: Vec<Value>,
    },
    Reasoning {
        id: Option<String>,
        summaries: Vec<String>,
        encrypted: Option<String>,
    },
    Call {
        id: Option<String>,
        call_id: Option<String>,
        name: String,
        namespace: Option<String>,
        arguments: String,
    },
    /// An item of a type the translators skip.
    Other(Value),
}

impl Output {
    fn random(rng: &mut Rng) -> Self {
        let id = rng.chance(85).then(|| rng.pick(ITEM_IDS).to_owned());
        match rng.below(10) {
            0..=3 => Self::Message {
                id,
                parts: (0..1 + rng.below(2))
                    .map(|_| match rng.below(10) {
                        0..=6 => json!({ "type": "output_text", "text": text(rng), "annotations": [] }),
                        7 => json!({ "type": "text", "text": text(rng) }),
                        8 => rng.pick(&[
                            json!({ "type": "output_image", "image_url": "data:image/png;base64,aGVsbG8=" }),
                            json!({ "type": "output_image", "image_url": "https://example.com/a.png" }),
                            json!({ "type": "output_image", "data": "aGVsbG8=", "mime_type": "image/jpeg" }),
                        ]),
                        _ => json!({ "type": "refusal", "refusal": text(rng) }),
                    })
                    .collect(),
            },
            4 | 5 => Self::Reasoning {
                id,
                summaries: (0..rng.below(3)).map(|_| text(rng)).collect(),
                encrypted: rng.chance(40).then(|| rng.pick(ENCRYPTED).to_owned()),
            },
            6..=8 => Self::Call {
                id,
                call_id: rng.chance(85).then(|| rng.pick(CALL_IDS).to_owned()),
                name: rng.pick(TOOL_NAMES).to_owned(),
                namespace: rng.chance(20).then(|| rng.pick(NAMESPACES).to_owned()),
                arguments: rng.pick(ARGUMENTS).to_owned(),
            },
            _ => Self::Other(rng.pick(&[
                json!({ "type": "web_search_call", "id": "ws_1", "status": "completed" }),
                json!({ "type": "custom_tool_call", "call_id": "call_c", "name": "run_sql", "input": "SELECT 1" }),
            ])),
        }
    }

    /// The item as `response.output_item.added` gives it.
    fn added(&self) -> Value {
        match self {
            Self::Message { id, .. } => {
                json!({ "id": id, "type": "message", "role": "assistant", "status": "in_progress", "content": [] })
            }
            Self::Reasoning { id, .. } => json!({ "id": id, "type": "reasoning", "summary": [] }),
            Self::Call {
                id,
                call_id,
                name,
                namespace,
                ..
            } => {
                let mut item = json!({ "id": id, "type": "function_call", "call_id": call_id, "name": name, "arguments": "" });
                if let Some(namespace) = namespace {
                    item["namespace"] = namespace.as_str().into();
                }
                item
            }
            Self::Other(item) => item.clone(),
        }
    }

    /// The finished item.
    fn done(&self) -> Value {
        match self {
            Self::Message { id, parts } => {
                json!({ "id": id, "type": "message", "role": "assistant", "status": "completed", "content": parts })
            }
            Self::Reasoning {
                id,
                summaries,
                encrypted,
            } => {
                let summary: Vec<Value> = summaries
                    .iter()
                    .map(|text| json!({ "type": "summary_text", "text": text }))
                    .collect();
                let mut item = json!({ "id": id, "type": "reasoning", "summary": summary });
                if let Some(encrypted) = encrypted {
                    item["encrypted_content"] = encrypted.as_str().into();
                }
                item
            }
            Self::Call { arguments, .. } => {
                let mut item = self.added();
                item["arguments"] = arguments.as_str().into();
                item["status"] = json!("completed");
                item
            }
            Self::Other(item) => item.clone(),
        }
    }
}

/// How a Responses stream ends.
#[derive(Clone, Copy)]
enum Ending {
    Completed,
    Incomplete,
    Failed,
    /// No final event.
    None,
}

/// A Responses answer, to give as an event stream or whole.
struct Answer {
    id: Option<String>,
    model: Option<String>,
    status: String,
    outputs: Vec<Output>,
    usage: Option<Value>,
    ending: Ending,
}

impl Answer {
    fn random(rng: &mut Rng) -> Self {
        let usage = rng.chance(70).then(|| {
            let mut fields: Vec<(&str, Value)> = Vec::new();
            for key in ["input_tokens", "output_tokens", "total_tokens"] {
                if rng.chance(80) {
                    fields.push((key, count(rng)));
                }
            }
            if rng.chance(40) {
                fields.push((
                    "input_tokens_details",
                    json!({ "cached_tokens": count(rng) }),
                ));
            }
            if rng.chance(40) {
                fields.push((
                    "output_tokens_details",
                    json!({ "reasoning_tokens": count(rng) }),
                ));
            }
            to_object(fields)
        });
        Self {
            id: rng.chance(80).then(|| format!("resp_{}", rng.below(1000))),
            model: rng.chance(80).then(|| pick_model(rng)),
            status: rng.pick(RESPONSE_STATUSES).to_owned(),
            outputs: (0..rng.below(5)).map(|_| Output::random(rng)).collect(),
            usage,
            ending: rng.pick(&[
                Ending::Completed,
                Ending::Completed,
                Ending::Completed,
                Ending::Completed,
                Ending::Completed,
                Ending::Completed,
                Ending::Incomplete,
                Ending::Failed,
                Ending::None,
            ]),
        }
    }

    /// The response object, with the finished items if `output`.
    fn response(&self, status: &str, output: bool) -> Value {
        let mut fields: Vec<(&str, Value)> = Vec::new();
        if let Some(id) = &self.id {
            fields.push(("id", id.as_str().into()));
        }
        fields.push(("object", json!("response")));
        fields.push(("status", status.into()));
        if let Some(model) = &self.model {
            fields.push(("model", model.as_str().into()));
        }
        let items: Vec<Value> = if output {
            self.outputs.iter().map(Output::done).collect()
        } else {
            Vec::new()
        };
        fields.push(("output", items.into()));
        if output && let Some(usage) = &self.usage {
            fields.push(("usage", usage.clone()));
        }
        to_object(fields)
    }

    /// The answer as events. Deltas may be left out, for the finished item
    /// or the completed response to carry, and the keys tying a delta to
    /// its item may be missing. Now and then an event is left out or
    /// repeated.
    fn events(&self, rng: &mut Rng) -> Vec<Value> {
        let mut events = Vec::new();
        let mut sequence = 0;
        let mut push = |events: &mut Vec<Value>, mut event: Value| {
            if let Value::Object(fields) = &mut event {
                fields.insert("sequence_number".into(), sequence.into());
            }
            sequence += 1;
            events.push(event);
        };
        if rng.chance(85) {
            push(
                &mut events,
                json!({ "type": "response.created", "response": self.response("in_progress", false) }),
            );
        }
        if rng.chance(20) {
            push(
                &mut events,
                json!({ "type": "response.in_progress", "response": self.response("in_progress", false) }),
            );
        }
        for (index, output) in self.outputs.iter().enumerate() {
            let keyed = rng.chance(85);
            let keys = |event: &mut Value, id: &Option<String>| {
                if keyed {
                    event["output_index"] = index.into();
                    if let Some(id) = id {
                        event["item_id"] = id.as_str().into();
                    }
                }
            };
            if rng.chance(85) {
                push(
                    &mut events,
                    json!({ "type": "response.output_item.added", "output_index": index, "item": output.added() }),
                );
            }
            let send_deltas = rng.chance(75);
            match output {
                Output::Message { id, parts } if send_deltas => {
                    for (content_index, part) in parts.iter().enumerate() {
                        let Some(text) = part.get("text").and_then(Value::as_str) else {
                            continue;
                        };
                        for piece in pieces(rng, text) {
                            let mut event =
                                json!({ "type": "response.output_text.delta", "delta": piece });
                            keys(&mut event, id);
                            if keyed && rng.chance(90) {
                                event["content_index"] = content_index.into();
                            }
                            push(&mut events, event);
                        }
                        if rng.chance(30) {
                            let mut event =
                                json!({ "type": "response.output_text.done", "text": text });
                            keys(&mut event, id);
                            push(&mut events, event);
                        }
                    }
                }
                Output::Reasoning { id, summaries, .. } if send_deltas => {
                    for (summary_index, summary) in summaries.iter().enumerate() {
                        for piece in pieces(rng, summary) {
                            let mut event = json!({ "type": "response.reasoning_summary_text.delta", "summary_index": summary_index, "delta": piece });
                            keys(&mut event, id);
                            push(&mut events, event);
                        }
                    }
                }
                Output::Call { id, arguments, .. } if send_deltas => {
                    for piece in pieces(rng, arguments) {
                        let mut event = json!({ "type": "response.function_call_arguments.delta", "delta": piece });
                        keys(&mut event, id);
                        push(&mut events, event);
                    }
                }
                _ => {}
            }
            if rng.chance(80) {
                push(
                    &mut events,
                    json!({ "type": "response.output_item.done", "output_index": index, "item": output.done() }),
                );
            }
        }
        let output = rng.chance(60);
        match self.ending {
            Ending::Completed => push(
                &mut events,
                json!({ "type": "response.completed", "response": self.response(&self.status, output) }),
            ),
            Ending::Incomplete => push(
                &mut events,
                json!({ "type": "response.incomplete", "response": self.response("incomplete", output) }),
            ),
            Ending::Failed => push(
                &mut events,
                json!({ "type": "response.failed", "response": { "status": "failed", "error": { "code": "server_error", "message": "boom" } } }),
            ),
            Ending::None => {}
        }
        if !events.is_empty() && rng.chance(10) {
            let at = rng.below(events.len());
            if rng.chance(50) {
                events.remove(at);
            } else {
                let event = events[at].clone();
                events.insert(at, event);
            }
        }
        events
    }

    /// The answer whole, as a non-streaming response.
    fn body(&self, rng: &mut Rng) -> Value {
        let status = match self.ending {
            Ending::Incomplete => "incomplete",
            _ => &self.status,
        };
        let body = self.response(status, true);
        if rng.chance(5) {
            json!({ "response": body })
        } else {
            body
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(line: &str) -> &str {
        match line.strip_prefix("event: ") {
            Some(frame) => frame.split_once("\ndata: ").map_or("", |(_, data)| data),
            None => line.strip_prefix("data: ").unwrap_or(line),
        }
    }

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let (streams, finals) = to_responses_cases(3, 200);
        let (again, _) = to_responses_cases(3, 200);
        assert_eq!((streams.len(), finals.len()), (200, 200));
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!((&a.request, &a.events), (&b.request, &b.events));
        }
        let (back, back_finals) = to_interactions_cases(3, 200);
        let tool_input = tool_input_cases(3, 200);
        let lines = streams.iter().chain(&back).chain(&tool_input);
        for line in lines.flat_map(|case| &case.events) {
            let payload = payload(line);
            if payload != "[DONE]" {
                serde_json::from_str::<Value>(payload).expect("an event is JSON");
            }
        }
        for body in finals
            .iter()
            .chain(&back_finals)
            .flat_map(|case| &case.events)
        {
            serde_json::from_str::<Value>(body).expect("a response is JSON");
        }
    }

    #[test]
    fn no_model_names_antigravity() {
        let (streams, finals) = to_responses_cases(9, 100);
        let (back, back_finals) = to_interactions_cases(9, 100);
        let cases = streams
            .into_iter()
            .chain(finals)
            .chain(back)
            .chain(back_finals)
            .chain(tool_input_cases(9, 100));
        for case in cases {
            let texts = [case.model, case.request, case.translated_request];
            for text in texts.into_iter().chain(case.events) {
                assert!(!text.to_lowercase().contains("antigravity"), "{text}");
            }
        }
    }

    #[test]
    fn patch_calls_are_declared_and_streamed() {
        let patched = tool_input_cases(5, 100)
            .iter()
            .filter(|case| case.request.contains("apply_patch"))
            .filter(|case| {
                case.events
                    .iter()
                    .any(|line| line.contains("arguments_delta"))
            })
            .count();
        assert!(patched > 50, "{patched}");
    }
}
