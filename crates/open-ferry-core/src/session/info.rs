// Ported from CLIProxyAPI sdk/cliproxy/session/info.go (SessionInfo,
// sessionObject, ExtractSessionInfo, isBodyForkCandidate,
// BoundSessionIdentity, finalizeSessionInfo, sessionHeaderValue,
// normalizedSessionCandidate) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The session a client named, and the parent it came from.
//!
//! [`extract_session_info`] tries, in order: Claude Code's session header,
//! the session in Claude Code's `metadata.user_id`, Codex's session and
//! thread headers and turn metadata, the generic, OpenCode, Pi, task,
//! conversation, thread and client request headers, then the body's
//! Gemini cached content, thread, session, task, prompt cache key,
//! conversation, user and legacy conversation fields, and last the
//! connection's execution session. The first that names a session wins,
//! with a prefix for where it came from (`claude:`, `codex:`, `header:`
//! and so on); a parent named in the headers or anywhere in the body joins
//! it under the same prefix.
//!
//! Deviations from upstream:
//! - Antigravity's `X-Http-Session-Id` branch is left out (out of scope),
//!   as are the LCP sessions, which only the LCP matcher makes.
//! - The connection's execution session is passed in, where upstream reads
//!   it from the call's metadata; there is no caller scope.
//! - Headers are read through [`HeaderMap`], whose names are already
//!   lowercase, so upstream's lookups of one name in two cases are one.

use http::HeaderMap;
use open_ferry_translate::go;

use super::identity::{ClaudeIdentities, normalize_explicit_id};
use super::payload::Payload;
use crate::auth::synthesizer::sha256_hex;
use crate::observe::usage::json::Node;

/// What a request says of its session (upstream's `SessionInfo`, the part
/// session affinity reads).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionInfo {
    /// The session, with the prefix of where it came from, such as
    /// `claude:` or `codex:`; at most 256 bytes.
    pub session_id: String,
    /// The session it came from, under the same prefix, or empty.
    pub parent_session_id: String,
    /// `main`, `subagent`, `slot` or the agent the client named.
    pub agent_name: String,
    /// The client it came from, such as `claude`, `codex` or `generic`.
    pub client_type: String,
    /// Whether it forked from its parent.
    pub is_fork: bool,
    /// Whether it is a subagent of its parent.
    pub is_subagent: bool,
}

/// The parent keys a body may name, in the order they are read (upstream's
/// list in `ExtractSessionInfo`).
const PARENT_PATHS: &[&str] = &[
    // Standard session / thread parent keys
    "parent_session_id",
    "parentSessionId",
    "parentSessionID",
    "parent_thread_id",
    "parentThreadId",
    "parentThreadID",
    "forked_from_thread_id",
    "forked_from_id",
    "parent_conversation_id",
    "parentConversationId",
    "parentConversationID",
    // OpenCode / generic parent ID keys
    "parent_id",
    "parentId",
    "parentID",
    // Roo Code / Cline task delegation keys
    "parent_task_id",
    "parentTaskId",
    "parentTaskID",
    // OpenHands action tree keys
    "parent_action_id",
    "parentActionId",
    "parentActionID",
    // Pi session keys
    "parent_session",
    "parentSession",
    // Hermes subagent keys
    "parent_subagent_id",
    "parentSubagentId",
    // OpenClaw fork sources
    "forkSource.sessionId",
    "fork_source.session_id",
    "previousSessionId",
    "previous_session_id",
    // Metadata nested keys
    "metadata.parent_session_id",
    "metadata.parentSessionId",
    "metadata.parentSessionID",
    "metadata.parent_thread_id",
    "metadata.parentThreadId",
    "metadata.forked_from_thread_id",
    "metadata.forked_from_id",
    "metadata.parent_id",
    "metadata.parentId",
    "metadata.parentID",
    "metadata.parent_task_id",
    "metadata.parentTaskId",
    "metadata.parentTaskID",
    "metadata.parent_action_id",
    "metadata.parentActionId",
    "metadata.parent_subagent_id",
    "metadata.parentSubagentId",
    "metadata.parent_session",
    "metadata.parentSession",
    "metadata.parent_agent_id",
    "metadata.parentAgentId",
    "metadata.forkSource.sessionId",
    "metadata.previousSessionId",
    // Extra body nested keys
    "extra_body.parent_session_id",
    "extra_body.parentSessionId",
    "extra_body.parentSessionID",
    "extra_body.parent_thread_id",
    "extra_body.parentThreadId",
    "extra_body.forked_from_thread_id",
    "extra_body.forked_from_id",
    "extra_body.parent_id",
    "extra_body.parentId",
    "extra_body.parentID",
    "extra_body.parent_task_id",
    "extra_body.parentTaskId",
    "extra_body.parent_action_id",
    "extra_body.parentActionId",
    "extra_body.parent_subagent_id",
    "extra_body.parentSubagentId",
    "extra_body.parent_session",
    "extra_body.parentSession",
];

/// The keys that make a body's parent a fork rather than a subagent
/// (upstream's `isBodyForkCandidate`).
const FORK_CANDIDATE_PATHS: &[&str] = &[
    "forked_from_thread_id",
    "forked_from_id",
    "forkSource.sessionId",
    "fork_source.session_id",
    "previousSessionId",
    "previous_session_id",
    "metadata.forked_from_thread_id",
    "metadata.forked_from_id",
    "metadata.forkSource.sessionId",
    "metadata.previousSessionId",
    "extra_body.forked_from_thread_id",
    "extra_body.forked_from_id",
    "extra_body.forkSource.sessionId",
    "extra_body.previousSessionId",
];

/// The keys a Codex request's body may fork from.
const CODEX_FORK_PATHS: &[&str] = &[
    "forked_from_thread_id",
    "forked_from_id",
    "metadata.forked_from_thread_id",
    "metadata.forked_from_id",
    "extra_body.forked_from_thread_id",
    "extra_body.forked_from_id",
];

/// The keys of a thread in a body.
const THREAD_PATHS: &[&str] = &["thread_id", "threadId", "metadata.thread_id"];

/// The keys of a session in a body.
const SESSION_PATHS: &[&str] = &[
    "session_id",
    "sessionId",
    "sessionID",
    "child_session_id",
    "childSessionId",
    "metadata.session_id",
    "metadata.sessionId",
    "metadata.sessionID",
    "metadata.child_session_id",
    "extra_body.session_id",
    "extra_body.sessionId",
    "extra_body.sessionID",
];

/// The keys of a task or action in a body (Roo Code, Cline, OpenHands).
const TASK_PATHS: &[&str] = &[
    "task_id",
    "taskId",
    "taskID",
    "action_id",
    "actionId",
    "actionID",
    "metadata.task_id",
    "metadata.taskId",
    "metadata.taskID",
    "metadata.action_id",
    "metadata.actionId",
    "metadata.actionID",
    "extra_body.task_id",
    "extra_body.taskId",
    "extra_body.taskID",
];

/// The legacy keys of a conversation in a body.
const CONVERSATION_PATHS: &[&str] = &[
    "conversation_id",
    "conversationId",
    "chat_id",
    "chatId",
    "metadata.conversation_id",
    "extra_body.conversation_id",
];

/// The keys of an agent in a body's metadata.
const AGENT_PATHS: &[&str] = &["metadata.agent_id", "metadata.subagent_id"];

/// The keys of a parent agent in a body's metadata.
const PARENT_AGENT_PATHS: &[&str] = &["metadata.parent_agent_id", "metadata.parentAgentId"];

/// The longest session ID [`bound_session_identity`] leaves as it is.
const MAX_BOUND_LENGTH: usize = 256;

/// The body's root and, when it nests its request under `request` and has
/// no `contents` of its own, that request (upstream's `root`, `reqRoot` and
/// `hasNestedReq`).
#[derive(Clone, Copy)]
pub(crate) struct Roots<'a> {
    pub(crate) root: Node<'a>,
    pub(crate) nested: Option<Node<'a>>,
}

impl<'a> Roots<'a> {
    /// The roots of `payload`, read as gjson's `Parse` reads them.
    pub(crate) fn of(payload: &'a Payload) -> Self {
        let root = payload.root();
        let request = root.get("request");
        let nested = (request.exists() && !root.get("contents").exists()).then_some(request);
        Self { root, nested }
    }

    /// The first of `paths` that holds an ID, each tried in the root and
    /// then in the nested request.
    pub(crate) fn first_by_path(self, paths: &[&str]) -> String {
        paths
            .iter()
            .find_map(|path| {
                let id = candidate(self.root.get(path));
                if !id.is_empty() {
                    return Some(id);
                }
                let id = self.nested.map(|nested| candidate(nested.get(path)))?;
                (!id.is_empty()).then_some(id)
            })
            .unwrap_or_default()
    }

    /// The first of `paths` that holds an ID in the root, else the first in
    /// the nested request.
    fn first_by_root(self, paths: &[&str]) -> String {
        let id = first_in(self.root, paths);
        if !id.is_empty() {
            return id;
        }
        self.nested
            .map(|nested| first_in(nested, paths))
            .unwrap_or_default()
    }

    /// The conversation the body names: the root's, else the nested
    /// request's.
    pub(crate) fn conversation(self) -> Node<'a> {
        let conversation = self.root.get("conversation");
        match self.nested {
            Some(nested) if !conversation.exists() => nested.get("conversation"),
            _ => conversation,
        }
    }
}

/// The first of `paths` in `node` that holds an ID.
pub(crate) fn first_in(node: Node<'_>, paths: &[&str]) -> String {
    paths
        .iter()
        .map(|path| candidate(node.get(path)))
        .find(|id| !id.is_empty())
        .unwrap_or_default()
}

/// A node's text as an explicit ID (upstream's `normalizedSessionCandidate`
/// of a `Result.String()`).
pub(crate) fn candidate(node: Node<'_>) -> String {
    normalize_explicit_id(&node.string())
}

/// The first value of the header `name` that is an explicit ID (upstream's
/// `sessionHeaderValue`).
pub(crate) fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .map(|value| normalize_explicit_id(&String::from_utf8_lossy(value.as_bytes())))
        .find(|id| !id.is_empty())
        .unwrap_or_default()
}

/// The first of `names` that holds an ID.
fn first_header(headers: &HeaderMap, names: &[&str]) -> String {
    names
        .iter()
        .map(|name| header(headers, name))
        .find(|id| !id.is_empty())
        .unwrap_or_default()
}

/// The session the request names, with its parent and client (upstream's
/// `ExtractSessionInfo`). `execution_id` is the connection's execution
/// session, such as a Responses WebSocket's, or empty.
pub fn extract_session_info(
    headers: &HeaderMap,
    payload: &Payload,
    execution_id: &str,
) -> Option<SessionInfo> {
    let roots = Roots::of(payload);
    let claude = ClaudeIdentities::of(roots);
    let mut parent_candidate = roots.first_by_path(PARENT_PATHS);
    if parent_candidate.is_empty() {
        parent_candidate.clone_from(&claude.parent);
    }
    let reader = Reader {
        headers,
        roots,
        claude: &claude,
        parent_candidate: &parent_candidate,
    };
    reader
        .claude_header()
        .or_else(|| reader.claude_metadata())
        .or_else(|| reader.codex())
        .or_else(|| reader.headers())
        .or_else(|| reader.body())
        .or_else(|| {
            let id = normalize_explicit_id(execution_id);
            (!id.is_empty()).then(|| SessionInfo {
                client_type: "generic".to_owned(),
                session_id: format!("execution:{id}"),
                agent_name: "main".to_owned(),
                ..SessionInfo::default()
            })
        })
        .and_then(finalize)
}

/// A session header, with the client it names, the prefix of its
/// session, the headers naming its parent, and the agent of a session
/// without one.
struct HeaderSession {
    names: &'static [&'static str],
    client: &'static str,
    prefix: &'static str,
    parents: &'static [&'static str],
    agent: &'static str,
}

/// One request's reads, in upstream's order.
struct Reader<'a> {
    headers: &'a HeaderMap,
    roots: Roots<'a>,
    claude: &'a ClaudeIdentities,
    /// The parent the body names (upstream's `parentCandidate`).
    parent_candidate: &'a str,
}

impl Reader<'_> {
    /// 1. Claude Code's session header.
    fn claude_header(&self) -> Option<SessionInfo> {
        let sid = header(self.headers, "x-claude-code-session-id");
        if sid.is_empty() {
            return None;
        }
        let mut agent = header(self.headers, "x-claude-code-agent-id");
        if agent.is_empty() {
            agent = self.roots.first_by_root(AGENT_PATHS);
        }
        if agent.is_empty() {
            agent.clone_from(&self.claude.agent);
        }
        let parent_agent = self.parent_agent();
        let pc = self.parent_candidate;
        let mut info = SessionInfo {
            client_type: "claude".to_owned(),
            ..SessionInfo::default()
        };
        if !agent.is_empty() && agent != "main" {
            info.parent_session_id = format!("claude:{sid}");
            if !parent_agent.is_empty() && parent_agent != "main" && parent_agent != agent {
                info.parent_session_id = format!("claude:{sid}:agent:{parent_agent}");
            } else if !pc.is_empty() && pc != sid {
                info.parent_session_id = format!("claude:{pc}");
            }
            info.session_id = format!("claude:{sid}:agent:{agent}");
            info.agent_name = agent;
        } else {
            info.agent_name = "main".to_owned();
            info.session_id = format!("claude:{sid}");
            if !pc.is_empty() && pc != sid {
                info.parent_session_id = format!("claude:{pc}");
                info.agent_name = "subagent".to_owned();
            }
        }
        Some(info)
    }

    /// 2. The session in Claude Code's `metadata.user_id`, which outranks
    ///    the generic headers.
    fn claude_metadata(&self) -> Option<SessionInfo> {
        let sid = &self.claude.session;
        if sid.is_empty() {
            return None;
        }
        let parent_sid = &self.claude.parent;
        let mut agent = self.claude.agent.clone();
        if agent.is_empty() {
            agent = header(self.headers, "x-claude-code-agent-id");
        }
        if agent.is_empty() {
            agent = self.roots.first_by_root(AGENT_PATHS);
        }
        let parent_agent = self.parent_agent();
        let pc = self.parent_candidate;
        let mut info = SessionInfo {
            client_type: "claude".to_owned(),
            ..SessionInfo::default()
        };
        if !agent.is_empty() && agent != "main" {
            info.session_id = format!("claude:{sid}:agent:{agent}");
            info.parent_session_id = format!("claude:{sid}");
            if !parent_agent.is_empty() && parent_agent != "main" && parent_agent != agent {
                info.parent_session_id = format!("claude:{sid}:agent:{parent_agent}");
            } else if !parent_sid.is_empty() && parent_sid != sid {
                info.parent_session_id = format!("claude:{parent_sid}");
            } else if !pc.is_empty() && pc != sid {
                info.parent_session_id = format!("claude:{pc}");
            }
            info.agent_name = agent;
        } else {
            info.session_id = format!("claude:{sid}");
            if !parent_sid.is_empty() && parent_sid != sid {
                info.parent_session_id = format!("claude:{parent_sid}");
                info.agent_name = "subagent".to_owned();
            } else if !pc.is_empty() && pc != sid {
                info.parent_session_id = format!("claude:{pc}");
                info.agent_name = "subagent".to_owned();
            } else {
                info.agent_name = "main".to_owned();
            }
        }
        Some(info)
    }

    /// Claude Code's parent agent: its header, else the body's metadata.
    fn parent_agent(&self) -> String {
        let parent_agent = header(self.headers, "x-claude-code-parent-agent-id");
        if !parent_agent.is_empty() {
            return parent_agent;
        }
        self.roots.first_by_root(PARENT_AGENT_PATHS)
    }

    /// 3. Codex's session and thread headers and turn metadata.
    fn codex(&self) -> Option<SessionInfo> {
        let headers = self.headers;
        let mut sid = first_header(headers, &["session-id", "session_id"]);
        let mut tid = first_header(headers, &["thread-id", "thread_id"]);
        let turn_text = headers
            .get("x-codex-turn-metadata")
            .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
            .unwrap_or_default();
        let turn_payload = Payload::parse(turn_text.as_bytes());
        let turn = turn_payload.root();
        if sid.is_empty() {
            sid = candidate(turn.get("session_id"));
        }
        if tid.is_empty() {
            tid = candidate(turn.get("thread_id"));
        }
        if tid.is_empty() && !sid.is_empty() {
            tid = self.roots.first_by_path(THREAD_PATHS);
        }
        if sid.is_empty() && tid.is_empty() {
            return None;
        }
        let pc = self.parent_candidate;
        let mut info = SessionInfo {
            client_type: "codex".to_owned(),
            ..SessionInfo::default()
        };

        let mut parent_thread = header(headers, "x-codex-parent-thread-id");
        if parent_thread.is_empty() {
            parent_thread = candidate(turn.get("parent_thread_id"));
        }
        let mut forked_from = candidate(turn.get("forked_from_thread_id"));
        if forked_from.is_empty() {
            forked_from = candidate(turn.get("forked_from_id"));
        }
        if forked_from.is_empty() {
            forked_from = self.roots.first_by_path(CODEX_FORK_PATHS);
        }
        let agent_name = turn.get("agent_name").string();
        let agent_name = agent_name.strip_prefix("/root/").unwrap_or(&agent_name);
        let agent_name = agent_name.strip_prefix('/').unwrap_or(agent_name);
        let agent_name = normalize_explicit_id(agent_name.trim());
        let clean_agent = if agent_name == "root" || agent_name == "main" {
            String::new()
        } else {
            agent_name
        };
        let subagent_header = header(headers, "x-openai-subagent");
        let subagent_signal = (!subagent_header.is_empty()
            && !go::equal_fold(&subagent_header, "false")
            && subagent_header != "0")
            || turn.get("subagent_kind").string() == "thread_spawn";

        // Fork detection.
        if !forked_from.is_empty() {
            let mut fork_session = if tid.is_empty() { &sid } else { &tid };
            if *fork_session == forked_from && !sid.is_empty() && sid != forked_from {
                fork_session = &sid;
            }
            info.session_id = format!("codex:{fork_session}");
            info.parent_session_id = format!("codex:{forked_from}");
            info.agent_name = "main".to_owned();
            info.is_fork = true;
            return Some(info);
        }

        // Subagent detection (Multi-Agent v2).
        if subagent_signal
            || (!tid.is_empty() && !sid.is_empty() && tid != sid)
            || (!parent_thread.is_empty() && parent_thread != tid && parent_thread != sid)
        {
            let child = if tid.is_empty() { &sid } else { &tid };
            let parent_sid = if parent_thread.is_empty() {
                &sid
            } else {
                &parent_thread
            };
            if !clean_agent.is_empty() && !sid.is_empty() {
                info.session_id = format!("codex:{sid}:agent:{clean_agent}");
                if !parent_sid.is_empty() {
                    info.parent_session_id = format!("codex:{parent_sid}");
                } else if !pc.is_empty() && pc != sid {
                    info.parent_session_id = format!("codex:{pc}");
                }
                info.agent_name = clean_agent;
            } else {
                info.session_id = format!("codex:{child}");
                info.agent_name = if clean_agent.is_empty() {
                    "subagent".to_owned()
                } else {
                    clean_agent
                };
                if !parent_sid.is_empty() && parent_sid != child {
                    info.parent_session_id = format!("codex:{parent_sid}");
                } else if !pc.is_empty() && pc != child {
                    info.parent_session_id = format!("codex:{pc}");
                }
            }
            info.is_subagent = true;
            return Some(info);
        }

        // Normal interactive session.
        let session = if sid.is_empty() { &tid } else { &sid };
        info.session_id = format!("codex:{session}");
        if !parent_thread.is_empty() && parent_thread != *session {
            info.parent_session_id = format!("codex:{parent_thread}");
            info.agent_name = "subagent".to_owned();
            info.is_subagent = true;
        } else if !pc.is_empty() && pc != session {
            info.parent_session_id = format!("codex:{pc}");
            info.agent_name = "subagent".to_owned();
            info.is_subagent = true;
        } else {
            info.agent_name = "main".to_owned();
        }
        Some(info)
    }

    /// 5. The OpenCode, Pi slot, task, conversation, thread and generic
    ///    headers.
    fn headers(&self) -> Option<SessionInfo> {
        const HEADERS: &[HeaderSession] = &[
            HeaderSession {
                names: &["x-session-id"],
                client: "generic",
                prefix: "header:",
                parents: &["x-parent-session-id", "x-parent-id"],
                agent: "main",
            },
            HeaderSession {
                names: &["x-session-affinity"],
                client: "opencode",
                prefix: "affinity:",
                parents: &[
                    "x-parent-session-affinity",
                    "x-parent-session-id",
                    "x-parent-id",
                ],
                agent: "main",
            },
            HeaderSession {
                names: &["x-slot-session-id"],
                client: "pi",
                prefix: "slot:",
                parents: &[
                    "x-parent-slot-session-id",
                    "x-parent-session-id",
                    "x-parent-id",
                ],
                agent: "slot",
            },
            HeaderSession {
                names: &["x-task-id", "x-task_id"],
                client: "task",
                prefix: "task:",
                parents: &["x-parent-task-id", "x-parent-session-id", "x-parent-id"],
                agent: "main",
            },
            HeaderSession {
                names: &["x-conversation-id"],
                client: "conv",
                prefix: "conv:",
                parents: &["x-parent-conversation-id", "x-parent-id"],
                agent: "main",
            },
            HeaderSession {
                names: &["x-thread-id"],
                client: "openai-thread",
                prefix: "thread:",
                parents: &["x-parent-thread-id", "x-parent-id"],
                agent: "main",
            },
            HeaderSession {
                names: &["x-client-request-id"],
                client: "generic",
                prefix: "clientreq:",
                parents: &["x-parent-session-id", "x-parent-id"],
                agent: "main",
            },
        ];
        HEADERS.iter().find_map(
            |&HeaderSession {
                 names,
                 client,
                 prefix,
                 parents,
                 agent,
             }| {
                let sid = first_header(self.headers, names);
                if sid.is_empty() {
                    return None;
                }
                let parent = first_header(self.headers, parents);
                let mut info = SessionInfo {
                    client_type: client.to_owned(),
                    session_id: format!("{prefix}{sid}"),
                    ..SessionInfo::default()
                };
                let pc = self.parent_candidate;
                if !parent.is_empty() && parent != sid {
                    info.parent_session_id = format!("{prefix}{parent}");
                    info.agent_name = "subagent".to_owned();
                } else if !pc.is_empty() && pc != sid {
                    info.parent_session_id = format!("{prefix}{pc}");
                    info.agent_name = "subagent".to_owned();
                } else {
                    info.agent_name = agent.to_owned();
                }
                Some(info)
            },
        )
    }

    /// 6. The body's own fields.
    fn body(&self) -> Option<SessionInfo> {
        let roots = self.roots;
        if !roots.root.exists() {
            return None;
        }
        let pc = self.parent_candidate;

        // Gemini context caching.
        for path in ["cachedContent", "cached_content"] {
            let id = roots.first_by_path(&[path]);
            if !id.is_empty() {
                let mut info = SessionInfo {
                    client_type: "gemini".to_owned(),
                    session_id: format!("geminicache:{id}"),
                    agent_name: "main".to_owned(),
                    ..SessionInfo::default()
                };
                if !pc.is_empty() && pc != id {
                    info.parent_session_id = format!("geminicache:{pc}");
                    info.agent_name = "subagent".to_owned();
                }
                return Some(info);
            }
        }

        // An OpenAI thread.
        let tid = roots.first_by_path(THREAD_PATHS);
        if !tid.is_empty() {
            return Some(self.forkable("openai-thread", "thread:", &tid));
        }

        // A generic session.
        let mut agent = first_in(roots.root, AGENT_PATHS);
        if agent.is_empty() {
            agent = first_header(self.headers, &["x-claude-code-agent-id", "x-agent-id"]);
        }
        if agent.is_empty() {
            agent = roots
                .nested
                .map(|nested| first_in(nested, AGENT_PATHS))
                .unwrap_or_default();
        }
        let sid = roots.first_by_path(SESSION_PATHS);
        if !sid.is_empty() {
            if agent.is_empty() || agent == "main" {
                return Some(self.forkable("generic", "session:", &sid));
            }
            let mut info = SessionInfo {
                client_type: "generic".to_owned(),
                session_id: format!("session:{sid}:agent:{agent}"),
                parent_session_id: format!("session:{sid}"),
                ..SessionInfo::default()
            };
            if !pc.is_empty() && pc != sid {
                info.parent_session_id = format!("session:{pc}");
            }
            info.agent_name = agent;
            return Some(info);
        }

        // A task or action (Roo Code, Cline, OpenHands).
        let tid = roots.first_by_path(TASK_PATHS);
        if !tid.is_empty() {
            return Some(self.forkable("task", "task:", &tid));
        }

        // A prompt cache key, else the conversation object.
        let conversation = roots.conversation();
        let mut conversation_id = candidate(conversation.get("id"));
        if conversation_id.is_empty() && conversation.is_string() {
            conversation_id = candidate(conversation);
        }
        let pck = roots.first_by_root(&["prompt_cache_key", "promptCacheKey"]);
        if !pck.is_empty() {
            return Some(self.child("generic", "pck:", &pck));
        }
        if !conversation_id.is_empty() {
            return Some(self.child("conv", "conv:", &conversation_id));
        }

        // A plain metadata.user_id.
        let user = roots.first_by_path(&["metadata.user_id"]);
        if !user.is_empty() {
            return Some(SessionInfo {
                client_type: "generic".to_owned(),
                session_id: format!("user:{user}"),
                agent_name: "main".to_owned(),
                ..SessionInfo::default()
            });
        }

        // Legacy conversation fields.
        let cid = roots.first_by_path(CONVERSATION_PATHS);
        (!cid.is_empty()).then(|| self.child("conv", "conv:", &cid))
    }

    /// The session `id` under `prefix`, a subagent of the body's parent
    /// when it names one.
    fn child(&self, client: &str, prefix: &str, id: &str) -> SessionInfo {
        let pc = self.parent_candidate;
        let mut info = SessionInfo {
            client_type: client.to_owned(),
            session_id: format!("{prefix}{id}"),
            agent_name: "main".to_owned(),
            ..SessionInfo::default()
        };
        if !pc.is_empty() && pc != id {
            info.parent_session_id = format!("{prefix}{pc}");
            info.agent_name = "subagent".to_owned();
        }
        info
    }

    /// The session `id` under `prefix`, a fork of the body's parent when the
    /// body says it forked, else a subagent of it.
    fn forkable(&self, client: &str, prefix: &str, id: &str) -> SessionInfo {
        let pc = self.parent_candidate;
        let mut info = SessionInfo {
            client_type: client.to_owned(),
            session_id: format!("{prefix}{id}"),
            agent_name: "main".to_owned(),
            ..SessionInfo::default()
        };
        if !pc.is_empty() && pc != id {
            info.parent_session_id = format!("{prefix}{pc}");
            if self.roots.first_by_path(FORK_CANDIDATE_PATHS).is_empty() {
                info.agent_name = "subagent".to_owned();
                info.is_subagent = true;
            } else {
                info.is_fork = true;
            }
        }
        info
    }
}

/// `id`, or when longer than 256 bytes its first 190 bytes, cut back to a
/// whole character, then `#` and the SHA-256 of all of it in hex
/// (upstream's `BoundSessionIdentity`).
pub fn bound_session_identity(id: &str) -> String {
    if id.len() <= MAX_BOUND_LENGTH {
        return id.to_owned();
    }
    let hash = sha256_hex(id.as_bytes());
    let mut end = (MAX_BOUND_LENGTH - 2 - hash.len()).min(id.len());
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}#{hash}", id.get(..end).unwrap_or_default())
}

/// `info` bounded, with its defaults, and without a parent that is itself
/// (upstream's `finalizeSessionInfo`).
fn finalize(mut info: SessionInfo) -> Option<SessionInfo> {
    if info.session_id.is_empty() {
        return None;
    }
    info.session_id = bound_session_identity(&info.session_id);
    info.parent_session_id = bound_session_identity(&info.parent_session_id);
    if info.agent_name.is_empty() {
        "main".clone_into(&mut info.agent_name);
    }
    if info.client_type.is_empty() {
        "generic".clone_into(&mut info.client_type);
    }
    if info.parent_session_id == info.session_id {
        info.parent_session_id.clear();
    }
    Some(info)
}
