//! Differential tests of open-ferry's translators against upstream CLIProxyAPI.
//!
//! Runs the same input through upstream's Go translators and our Rust ports
//! and compares the results. See README.md.

mod baseline;
mod cases;
mod codex_models;
mod compare;
mod config_diff;
mod config_save;
mod embedded_names;
mod generate;
mod interactions;
mod live;
mod multi_agent;
mod parallel;
mod payload;
mod quota_signals;
mod raw_json;
mod request_witness;
mod session;
mod signature;
mod stream_framing;
mod summary;
mod translator;
mod ttft;
mod upstream;
mod usage;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Instant;

use serde_json::{Value, json};

use crate::cases::Case;
use crate::compare::{Deviation, Difference};
use crate::translator::Translator;
use crate::upstream::{GoResult, Upstream};

const USAGE: &str = "\
usage: open-ferry-parity --upstream <CLIProxyAPI checkout> [options]

options:
  --random <n>   random cases to generate per translator (default 5000)
  --seed <n>     seed for the random cases (default 1)
  --jobs <n>     concurrent config-save cases (default: available logical processors; 1: sequential)
  --suite <name> run only this offline suite (directory slug); repeat to select several
                 (default: all suites; unknown names are errors)
  --show <n>     kinds of difference to print (default 10)
  --go <path>    Go binary (default: go on PATH)
  --live <url>   instead, send a few requests through a running CLIProxyAPI;
                 needs --model and the client API key in OPEN_FERRY_PARITY_API_KEY
  --model <name> model for --live
  --summary <file>
                 also write a Markdown summary of the results to <file>,
                 replacing the part between its parity-summary markers
  --baseline <file> apply the config-save certification-failure budget
  --observation <file> write portable config-save baseline observations
  --write-baseline <file> refresh deterministically from --baseline-observation files
                 add --baseline <existing> for an incremental refresh
  --baseline-observation <file> repeat for refresh or --baseline replay (no Go run)
  --export-config-cases <file> export canonical random config-save inputs for --seed/--random
                 (no upstream checkout or Go run)
  --relabel-baseline <file> update only generator_sha256 if both recorded probes match
                 (fixed seed-1/25 and seed-38/5000 probes; no Go run)
  --import-config-failures <dir> import saved fixtures, with --import-proofs <dir>,
                 --import-cases <n> and --observation <file> (no Go run)
  --selected-failures mark imported selected failures as kind coverage only
  --replay-config-failures <dir> execute saved config-save inputs (selected replay)";

/// Failing cases written to disk; the rest are only counted.
const MAX_FAILURE_FILES: usize = 200;
/// Dense config sweeps need all failures to measure upstream defect frequency.
const MAX_CONFIG_FAILURE_FILES: usize = 1000;

#[derive(Clone)]
struct Args {
    upstream: PathBuf,
    random: usize,
    jobs: usize,
    seed: u64,
    show: usize,
    go: PathBuf,
    live: Option<String>,
    model: Option<String>,
    summary: Option<PathBuf>,
    suites: BTreeSet<String>,
    baseline: Option<PathBuf>,
    observation: Option<PathBuf>,
    write_baseline: Option<PathBuf>,
    baseline_observations: Vec<PathBuf>,
    import_config_failures: Option<PathBuf>,
    import_proofs: Option<PathBuf>,
    import_cases: Option<usize>,
    selected_failures: bool,
    replay_config_failures: Option<PathBuf>,
    export_config_cases: Option<PathBuf>,
    relabel_baseline: Option<PathBuf>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut upstream = None;
        let mut run_flags = BTreeSet::new();
        let mut parsed = Self {
            upstream: PathBuf::new(),
            random: 5000,
            jobs: std::thread::available_parallelism().map_or(1, usize::from),
            seed: 1,
            show: 10,
            go: "go".into(),
            live: None,
            model: None,
            summary: None,
            suites: BTreeSet::new(),
            baseline: None,
            observation: None,
            write_baseline: None,
            baseline_observations: Vec::new(),
            import_config_failures: None,
            import_proofs: None,
            import_cases: None,
            selected_failures: false,
            replay_config_failures: None,
            export_config_cases: None,
            relabel_baseline: None,
        };
        while let Some(flag) = args.next() {
            if matches!(
                flag.as_str(),
                "--seed" | "--random" | "--show" | "--go" | "--jobs"
            ) {
                run_flags.insert(flag.clone());
            }
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--upstream" => upstream = Some(PathBuf::from(value()?)),
                "--random" => parsed.random = parse_number(&flag, &value()?)?,
                "--jobs" => {
                    parsed.jobs = parse_number(&flag, &value()?)?;
                    if parsed.jobs == 0 {
                        return Err("--jobs must be at least 1".into());
                    }
                }
                "--seed" => parsed.seed = parse_number(&flag, &value()?)?,
                "--show" => parsed.show = parse_number(&flag, &value()?)?,
                "--go" => parsed.go = value()?.into(),
                "--live" => parsed.live = Some(value()?),
                "--model" => parsed.model = Some(value()?),
                "--summary" => parsed.summary = Some(value()?.into()),
                "--baseline" => parsed.baseline = Some(value()?.into()),
                "--observation" => parsed.observation = Some(value()?.into()),
                "--write-baseline" => parsed.write_baseline = Some(value()?.into()),
                "--baseline-observation" => parsed.baseline_observations.push(value()?.into()),
                "--import-config-failures" => parsed.import_config_failures = Some(value()?.into()),
                "--import-proofs" => parsed.import_proofs = Some(value()?.into()),
                "--import-cases" => parsed.import_cases = Some(parse_number(&flag, &value()?)?),
                "--selected-failures" => parsed.selected_failures = true,
                "--replay-config-failures" => parsed.replay_config_failures = Some(value()?.into()),
                "--export-config-cases" => parsed.export_config_cases = Some(value()?.into()),
                "--relabel-baseline" => parsed.relabel_baseline = Some(value()?.into()),
                "--suite" => {
                    parsed.suites.insert(value()?);
                }
                "-h" | "--help" => return Err(String::new()),
                _ => return Err(format!("unknown argument {flag}")),
            }
        }
        let replay = !parsed.baseline_observations.is_empty();
        let maintenance = parsed.export_config_cases.is_some() || parsed.relabel_baseline.is_some();
        if maintenance {
            if parsed.relabel_baseline.is_some() && !run_flags.is_empty() {
                return Err(format!(
                    "--relabel-baseline cannot use run flags: {}; its probe profiles are fixed",
                    run_flags.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
            if parsed.export_config_cases.is_some()
                && run_flags
                    .iter()
                    .any(|flag| !matches!(flag.as_str(), "--seed" | "--random"))
            {
                return Err("--export-config-cases accepts only --seed and --random".into());
            }
            if (parsed.export_config_cases.is_some() && parsed.relabel_baseline.is_some())
                || replay
                || parsed.baseline.is_some()
                || parsed.write_baseline.is_some()
                || parsed.observation.is_some()
                || parsed.live.is_some()
                || parsed.model.is_some()
                || parsed.import_config_failures.is_some()
                || parsed.import_proofs.is_some()
                || parsed.import_cases.is_some()
                || parsed.selected_failures
                || parsed.replay_config_failures.is_some()
                || !parsed.suites.is_empty()
                || parsed.summary.is_some()
                || upstream.is_some()
            {
                return Err("export/relabel must run separately from parity runs and baseline imports/refreshes".into());
            }
            return Ok(parsed);
        }
        if (parsed.write_baseline.is_some() && !replay)
            || (replay && parsed.write_baseline.is_none() && parsed.baseline.is_none())
        {
            return Err("baseline refresh/replay needs --baseline-observation and --write-baseline or --baseline".into());
        }
        let import = parsed.import_config_failures.is_some();
        if import
            && (parsed.import_proofs.is_none()
                || parsed.import_cases.is_none()
                || parsed.observation.is_none()
                || replay
                || parsed.baseline.is_some())
        {
            return Err("import needs --import-proofs, --import-cases and --observation; cannot apply a baseline".into());
        }
        if parsed.replay_config_failures.is_some()
            && (import
                || replay
                || parsed.live.is_some()
                || parsed.suites.iter().any(|s| s != "config-save"))
        {
            return Err("--replay-config-failures selects only offline config-save inputs".into());
        }
        if !import
            && (parsed.import_proofs.is_some()
                || parsed.import_cases.is_some()
                || parsed.selected_failures)
        {
            return Err("import options need --import-config-failures".into());
        }
        parsed.upstream = if replay || import {
            upstream.unwrap_or_default()
        } else {
            upstream.ok_or("--upstream is required")?
        };
        if parsed.live.is_some() && parsed.model.is_none() {
            return Err("--live needs --model".into());
        }
        if parsed.live.is_some() && !parsed.suites.is_empty() {
            return Err("--suite cannot be combined with --live".into());
        }
        if parsed.live.is_some()
            && (parsed.baseline.is_some() || replay || import || parsed.observation.is_some())
        {
            return Err("baseline options cannot be combined with --live".into());
        }
        if !parsed.suites.is_empty() {
            // Validate without generating random cases or invoking Go.
            selected_suites(&Self {
                random: 0,
                ..parsed.clone()
            })?;
        }
        Ok(parsed)
    }
}

fn parse_number<T: FromStr>(flag: &str, value: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} expects a number, got {value:?}"))
}

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("{message}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

type Suite = (Translator, Vec<Case>, Vec<Case>);

fn selected_suites(args: &Args) -> Result<Vec<Suite>, String> {
    if let Some(path) = &args.replay_config_failures {
        return baseline::replay_cases(path)
            .map(|cases| vec![(Translator::ConfigSave, cases, vec![])])
            .map_err(|error| error.to_string());
    }
    let seed = args.seed;
    let random = args.random;
    let wanted =
        |translator: Translator| args.suites.is_empty() || args.suites.contains(translator.slug());
    macro_rules! suite {
        ($translator:expr, $handwritten:expr, $random:expr $(,)?) => {{
            let translator = $translator;
            if wanted(translator) {
                (translator, $handwritten, $random)
            } else {
                (translator, Vec::new(), Vec::new())
            }
        }};
    }
    macro_rules! events {
        ($stream:ident, $final:ident, $generate:expr) => {
            if wanted(Translator::$stream) || wanted(Translator::$final) {
                $generate
            } else {
                (Vec::new(), Vec::new())
            }
        };
    }
    let (streams, finals) = events!(Stream, NonStream, generate::response::cases(seed, random));
    let (responses_streams, responses_finals) = events!(
        ResponsesStream,
        ResponsesNonStream,
        generate::responses::event_cases(seed, random)
    );
    let (chat_streams, chat_finals) = events!(
        ChatStream,
        ChatNonStream,
        generate::chat::event_cases(seed, random)
    );
    let (claude_chat_streams, claude_chat_finals) = events!(
        ClaudeChatStream,
        ClaudeChatNonStream,
        generate::claude_chat::event_cases(seed, random)
    );
    let (claude_responses_streams, claude_responses_finals) = events!(
        ClaudeResponsesStream,
        ClaudeResponsesNonStream,
        generate::claude_responses::event_cases(seed, random)
    );
    let (openai_responses_streams, openai_responses_finals) = events!(
        OpenAIResponsesStream,
        OpenAIResponsesNonStream,
        generate::openai_responses::event_cases(seed, random)
    );
    let (openai_claude_streams, openai_claude_finals) = events!(
        OpenAIClaudeStream,
        OpenAIClaudeNonStream,
        generate::openai_claude::event_cases(seed, random)
    );
    let (openai_chat_streams, openai_chat_finals) = events!(
        OpenAIChatStream,
        OpenAIChatNonStream,
        generate::openai_chat::event_cases(seed, random)
    );
    let (codex_gemini_streams, codex_gemini_finals) = events!(
        CodexGeminiStream,
        CodexGeminiNonStream,
        generate::gemini::codex_event_cases(seed, random)
    );
    let (claude_gemini_streams, claude_gemini_finals) = events!(
        ClaudeGeminiStream,
        ClaudeGeminiNonStream,
        generate::gemini::claude_event_cases(seed, random)
    );
    let (openai_gemini_streams, openai_gemini_finals) = events!(
        OpenAIGeminiStream,
        OpenAIGeminiNonStream,
        generate::gemini::openai_event_cases(seed, random)
    );
    let (registry_streams, registry_finals) = events!(
        RegistryStream,
        RegistryNonStream,
        generate::registry::response_cases(seed, random)
    );
    let (gemini_streams, gemini_finals) = events!(
        GeminiGeminiStream,
        GeminiGeminiNonStream,
        generate::to_gemini::event_cases(seed, random)
    );
    let (gemini_claude_streams, gemini_claude_finals) = events!(
        GeminiClaudeStream,
        GeminiClaudeNonStream,
        generate::to_gemini::claude_event_cases(seed, random)
    );
    let (gemini_chat_streams, gemini_chat_finals) = events!(
        GeminiChatStream,
        GeminiChatNonStream,
        generate::to_gemini::chat_event_cases(seed, random)
    );
    let (gemini_responses_streams, gemini_responses_finals) = events!(
        GeminiResponsesStream,
        GeminiResponsesNonStream,
        generate::gemini_responses::event_cases(seed, random)
    );
    let mut suites = vec![
        suite!(
            Translator::Request,
            cases::hand_written(),
            generate::cases(seed, random),
        ),
        suite!(Translator::Stream, cases::hand_written_streams(), streams),
        suite!(Translator::NonStream, cases::hand_written_finals(), finals),
        suite!(
            Translator::ResponsesRequest,
            cases::responses::requests(),
            generate::responses::request_cases(seed, random),
        ),
        suite!(
            Translator::ResponsesStream,
            cases::responses::streams(),
            responses_streams,
        ),
        suite!(
            Translator::ResponsesNonStream,
            cases::responses::finals(),
            responses_finals,
        ),
        suite!(
            Translator::ChatRequest,
            cases::chat::requests(),
            generate::chat::request_cases(seed, random),
        ),
        suite!(Translator::ChatStream, cases::chat::streams(), chat_streams),
        suite!(
            Translator::ChatNonStream,
            cases::chat::finals(),
            chat_finals,
        ),
        suite!(
            Translator::RequestCompat,
            cases::hand_written(),
            generate::cases(seed, random),
        ),
        suite!(
            Translator::ClaudeChatRequest,
            cases::claude_chat::requests(),
            generate::claude_chat::request_cases(seed, random),
        ),
        suite!(
            Translator::ClaudeChatRequestCompat,
            cases::claude_chat::requests(),
            generate::claude_chat::request_cases(seed, random),
        ),
        suite!(
            Translator::ClaudeChatStream,
            cases::claude_chat::streams(),
            claude_chat_streams,
        ),
        suite!(
            Translator::ClaudeChatNonStream,
            cases::claude_chat::finals(),
            claude_chat_finals
                .into_iter()
                .chain(generate::claude_chat::native_cases(seed, random / 2))
                .collect(),
        ),
        suite!(
            Translator::ClaudeResponsesRequest,
            cases::claude_responses::requests(),
            generate::claude_responses::request_cases(seed, random),
        ),
        suite!(
            Translator::ClaudeResponsesRequestCompat,
            cases::claude_responses::requests(),
            generate::claude_responses::request_cases(seed, random),
        ),
        suite!(
            Translator::ClaudeResponsesStream,
            cases::claude_responses::streams(),
            claude_responses_streams,
        ),
        suite!(
            Translator::ClaudeResponsesNonStream,
            cases::claude_responses::finals(),
            claude_responses_finals
                .into_iter()
                .chain(generate::claude_responses::native_cases(seed, random / 2))
                .collect(),
        ),
        suite!(
            Translator::OpenAIResponsesRequest,
            cases::openai_responses::requests(),
            generate::openai_responses::request_cases(seed, random),
        ),
        suite!(
            Translator::OpenAIResponsesStream,
            cases::openai_responses::streams(),
            openai_responses_streams,
        ),
        suite!(
            Translator::OpenAIResponsesNonStream,
            cases::openai_responses::finals(),
            openai_responses_finals,
        ),
        suite!(
            Translator::OpenAIClaudeRequest,
            cases::openai_claude::requests(),
            generate::openai_claude::request_cases(seed, random),
        ),
        suite!(
            Translator::OpenAIClaudeRequestCompat,
            cases::openai_claude::requests(),
            generate::openai_claude::request_cases(seed, random),
        ),
        suite!(
            Translator::OpenAIClaudeStream,
            cases::openai_claude::streams(),
            openai_claude_streams,
        ),
        suite!(
            Translator::OpenAIClaudeNonStream,
            cases::openai_claude::finals(),
            openai_claude_finals,
        ),
        suite!(
            Translator::OpenAIChatRequest,
            cases::openai_chat::requests(),
            generate::openai_chat::request_cases(seed, random),
        ),
        suite!(
            Translator::OpenAIChatStream,
            cases::openai_chat::streams(),
            openai_chat_streams,
        ),
        suite!(
            Translator::OpenAIChatNonStream,
            cases::openai_chat::finals(),
            openai_chat_finals,
        ),
        suite!(
            Translator::CodexGeminiRequest,
            cases::gemini::requests(),
            generate::gemini::request_cases(seed, random),
        ),
        suite!(
            Translator::CodexGeminiStream,
            cases::gemini::codex_streams(),
            codex_gemini_streams,
        ),
        suite!(
            Translator::CodexGeminiNonStream,
            cases::gemini::codex_finals(),
            codex_gemini_finals,
        ),
        suite!(
            Translator::ClaudeGeminiRequest,
            cases::gemini::requests(),
            generate::gemini::request_cases(seed.rotate_left(1), random),
        ),
        suite!(
            Translator::ClaudeGeminiStream,
            cases::gemini::claude_streams(),
            claude_gemini_streams,
        ),
        suite!(
            Translator::ClaudeGeminiNonStream,
            cases::gemini::claude_finals(),
            claude_gemini_finals,
        ),
        suite!(
            Translator::OpenAIGeminiRequest,
            cases::gemini::requests(),
            generate::gemini::request_cases(seed.rotate_left(2), random),
        ),
        suite!(
            Translator::OpenAIGeminiStream,
            cases::gemini::openai_streams(),
            openai_gemini_streams,
        ),
        suite!(
            Translator::OpenAIGeminiNonStream,
            cases::gemini::openai_finals(),
            openai_gemini_finals,
        ),
        suite!(
            Translator::SignatureInspect,
            cases::signature::inspect(),
            generate::signature::inspect_cases(seed, random),
        ),
        suite!(
            Translator::ClaudeMessagesSignatures,
            cases::signature::claude_messages(),
            generate::signature::claude_messages_cases(seed, random),
        ),
        suite!(
            Translator::GeminiSignatures,
            cases::signature::gemini(),
            generate::signature::gemini_cases(seed, random),
        ),
        suite!(
            Translator::RegistryRequest,
            cases::registry::requests(),
            generate::registry::request_cases(seed, random),
        ),
        suite!(
            Translator::RegistryStream,
            cases::registry::streams(),
            registry_streams,
        ),
        suite!(
            Translator::RegistryNonStream,
            cases::registry::finals(),
            registry_finals,
        ),
        suite!(
            Translator::RegistryLookup,
            cases::registry::lookups(),
            generate::registry::lookup_cases(seed, random),
        ),
        suite!(
            Translator::CompletionsRequest,
            cases::completions::requests(),
            generate::completions::request_cases(seed, random),
        ),
        suite!(
            Translator::CompletionsResponse,
            cases::completions::responses(),
            generate::completions::response_cases(seed, random),
        ),
        suite!(
            Translator::CompletionsStreamChunk,
            cases::completions::stream_chunks(),
            generate::completions::chunk_cases(seed, random),
        ),
        suite!(
            Translator::GeminiGeminiRequest,
            cases::to_gemini::gemini_requests(),
            generate::to_gemini::request_cases(seed, random),
        ),
        suite!(
            Translator::GeminiGeminiStream,
            cases::to_gemini::gemini_streams(),
            gemini_streams,
        ),
        suite!(
            Translator::GeminiGeminiNonStream,
            cases::to_gemini::gemini_finals(),
            gemini_finals,
        ),
        suite!(
            Translator::GeminiClaudeRequest,
            cases::to_gemini::claude_requests(),
            generate::to_gemini::claude_request_cases(seed, random),
        ),
        suite!(
            Translator::GeminiClaudeRequestCompat,
            cases::to_gemini::claude_requests(),
            generate::to_gemini::claude_request_cases(seed, random),
        ),
        suite!(
            Translator::GeminiClaudeStream,
            cases::to_gemini::claude_streams(),
            gemini_claude_streams,
        ),
        suite!(
            Translator::GeminiClaudeNonStream,
            cases::to_gemini::claude_finals(),
            gemini_claude_finals,
        ),
        suite!(
            Translator::GeminiChatRequest,
            cases::to_gemini::chat_requests(),
            generate::to_gemini::chat_request_cases(seed, random),
        ),
        suite!(
            Translator::GeminiChatStream,
            cases::to_gemini::chat_streams(),
            gemini_chat_streams,
        ),
        suite!(
            Translator::GeminiChatNonStream,
            cases::to_gemini::chat_finals(),
            gemini_chat_finals,
        ),
        suite!(
            Translator::ThinkingCodex,
            cases::thinking::codex(),
            generate::thinking::codex_cases(seed, random),
        ),
        suite!(
            Translator::ThinkingOpenAI,
            cases::thinking::openai(),
            generate::thinking::openai_cases(seed, random),
        ),
        suite!(
            Translator::GeminiResponsesRequest,
            cases::gemini_responses::requests(),
            generate::gemini_responses::request_cases(seed, random),
        ),
        suite!(
            Translator::GeminiResponsesStream,
            cases::gemini_responses::streams(),
            gemini_responses_streams,
        ),
        suite!(
            Translator::GeminiResponsesNonStream,
            cases::gemini_responses::finals(),
            gemini_responses_finals,
        ),
        suite!(
            Translator::CodexModels,
            cases::codex_models::lists(),
            generate::codex_models::list_cases(seed, random),
        ),
        suite!(
            Translator::MultiAgentPrepare,
            cases::multi_agent::prepares(),
            generate::multi_agent::prepare_cases(seed, random),
        ),
        suite!(
            Translator::MultiAgentOptimize,
            cases::multi_agent::optimizes(),
            generate::multi_agent::optimize_cases(seed, random),
        ),
        suite!(
            Translator::MultiAgentInput,
            cases::multi_agent::inputs(),
            generate::multi_agent::input_cases(seed, random),
        ),
        suite!(
            Translator::MultiAgentOrphan,
            cases::multi_agent::orphans(),
            generate::multi_agent::orphan_cases(seed, random),
        ),
        suite!(
            Translator::MultiAgentRestore,
            cases::multi_agent::restores(),
            generate::multi_agent::restore_cases(seed, random),
        ),
        suite!(
            Translator::Payload,
            cases::payload::applies(),
            generate::payload::apply_cases(seed, random),
        ),
        suite!(
            Translator::Usage,
            cases::usage::parses(),
            generate::usage::parse_cases(seed, random),
        ),
        suite!(
            Translator::Ttft,
            cases::ttft::token_events(),
            generate::ttft::token_event_cases(seed, random),
        ),
        suite!(
            Translator::SessionInfo,
            cases::session::infos(),
            generate::session::info_cases(seed, random),
        ),
        suite!(
            Translator::SessionDerive,
            cases::session::derives(),
            generate::session::derive_cases(seed, random),
        ),
        suite!(
            Translator::ConfigDiff,
            cases::config_diff::details(),
            generate::config_diff::detail_cases(seed, random),
        ),
        suite!(
            Translator::ConfigSave,
            cases::config_save::steps(&args.upstream),
            generate::config_save::step_cases(seed, random),
        ),
        suite!(
            Translator::QuotaSignals,
            cases::quota_signals::observations(),
            generate::quota_signals::observe_cases(seed, random),
        ),
    ];
    // The Gemini Interactions families' suites (see interactions/mod.rs).
    suites.extend(interactions::selected_suites(seed, random, &wanted));

    filter_suites(&mut suites, &args.suites)?;
    Ok(suites)
}

fn filter_suites(suites: &mut Vec<Suite>, names: &BTreeSet<String>) -> Result<(), String> {
    for name in names {
        if !suites
            .iter()
            .any(|(translator, _, _)| translator.slug() == name)
        {
            return Err(format!(
                "unknown suite {name:?}; use a suite directory slug"
            ));
        }
    }
    if !names.is_empty() {
        suites.retain(|(translator, _, _)| names.contains(translator.slug()));
    }
    Ok(())
}

/// Returns whether every case matched upstream, allowing documented deviations.
fn run(args: &Args) -> Result<bool, Box<dyn Error>> {
    if let Some(path) = &args.export_config_cases {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, baseline::probes::export(args.seed, args.random)?)?;
        println!(
            "Canonical random config-save cases written to {}",
            path.display()
        );
        return Ok(true);
    }
    if let Some(path) = &args.relabel_baseline {
        baseline::relabel(path, &baseline::pinned_fingerprint()?)?;
        println!(
            "Generator fingerprint relabeled in {}; both random probes match.",
            path.display()
        );
        return Ok(true);
    }
    if let Some(failures) = &args.import_config_failures {
        let observation = baseline::import(
            failures,
            args.import_proofs.as_ref().unwrap(),
            baseline::Observation {
                fingerprint: baseline::pinned_fingerprint()?,
                seed: args.seed,
                random: args.random,
                cases: args.import_cases.unwrap(),
                selected: args.selected_failures,
                different: 0,
                non_certification: 0,
                case_kinds: BTreeMap::new(),
            },
        )?;
        baseline::write(args.observation.as_ref().unwrap(), &observation)?;
        return Ok(observation.non_certification == 0);
    }
    if !args.baseline_observations.is_empty() {
        let fingerprint = baseline::pinned_fingerprint()?;
        let observations = baseline::observations(&args.baseline_observations)?;
        if let Some(path) = &args.write_baseline {
            let refreshed = if let Some(existing) = &args.baseline {
                baseline::refresh_incremental(
                    &baseline::read(existing)?,
                    &observations,
                    &fingerprint,
                )?
            } else {
                baseline::refresh(&observations, &fingerprint)?
            };
            baseline::write(path, &refreshed)?;
            println!(
                "Baseline written to {}; raises/additional kinds require a PR explanation.",
                path.display()
            );
            return Ok(true);
        }
        let path = args.baseline.as_ref().unwrap();
        let baseline: baseline::Baseline = baseline::read(path)?;
        baseline.validate(&fingerprint)?;
        let mut passed = true;
        for observation in &observations {
            let assessment = baseline.assess(observation, path)?;
            println!(
                "Seed {}, random {}{}",
                observation.seed,
                observation.random,
                if observation.selected {
                    " (selected failures)"
                } else {
                    ""
                }
            );
            println!("{}", assessment.markdown);
            passed &= assessment.passed;
        }
        return Ok(passed);
    }
    let budget: Option<baseline::Baseline> =
        args.baseline.as_deref().map(baseline::read).transpose()?;
    if let Some(budget) = &budget {
        budget.validate(&baseline::pinned_fingerprint()?)?;
    }
    let default_work_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/parity is two levels below the workspace root")
        .join("target")
        .join("parity");
    let custom_work_dir = std::env::var_os("OPEN_FERRY_PARITY_WORK_DIR").map(PathBuf::from);
    let work_dir = prepare_work_dir(&default_work_dir, custom_work_dir.as_deref())?;
    // Even a failed upstream setup must not retain proofs from an older run.
    fresh_dir(&work_dir.join("counterfactual"))?;
    let upstream = Upstream::build(&args.upstream, &args.go, &work_dir)?;
    let fingerprint = baseline::fingerprint(&upstream.commit)?;
    if let Some(budget) = &budget {
        budget.validate(&fingerprint)?;
    }
    if let (Some(url), Some(model)) = (&args.live, &args.model) {
        return run_live(url, model, &upstream, &work_dir);
    }

    let suites = selected_suites(args)?;
    let failures_dir = fresh_dir(&work_dir.join("failures"))?;

    println!("open-ferry parity");
    println!(
        "upstream    {} at {} ({})",
        upstream.dir.display(),
        upstream.version,
        upstream.commit
    );
    let mut all_match = true;
    let mut results = Vec::with_capacity(suites.len());
    let mut ratchet_summary = String::new();
    for (translator, mut cases, random) in suites {
        cases.extend(crate::cases::signature_relay::for_suite(translator.key()));
        let hand_written = cases.len();
        let generated = random.len();
        cases.extend(random);
        let started = Instant::now();
        if translator == Translator::ConfigSave {
            config_save::metrics::reset();
        }
        let dir = failures_dir.join(translator.slug());
        let tally = check(translator, &cases, &upstream, &work_dir, &dir, args.jobs)?;
        println!();
        println!("{}", translator.title());
        println!(
            "cases       {hand_written} hand-written + {generated} random (seed {}), {:.1?}",
            args.seed,
            started.elapsed()
        );
        tally.print(args.show, &dir);
        if translator == Translator::ConfigSave {
            config_save::metrics::flush()?;
        }
        let mut matched = tally.different == 0;
        if translator == Translator::ConfigSave {
            (matched, ratchet_summary) = config_save_result(
                &tally,
                args,
                &fingerprint,
                cases.len(),
                budget.as_ref().zip(args.baseline.as_deref()),
                args.observation
                    .as_deref()
                    .unwrap_or(&work_dir.join("config-save-observation.json")),
            )?;
            if !ratchet_summary.is_empty() {
                println!("{ratchet_summary}");
            }
        }
        all_match &= matched;
        results.push(summary::SuiteResult {
            title: translator.title(),
            deviations: tally
                .deviations
                .iter()
                .map(|(d, n)| (d.describe(), *n))
                .collect(),
            cases: cases.len(),
            hand_written,
            identical: tally.identical,
            equivalent: tally.equivalent,
            known: tally.known.len(),
            different: tally.different,
            upstream_lossy_plugins: tally
                .deviations
                .get(&Deviation::UpstreamLossyPluginReencode)
                .copied()
                .unwrap_or_default(),
            upstream_archived_counterfactuals: tally
                .deviations
                .get(&Deviation::UpstreamArchivedScalarCounterfactual)
                .copied()
                .unwrap_or_default(),
            upstream_folded_reencodes: tally
                .deviations
                .get(&Deviation::UpstreamFoldedScalarReencode)
                .copied()
                .unwrap_or_default(),
            upstream_counterfactuals: tally
                .deviations
                .get(&Deviation::UpstreamBlockScalarCounterfactual)
                .copied()
                .unwrap_or_default(),
            upstream_version_comments: tally
                .deviations
                .get(&Deviation::UpstreamVersionCommentLoss)
                .copied()
                .unwrap_or(0),
        });
    }
    if let Some(path) = &args.summary {
        let go_version = go_version(&args.go);
        let run = summary::RunInfo {
            version: &upstream.version,
            commit: &upstream.commit,
            go_version: &go_version,
            random: args.random,
            seed: args.seed,
        };
        summary::write(path, &(summary::render(&run, &results) + &ratchet_summary))?;
        println!();
        println!("Summary written to {}", path.display());
    }
    Ok(all_match)
}

/// The Go toolchain's version, such as `go1.26.4`, for the summary.
fn go_version(go: &Path) -> String {
    std::process::Command::new(go)
        .args(["env", "GOVERSION"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "an unknown Go version".into())
}

/// Runs `cases` through both sides of `translator`, writing failing cases to `dir`.
fn check(
    translator: Translator,
    cases: &[Case],
    upstream: &Upstream,
    work_dir: &Path,
    dir: &Path,
    jobs: usize,
) -> Result<Tally, Box<dyn Error>> {
    let go_results = upstream.run(translator.key(), cases, work_dir)?;
    // The documented SSE exception needs an independent upstream execution
    // on unframed input. A fixture name or a structural mismatch is no proof.
    let probes: Vec<_> = cases
        .iter()
        .enumerate()
        .filter_map(|(index, case)| {
            stream_framing::unframe(translator, case).map(|bare| (index, bare))
        })
        .collect();
    let bare_cases: Vec<_> = probes.iter().map(|(_, bare)| bare.case.clone()).collect();
    let bare_go = if bare_cases.is_empty() {
        Vec::new()
    } else {
        upstream.run(translator.key(), &bare_cases, work_dir)?
    };
    let mut proofs = probes.into_iter().zip(bare_go).peekable();
    let mut tally = Tally::default();
    let jobs = if translator == Translator::ConfigSave {
        jobs
    } else {
        1
    };
    // Workers start with default thread-local protection/trace scopes. Every
    // nested diagnostic scope restores itself on return and on unwinding.
    let staging = work_dir.join("case-workers");
    if jobs > 1 {
        fresh_dir(&staging)?;
    }
    // Rust panics are caught and reported per case, so keep them off stderr.
    panic::set_hook(Box::new(|_| {}));
    let recorded = parallel::for_each(
        cases.len().min(go_results.len()),
        jobs,
        |index| {
            let case = &cases[index];
            let go = &go_results[index];
            if jobs == 1 {
                return (
                    None,
                    evaluate_with_counterfactual(translator, case, go, upstream, work_dir)
                        .map_err(|e| e.to_string()),
                );
            }
            let dir = staging.join(index.to_string());
            let result = (|| {
                fs::create_dir(&dir)?;
                config_save::ablation::without_parallel_replay(|| {
                    evaluate_with_counterfactual(translator, case, go, upstream, &dir)
                })
            })()
            .map_err(|e: Box<dyn Error>| e.to_string());
            (Some(dir), result)
        },
        |index, (staged, evaluated)| {
            let case = &cases[index];
            if let Some(staged) = staged {
                // Publish even partial evidence before returning a case error,
                // just as sequential evaluation does. Later cases stay private.
                publish_case(&staged, work_dir, case)?;
                if staged.is_dir() {
                    fs::remove_dir_all(staged)?;
                }
            }
            let evaluated = evaluated.map_err(|e| -> Box<dyn Error> { e.into() })?;
            if let Some(path) = std::env::var_os("OPEN_FERRY_PARITY_DECISIONS") {
                use std::io::Write as _;
                let (decision, deviations) = match &evaluated.outcome {
                    Outcome::Identical => ("identical", Vec::new()),
                    Outcome::Equivalent(found) => {
                        ("equivalent", found.iter().map(|d| d.describe()).collect())
                    }
                    Outcome::Different(_) => ("different", Vec::new()),
                };
                let certificate =
                    if translator == Translator::ConfigSave && safe_case_name(&case.name) {
                        fs::read(
                            work_dir
                                .join("counterfactual")
                                .join(format!("{}.json", case.name)),
                        )
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    } else {
                        None
                    };
                let certificate_kind = certificate.as_ref().and_then(|c| c.get("kind"));
                let reasons = certificate.as_ref().map(|c| {
                    let mut reasons = Vec::new();
                    if let Some(reason) = c["reason"].as_str() {
                        reasons.push(reason);
                    }
                    if let Some(failures) = c["failures"].as_array() {
                        reasons.extend(failures.iter().filter_map(|f| f["reason"].as_str()));
                    }
                    reasons
                });
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path)?;
                serde_json::to_writer(
                    &mut file,
                    &json!({"suite":translator.slug(),"case":case.name,"decision":decision,"deviations":deviations,"kinds":evaluated.certification_failure,"certificate_kind":certificate_kind,"reasons":reasons}),
                )?;
                file.write_all(b"\n")?;
            }
            let proof = proofs.next_if(|((candidate, _), _)| *candidate == index);
            let mut checked = case.clone();
            if checked.known_difference.is_none()
                && matches!(evaluated.outcome, Outcome::Different(_))
                && let Ok(rust) = &evaluated.rust_output
                && let Some(((_, bare), go)) = proof
                && bare.matches(translator, rust, &go)
            {
                checked.known_difference = Some(stream_framing::REASON);
            }
            tally.record(&checked, evaluated, dir)
        },
        |index, payload| {
            if translator != Translator::ConfigSave {
                panic::resume_unwind(payload);
            }
            let staged = (jobs > 1)
                .then(|| staging.join(index.to_string()))
                .filter(|p| p.is_dir());
            (
                staged,
                Ok(evaluate_panic(
                    translator,
                    &cases[index],
                    &go_results[index],
                    payload,
                )),
            )
        },
    );
    let _ = panic::take_hook();
    // Speculative work is never published, even after a recorder error.
    let cleaned = if jobs > 1 {
        fs::remove_dir_all(staging)
    } else {
        Ok(())
    };
    recorded?;
    cleaned?;
    Ok(tally)
}

/// Preserve the sequential final request and proof bytes, including repeated
/// case names. Only this ordered recorder writes shared artifact paths.
fn publish_case(staged: &Path, work_dir: &Path, case: &Case) -> Result<(), Box<dyn Error>> {
    if staged.join("input.jsonl").is_file() {
        fs::copy(staged.join("input.jsonl"), work_dir.join("input.jsonl"))?;
    }
    if safe_case_name(&case.name) {
        let name = format!("{}.json", case.name);
        let proof = staged.join("counterfactual").join(&name);
        if proof.is_file() {
            fs::create_dir_all(work_dir.join("counterfactual"))?;
            fs::copy(proof, work_dir.join("counterfactual").join(name))?;
        }
    }
    Ok(())
}

/// Translates the live cases both ways and sends each body through the proxy,
/// then runs the replies through both response translators.
fn run_live(
    url: &str,
    model: &str,
    upstream: &Upstream,
    work_dir: &Path,
) -> Result<bool, Box<dyn Error>> {
    let api_key = std::env::var(live::API_KEY_VAR)
        .map_err(|_| format!("set {} to the proxy's client API key", live::API_KEY_VAR))?;
    let live_dir = fresh_dir(&work_dir.join("live"))?;
    let live_cases = live::cases();
    let cases: Vec<Case> = live_cases.iter().map(|case| case.to_case(model)).collect();
    let go_results = upstream.run(Translator::Request.key(), &cases, work_dir)?;
    let client = live::Client::new(url, api_key);

    println!("open-ferry parity, live");
    println!(
        "upstream    translators from {} ({})",
        upstream.version, upstream.commit
    );
    println!("proxy       {url}/v1/responses, model {model}");
    println!();
    println!("{}", Translator::Request.title());

    let (mut same, mut expected, mut tokens) = (0, 0, (0, 0));
    let (mut streams, mut finals) = (Vec::new(), Vec::new());
    for ((live_case, case), go) in live_cases.iter().zip(&cases).zip(&go_results) {
        let evaluated = evaluate(Translator::Request, case, go);
        let offline = match &evaluated.outcome {
            Outcome::Identical => "identical".to_owned(),
            Outcome::Equivalent(deviations) => {
                let names: Vec<&str> = deviations.iter().map(|d| d.describe()).collect();
                format!("equivalent ({})", names.join(", "))
            }
            Outcome::Different(_) => "DIFFERENT".to_owned(),
        };
        let GoResult::Output(go_body) = go else {
            return Err(format!("upstream panicked on {}", case.name).into());
        };
        let rust_output = evaluated.rust_output.as_ref().map_err(|err| err.clone())?;
        let rust_body = serde_json::to_vec(rust_output)?;

        let go_reply = client.send(go_body)?;
        let rust_reply = client.send(&rust_body)?;
        let shape_matches = go_reply.same_shape(&rust_reply);
        let both_expected = go_reply.meets(live_case.expect) && rust_reply.meets(live_case.expect);
        same += usize::from(shape_matches);
        expected += usize::from(both_expected);
        for (body, reply) in [
            ("upstream-body", &go_reply),
            ("open-ferry-body", &rust_reply),
        ] {
            tokens.0 += reply.input_tokens;
            tokens.1 += reply.output_tokens;
            let name = format!("{}-{body}", live_case.name);
            if let Some(event) = reply.final_event() {
                let events = vec![event.to_string()];
                finals.push(Case::response(&name, &live_case.request, events));
            }
            streams.push(Case::response(
                name,
                &live_case.request,
                reply.lines.clone(),
            ));
        }

        println!("{}: {}", live_case.name, live_case.exercises);
        println!("  offline     {offline}");
        println!(
            "  upstream    {}  {:.1?}",
            clip(&go_reply.summary()),
            go_reply.elapsed
        );
        println!(
            "  open-ferry  {}  {:.1?}",
            clip(&rust_reply.summary()),
            rust_reply.elapsed
        );
        println!(
            "  result      {}",
            match (shape_matches, both_expected) {
                (true, true) => "same shape, both as expected",
                (true, false) => "same shape, but not the expected reply",
                (false, _) => "DIFFERENT",
            }
        );

        let report = json!({
            "case": live_case.name,
            "model": model,
            "request": serde_json::from_str::<Value>(&case.request)?,
            "upstream_body": evaluated
                .go_value
                .clone()
                .unwrap_or_else(|| evaluated.go_output.as_str().into()),
            "open_ferry_body": rust_output,
            "upstream_reply": go_reply.to_json(),
            "open_ferry_reply": rust_reply.to_json(),
        });
        fs::write(
            live_dir.join(format!("{}.json", live_case.name)),
            serde_json::to_string_pretty(&report)?,
        )?;
    }

    println!();
    println!(
        "{} cases, {} requests, {} input and {} output tokens",
        cases.len(),
        cases.len() * 2,
        tokens.0,
        tokens.1
    );
    println!(
        "same shape  {same}/{}
as expected {expected}/{}",
        cases.len(),
        cases.len()
    );

    // The replies are real Codex event streams, which both sides of each
    // response translator must handle exactly alike.
    let mut responses_match = true;
    for (translator, replies) in [
        (Translator::Stream, &streams),
        (Translator::NonStream, &finals),
        (Translator::ResponsesStream, &streams),
        (Translator::ResponsesNonStream, &finals),
        (Translator::ChatStream, &streams),
        (Translator::ChatNonStream, &finals),
    ] {
        let dir = live_dir.join(translator.slug());
        let tally = check(translator, replies, upstream, work_dir, &dir, 1)?;
        println!();
        println!("{}", translator.title());
        println!("cases       the {} replies above", replies.len());
        tally.print(10, &dir);
        responses_match &= tally.different == 0;
    }
    println!();
    println!("Bodies and replies are in {}", live_dir.display());
    Ok(same == cases.len() && responses_match)
}

/// The live config suite uses this same observation and assessment for its
/// printed summary and returned verdict. Other suites keep the strict tally.
fn config_save_result(
    tally: &Tally,
    args: &Args,
    fingerprint: &baseline::Fingerprint,
    cases: usize,
    budget: Option<(&baseline::Baseline, &Path)>,
    observation_path: &Path,
) -> Result<(bool, String), Box<dyn Error>> {
    let observation = baseline::Observation {
        fingerprint: fingerprint.clone(),
        seed: args.seed,
        random: args.random,
        cases,
        selected: args.replay_config_failures.is_some(),
        different: tally.different,
        non_certification: tally.different - tally.certification_failures.len(),
        case_kinds: tally.certification_failures.clone(),
    };
    baseline::write(observation_path, &observation)?;
    let Some((budget, path)) = budget else {
        return Ok((tally.different == 0, String::new()));
    };
    let assessment = budget.assess(&observation, path)?;
    Ok((assessment.passed, assessment.markdown))
}

const WORK_DIR_MARKER: &str = ".open-ferry-parity-work-dir";
const WORK_DIR_ID: &[u8] = b"open-ferry-parity work directory v1\n";

/// The default is tool-owned, including restored caches and older run output.
/// Validate custom scratch space before setup writes or run subfolder resets.
fn prepare_work_dir(
    default_dir: &Path,
    custom_dir: Option<&Path>,
) -> Result<PathBuf, Box<dyn Error>> {
    let dir = custom_dir.unwrap_or(default_dir);
    fs::create_dir_all(dir)?;
    let marker = dir.join(WORK_DIR_MARKER);
    if custom_dir.is_none() {
        fs::write(marker, WORK_DIR_ID)?;
        return Ok(dir.to_owned());
    }
    if fs::read_dir(dir)?.next().transpose()?.is_some() {
        if fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_file())
            && fs::read(&marker).is_ok_and(|bytes| bytes == WORK_DIR_ID)
        {
            return Ok(dir.to_owned());
        }
        return Err(format!(
            "refusing parity work directory {}: it is nonempty and has no valid {WORK_DIR_MARKER} marker; choose a new or empty directory with OPEN_FERRY_PARITY_WORK_DIR",
            dir.display()
        )
        .into());
    }
    use std::io::Write as _;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)?
        .write_all(WORK_DIR_ID)?;
    Ok(dir.to_owned())
}

/// Empties `dir`, creating it if needed.
fn fresh_dir(dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)?;
    Ok(dir.to_owned())
}

enum Outcome {
    Identical,
    Equivalent(BTreeSet<Deviation>),
    Different(Vec<Difference>),
}

struct Evaluated {
    outcome: Outcome,
    /// Upstream's raw output, or its panic.
    go_output: String,
    /// Upstream's output as read for comparison.
    go_value: Option<Value>,
    rust_output: Result<Value, String>,
    certification_failure: Option<BTreeSet<String>>,
}

fn evaluate_with_counterfactual(
    translator: Translator,
    case: &Case,
    go: &GoResult,
    upstream: &Upstream,
    work_dir: &Path,
) -> Result<Evaluated, Box<dyn Error>> {
    let mut evaluated =
        evaluate_with_whole_counterfactual(translator, case, go, upstream, work_dir)?;
    if translator == Translator::ConfigSave
        && safe_case_name(&case.name)
        && matches!(evaluated.outcome, Outcome::Different(_))
        && let Ok(original_go) = serde_json::from_str::<Value>(&evaluated.go_output)
        && let Ok(rust) = &evaluated.rust_output
        && let Some(proof) =
            config_save::counterfactual::stepwise(case, &original_go, rust, |single| {
                Ok(upstream
                    .run(translator.key(), std::slice::from_ref(single), work_dir)?
                    .remove(0))
            })?
    {
        save_counterfactual(work_dir, case, &proof.evidence)?;
        evaluated.outcome = Outcome::Equivalent(proof.deviations);
    }
    if translator == Translator::ConfigSave
        && safe_case_name(&case.name)
        && matches!(evaluated.outcome, Outcome::Different(_))
        && let Ok(original_go) = serde_json::from_str::<Value>(&evaluated.go_output)
        && let Ok(rust) = &evaluated.rust_output
    {
        let proof = config_save::ablation::certify(case, &original_go, rust, |candidate| {
            Ok(upstream
                .run(translator.key(), std::slice::from_ref(candidate), work_dir)?
                .remove(0))
        })?;
        save_counterfactual(work_dir, case, &proof.evidence)?;
        if let Some(deviations) = proof.deviations {
            evaluated.outcome = Outcome::Equivalent(deviations);
        } else {
            evaluated.certification_failure = if baseline::config_output(&original_go)
                && baseline::config_output(rust)
            {
                baseline::difference_kinds(&case.options, &proof.evidence, &original_go, Some(rust))
            } else {
                None
            };
        }
    }
    Ok(evaluated)
}

fn safe_case_name(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn proof_deviations(reason: Deviation, outcome: &Outcome) -> BTreeSet<Deviation> {
    let mut deviations = BTreeSet::from([reason]);
    if let Outcome::Equivalent(found) = outcome {
        deviations.extend(found);
    }
    deviations
}

fn save_counterfactual(work_dir: &Path, case: &Case, proof: &Value) -> Result<(), Box<dyn Error>> {
    if !safe_case_name(&case.name) {
        return Err("unsafe counterfactual evidence filename".into());
    }
    let dir = work_dir.join("counterfactual");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join(format!("{}.json", case.name)),
        serde_json::to_vec_pretty(proof)?,
    )?;
    Ok(())
}

fn evaluate_with_whole_counterfactual(
    translator: Translator,
    case: &Case,
    go: &GoResult,
    upstream: &Upstream,
    work_dir: &Path,
) -> Result<Evaluated, Box<dyn Error>> {
    let mut evaluated = evaluate(translator, case, go);
    if translator == Translator::ConfigSave
        && matches!(&evaluated.outcome,Outcome::Equivalent(d) if d.contains(&Deviation::UpstreamVersionCommentLoss))
    {
        if !safe_case_name(&case.name) {
            evaluated.outcome = Outcome::Different(vec![Difference::whole(
                "safe counterfactual evidence filename".into(),
                case.name.clone(),
            )]);
            return Ok(evaluated);
        }
        if let (Ok(original_go), Ok(rust)) = (
            serde_json::from_str::<Value>(&evaluated.go_output),
            &evaluated.rust_output,
        ) && let Some(mut proof) = config_save::version::evidence(case, &original_go, rust)
        {
            if let Outcome::Equivalent(d) = &evaluated.outcome {
                proof["comparison_deviations"] =
                    json!(d.iter().map(|d| d.describe()).collect::<Vec<_>>());
            }
            save_counterfactual(work_dir, case, &proof)?;
        }
    }
    if translator != Translator::ConfigSave || !matches!(evaluated.outcome, Outcome::Different(_)) {
        return Ok(evaluated);
    }
    if !safe_case_name(&case.name) {
        evaluated.outcome = Outcome::Different(vec![Difference::whole(
            "safe counterfactual evidence filename".into(),
            case.name.clone(),
        )]);
        return Ok(evaluated);
    }
    let Some(original_go) = serde_json::from_str::<Value>(&evaluated.go_output).ok() else {
        return Ok(evaluated);
    };
    let Ok(rust) = &evaluated.rust_output else {
        return Ok(evaluated);
    };
    if let Some(proof) = config_save::folded::evidence(case, &original_go, rust) {
        save_counterfactual(work_dir, case, &proof)?;
        evaluated.outcome =
            Outcome::Equivalent(BTreeSet::from([Deviation::UpstreamFoldedScalarReencode]));
        return Ok(evaluated);
    }
    let Some(candidate) = config_save::counterfactual::rewritten(case) else {
        return Ok(evaluated);
    };
    let results = upstream.run(translator.key(), std::slice::from_ref(&candidate), work_dir)?;
    let mut owner_proof = None;
    let mut counterfactual = evaluate(translator, &candidate, &results[0]);
    let Some(counterfactual_go) = serde_json::from_str::<Value>(&counterfactual.go_output).ok()
    else {
        return Ok(evaluated);
    };
    let Some(reason) = config_save::counterfactual::reason_with_counterfactual(
        case,
        &original_go,
        &counterfactual_go,
        rust,
    ) else {
        return Ok(evaluated);
    };
    if matches!(counterfactual.outcome, Outcome::Different(_))
        && let (Ok(cf_go), Ok(cf_rust)) = (
            serde_json::from_str::<Value>(&counterfactual.go_output),
            &counterfactual.rust_output,
        )
    {
        let proof = config_save::ablation::certify(&candidate, &cf_go, cf_rust, |request| {
            Ok(upstream
                .run(translator.key(), std::slice::from_ref(request), work_dir)?
                .remove(0))
        })?;
        if let Some(deviations) = proof.deviations {
            owner_proof = Some(proof.evidence);
            counterfactual.outcome = Outcome::Equivalent(deviations);
        }
    }
    if let (Ok(rust_original), Ok(rust_rewritten)) =
        (&evaluated.rust_output, &counterfactual.rust_output)
        && config_save::counterfactual::proves(
            case,
            &candidate,
            &original_go,
            &counterfactual_go,
            matches!(
                counterfactual.outcome,
                Outcome::Identical | Outcome::Equivalent(_)
            ),
            rust_original,
            rust_rewritten,
        )
    {
        let mut proof = config_save::counterfactual::evidence(
            case,
            &candidate,
            &original_go,
            &counterfactual_go,
            rust_original,
            rust_rewritten,
        );
        if let Some(owner) = owner_proof {
            proof["counterfactual_owner_proof"] = owner;
        }
        if let Outcome::Equivalent(found) = &counterfactual.outcome {
            proof["counterfactual_deviations"] =
                json!(found.iter().map(|d| d.describe()).collect::<Vec<_>>());
            if let Some(witness) =
                config_save::version::evidence(&candidate, &counterfactual_go, rust_rewritten)
            {
                proof["version_comment_proof"] = witness;
            }
        }
        save_counterfactual(work_dir, case, &proof)?;
        evaluated.outcome = Outcome::Equivalent(proof_deviations(reason, &counterfactual.outcome));
    }
    Ok(evaluated)
}

fn evaluate(translator: Translator, case: &Case, go: &GoResult) -> Evaluated {
    let rust_output = panic::catch_unwind(AssertUnwindSafe(|| translator.run_rust(case)))
        .unwrap_or_else(|payload| Err(panic_message(&*payload)));
    evaluate_rust_output(translator, case, go, rust_output)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    format!("panic: {message}")
}

/// Use the existing per-case Rust-panic comparison path for the entire case,
/// including consumed diagnostic replays. No proof can certify a panic.
fn evaluate_panic(
    translator: Translator,
    case: &Case,
    go: &GoResult,
    payload: Box<dyn std::any::Any + Send>,
) -> Evaluated {
    evaluate_rust_output(translator, case, go, Err(panic_message(&*payload)))
}

fn evaluate_rust_output(
    translator: Translator,
    case: &Case,
    go: &GoResult,
    rust_output: Result<Value, String>,
) -> Evaluated {
    let (go_output, mut go_value) = match go {
        GoResult::Panic(message) => (format!("panic: {message}"), None),
        GoResult::Output(bytes) => (
            String::from_utf8_lossy(bytes).into_owned(),
            translator.read(case, bytes),
        ),
    };
    let mut omitted = go_value
        .as_mut()
        .map(|go| translator.drop_deliberate_omissions(case, go))
        .unwrap_or_default();
    // Our output as compared: what we add on purpose and upstream doesn't
    // have is taken out of it (the failure files keep the whole output).
    let mut filtered = None;
    if translator == Translator::ConfigDiff
        && let Ok(rust) = &rust_output
    {
        let mut rust = rust.clone();
        omitted.extend(config_diff::drop_unlogged(&mut rust));
        filtered = Some(Ok(rust));
    }
    let compared: &Result<Value, String> = filtered.as_ref().unwrap_or(&rust_output);
    if translator == Translator::ConfigSave
        && let (Some(go), Ok(rust)) = (&mut go_value, compared)
        && let Some(loss) = config_save::version::repair(case, go, rust)
    {
        omitted.push(loss);
    }
    if translator == Translator::ConfigSave
        && let (Some(go), Ok(rust)) = (&mut go_value, compared)
        && let Some(loss) = config_save::repair_lossy_plugin_reencode(case, go, rust)
    {
        omitted.push(loss);
    }

    if translator.collection_tool_name_source(case)
        && let (Some(go), Ok(rust)) = (&mut go_value, compared)
        && embedded_names::repair(case, go, rust)
    {
        omitted.push(Deviation::EmbeddedJson);
    }

    let outcome = match (&go_value, compared) {
        (Some(go), Ok(rust)) => {
            let witness = |path: &str, span: &str, text: &str, at: usize| {
                request_witness::bound_span(translator, case, rust, path, span, text, at)
            };
            let mut comparison = compare::compare_numbers_with_witness(
                go,
                rust,
                translator.embedded_json(case),
                translator.numbers(),
                translator.embedded_json_request_source(case),
                translator
                    .embedded_json_request_source(case)
                    .map(|_| &witness as &dyn Fn(&str, &str, &str, usize) -> bool),
            );
            comparison.deviations.extend(omitted);
            if !comparison.differences.is_empty() {
                Outcome::Different(comparison.differences)
            } else if !comparison.deviations.is_empty() {
                Outcome::Equivalent(comparison.deviations)
            } else {
                Outcome::Identical
            }
        }
        (go, rust) => {
            let go = match go {
                Some(_) => "(JSON output)".to_owned(),
                None if go_output.starts_with("panic: ") => go_output.clone(),
                None => format!("unreadable output: {go_output}"),
            };
            let rust = match rust {
                Ok(_) => "(JSON output)".to_owned(),
                Err(message) => message.clone(),
            };
            Outcome::Different(vec![Difference::whole(go, rust)])
        }
    };
    Evaluated {
        outcome,
        go_output,
        go_value,
        rust_output,
        certification_failure: None,
    }
}

#[derive(Default)]
struct Tally {
    identical: usize,
    equivalent: usize,
    deviations: BTreeMap<Deviation, usize>,
    different: usize,
    /// First differences grouped by path shape, with the first case showing each.
    groups: BTreeMap<String, Group>,
    known: Vec<(String, &'static str)>,
    /// Cases marked as known differences that now match.
    resolved: Vec<String>,
    failure_files: usize,
    certification_failures: BTreeMap<String, BTreeSet<String>>,
}

struct Group {
    count: usize,
    case: String,
    difference: Difference,
}

impl Tally {
    fn record(
        &mut self,
        case: &Case,
        evaluated: Evaluated,
        failures_dir: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let differences = match &evaluated.outcome {
            Outcome::Identical => {
                self.identical += 1;
                None
            }
            Outcome::Equivalent(deviations) => {
                self.equivalent += 1;
                for &deviation in deviations {
                    *self.deviations.entry(deviation).or_default() += 1;
                }
                None
            }
            Outcome::Different(differences) => Some(differences),
        };

        match (differences, case.known_difference) {
            (None, Some(reason)) if reason != cases::MAP_ORDER_DEPENDENT => {
                self.resolved.push(case.name.clone())
            }
            (None, _) => {}
            (Some(_), Some(reason)) => self.known.push((case.name.clone(), reason)),
            (Some(differences), None) => {
                self.different += 1;
                if let Some(kinds) = &evaluated.certification_failure {
                    self.certification_failures
                        .insert(case.name.clone(), kinds.clone());
                }
                let first = &differences[0];
                self.groups
                    .entry(first.shape())
                    .and_modify(|group| group.count += 1)
                    .or_insert_with(|| Group {
                        count: 1,
                        case: case.name.clone(),
                        difference: first.clone(),
                    });
                let limit = if failures_dir.ends_with("config-save") {
                    MAX_CONFIG_FAILURE_FILES
                } else {
                    MAX_FAILURE_FILES
                };
                if self.failure_files < limit {
                    self.failure_files += 1;
                    write_failure(failures_dir, case, &evaluated, differences)?;
                }
            }
        }
        Ok(())
    }

    fn print(&self, show: usize, failures_dir: &Path) {
        println!("identical   {:>6}", self.identical);
        println!("equivalent  {:>6}", self.equivalent);
        for (deviation, count) in &self.deviations {
            println!("  {:<32} {count:>6}", deviation.describe());
        }
        println!("known       {:>6}", self.known.len());
        for (name, reason) in &self.known {
            println!("  {name}: {reason}");
        }
        println!("different   {:>6}", self.different);
        for name in &self.resolved {
            println!("note: {name} now matches upstream; remove its known_difference");
        }

        if self.groups.is_empty() {
            return;
        }
        let mut groups: Vec<(&String, &Group)> = self.groups.iter().collect();
        groups.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
        println!();
        println!(
            "First differences by path ({} kinds; showing {}):",
            groups.len(),
            show.min(groups.len())
        );
        for (shape, group) in groups.into_iter().take(show) {
            let difference = &group.difference;
            println!("  {shape}  x{}  e.g. {}", group.count, group.case);
            println!("    at   {}", difference.path);
            println!("    go   {}", clip(&difference.go));
            println!("    rust {}", clip(&difference.rust));
        }
        println!();
        println!("Failing cases are in {}", failures_dir.display());
    }
}

fn write_failure(
    dir: &Path,
    case: &Case,
    evaluated: &Evaluated,
    differences: &[Difference],
) -> Result<(), Box<dyn Error>> {
    let rust_output = match &evaluated.rust_output {
        Ok(value) => value.clone(),
        Err(message) => message.as_str().into(),
    };
    let upstream_output = evaluated
        .go_value
        .clone()
        .unwrap_or_else(|| evaluated.go_output.as_str().into());
    let differences: Vec<Value> = differences
        .iter()
        .map(|d| json!({ "path": d.path, "go": d.go, "rust": d.rust }))
        .collect();
    let mut report = json!({
        "case": case.name,
        "model": case.model,
        "differences": differences,
        "request": serde_json::from_str::<Value>(&case.request)
            .unwrap_or_else(|_| case.request.as_str().into()),
        "request_text": case.request,
    });
    if !case.options.is_null() {
        report["options"] = case.options.clone();
    }
    if !case.events.is_empty() {
        report["events"] = json!(case.events);
    }
    if !case.translated_request.is_empty() {
        report["translated_request"] = json!(case.translated_request);
    }
    report["upstream_output_raw"] = Value::String(evaluated.go_output.clone());
    report["upstream_output"] = upstream_output;
    report["open_ferry_output"] = rust_output;
    fs::create_dir_all(dir)?;
    fs::write(
        dir.join(format!("{}.json", safe_failure_name(&case.name))),
        serde_json::to_string_pretty(&report)?,
    )?;
    Ok(())
}

fn safe_failure_name(name: &str) -> String {
    if safe_case_name(name) {
        return name.to_owned();
    }
    use sha2::{Digest, Sha256};
    format!(
        "unsafe-{}",
        Sha256::digest(name.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Shortens long values for the terminal, on a character boundary.
fn clip(text: &str) -> String {
    const MAX: usize = 160;
    match text.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod parallel_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_work_directory_accepts_an_unmarked_restored_go_cache() {
        let dir = std::env::temp_dir().join(format!("parity-work-default-{}", std::process::id()));
        fresh_dir(&dir).unwrap();
        fs::create_dir(dir.join("go-build")).unwrap();
        let cached = dir.join("go-build/cached-object");
        fs::write(&cached, b"restored Go cache").unwrap();
        assert!(!dir.join(WORK_DIR_MARKER).exists());
        assert_eq!(prepare_work_dir(&dir, None).unwrap(), dir);
        assert_eq!(fs::read(&cached).unwrap(), b"restored Go cache");
        assert_eq!(fs::read(dir.join(WORK_DIR_MARKER)).unwrap(), WORK_DIR_ID);
        fs::write(dir.join(WORK_DIR_MARKER), b"older marker").unwrap();
        prepare_work_dir(&dir, None).unwrap();
        assert_eq!(fs::read(dir.join(WORK_DIR_MARKER)).unwrap(), WORK_DIR_ID);
        assert_eq!(fs::read(&cached).unwrap(), b"restored Go cache");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn work_directory_accepts_new_empty_and_previously_owned_scratch() {
        let dir = std::env::temp_dir().join(format!("parity-work-owned-{}", std::process::id()));
        fresh_dir(&dir).unwrap();
        let new = dir.join("new");
        assert_eq!(prepare_work_dir(&dir, Some(&new)).unwrap(), new);
        assert_eq!(fs::read(new.join(WORK_DIR_MARKER)).unwrap(), WORK_DIR_ID);
        fs::write(new.join("previous-run"), b"old evidence").unwrap();
        prepare_work_dir(&dir, Some(&new)).unwrap();
        assert_eq!(fs::read(new.join("previous-run")).unwrap(), b"old evidence");
        let empty = dir.join("empty");
        fs::create_dir(&empty).unwrap();
        prepare_work_dir(&dir, Some(&empty)).unwrap();
        assert_eq!(fs::read(empty.join(WORK_DIR_MARKER)).unwrap(), WORK_DIR_ID);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn work_directory_refuses_unowned_files_without_changing_them() {
        let dir = std::env::temp_dir().join(format!("parity-work-unowned-{}", std::process::id()));
        fresh_dir(&dir).unwrap();
        for sub in ["counterfactual", "failures", "live"] {
            fs::create_dir(dir.join(sub)).unwrap();
            fs::write(dir.join(sub).join("keep"), sub).unwrap();
        }
        for marker in [None, Some(b"unrelated marker".as_slice())] {
            if let Some(bytes) = marker {
                fs::write(dir.join(WORK_DIR_MARKER), bytes).unwrap();
            }
            let error = prepare_work_dir(&dir, Some(&dir)).unwrap_err().to_string();
            assert!(error.contains("refusing parity work directory"));
            assert!(error.contains("OPEN_FERRY_PARITY_WORK_DIR"));
            for sub in ["counterfactual", "failures", "live"] {
                assert_eq!(
                    fs::read(dir.join(sub).join("keep")).unwrap(),
                    sub.as_bytes()
                );
            }
            assert_eq!(fs::read(dir.join(WORK_DIR_MARKER)).ok().as_deref(), marker);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn counterfactual_deviations_include_comparison_classes() {
        let found = Outcome::Equivalent(BTreeSet::from([Deviation::EmbeddedJson]));
        assert_eq!(
            proof_deviations(Deviation::UpstreamBlockScalarCounterfactual, &found),
            BTreeSet::from([
                Deviation::EmbeddedJson,
                Deviation::UpstreamBlockScalarCounterfactual
            ])
        );
    }
    #[test]
    fn evidence_is_fresh_and_unsafe_names_stay_in_the_run() {
        let dir = std::env::temp_dir().join(format!("parity-proof-hygiene-{}", std::process::id()));
        fresh_dir(&dir).unwrap();
        fs::write(dir.join("stale.json"), b"old").unwrap();
        fresh_dir(&dir).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!safe_case_name("../escape"));
        assert!(!safe_failure_name("../escape").contains(['/', '\\']));
        let case = Case::new("../escape", "", "")
            .with_options(json!({"file":"debug: false\n","steps":[]}));
        // The guard precedes all upstream access, so no process is started.
        assert!(save_counterfactual(&dir, &case, &json!({})).is_err());
        let e = evaluate(
            Translator::ConfigSave,
            &case,
            &GoResult::Output(br#"{"files":[]}"#.to_vec()),
        );
        let e = Evaluated {
            outcome: Outcome::Different(vec![Difference::whole(
                "safe name".into(),
                case.name.clone(),
            )]),
            ..e
        };
        let mut tally = Tally::default();
        tally.record(&case, e, &dir).unwrap();
        assert_eq!(tally.different, 1);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn suite_filter_is_repeatable_defaults_to_all_and_rejects_unknown_names() {
        let parse = |flags: &[&str]| {
            Args::parse(
                ["--upstream", "."]
                    .into_iter()
                    .chain(flags.iter().copied())
                    .map(str::to_owned),
            )
            .unwrap()
        };
        let catalog = || {
            vec![
                (Translator::ConfigSave, Vec::new(), Vec::new()),
                (Translator::ConfigDiff, Vec::new(), Vec::new()),
                (Translator::Usage, Vec::new(), Vec::new()),
            ]
        };
        let mut all = catalog();
        filter_suites(&mut all, &parse(&[]).suites).unwrap();
        assert_eq!(all.len(), 3);
        let selected = parse(&[
            "--suite",
            "config-save",
            "--suite",
            "usage",
            "--suite",
            "config-save",
        ]);
        let mut suites = catalog();
        filter_suites(&mut suites, &selected.suites).unwrap();
        assert_eq!(
            suites
                .iter()
                .map(|(translator, _, _)| translator.slug())
                .collect::<Vec<_>>(),
            ["config-save", "usage"]
        );
        let mut unknown = catalog();
        assert_eq!(
            filter_suites(
                &mut unknown,
                &["config-sav".to_owned()].into_iter().collect()
            ),
            Err("unknown suite \"config-sav\"; use a suite directory slug".into())
        );
        assert_eq!(unknown.len(), 3);
        assert!(
            Args::parse(
                ["--upstream", ".", "--suite"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .is_err()
        );
    }

    #[test]
    fn selected_suite_generates_its_cases_and_validates_the_real_catalog() {
        let mut args = Args::parse(
            ["--upstream", ".", "--suite", "config-save", "--random", "2"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let suites = selected_suites(&args).unwrap();
        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].0.slug(), "config-save");
        assert!(!suites[0].1.is_empty());
        assert_eq!(suites[0].2.len(), 2);
        args.suites.insert("unknown-suite".into());
        assert!(selected_suites(&args).is_err());
    }

    #[test]
    fn incremental_baseline_cli_needs_observations_but_no_upstream() {
        let flags = [
            "--write-baseline",
            "out.json",
            "--baseline",
            "existing.json",
        ];
        assert!(Args::parse(flags.into_iter().map(str::to_owned)).is_err());
        let parsed = Args::parse(
            flags
                .into_iter()
                .chain(["--baseline-observation", "nightly.json"])
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(parsed.write_baseline.unwrap(), Path::new("out.json"));
        assert_eq!(parsed.baseline.unwrap(), Path::new("existing.json"));
        assert!(parsed.upstream.as_os_str().is_empty());
    }

    #[test]
    fn review_r13_live_config_verdict_follows_each_ratchet_failure() {
        let value: Value =
            serde_json::from_str(include_str!("../baseline/config-save.json")).unwrap();
        let budget: baseline::Baseline = serde_json::from_value(value.clone()).unwrap();
        let fingerprint = serde_json::from_value(value["fingerprint"].clone()).unwrap();
        let args = Args::parse(
            ["--upstream", ".", "--seed", "30", "--random", "5000"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("parity-live-verdict-{}", std::process::id()));
        let path = dir.join("observation.json");
        for (cases, different, kind, expected, message) in [
            (
                10,
                1,
                Some("check-3/owner_budget/save"),
                false,
                "rate exceeds baseline",
            ),
            (1000, 1, Some("new/kind/save"), false, "new difference kind"),
            (1000, 1, None, false, "non-certification failures"),
            (1000, 0, None, true, "Ratchet verdict: **PASS**"),
        ] {
            let tally = Tally {
                different,
                certification_failures: kind
                    .map(|kind| BTreeMap::from([("case".into(), BTreeSet::from([kind.into()]))]))
                    .unwrap_or_default(),
                ..Tally::default()
            };
            let (matched, markdown) = config_save_result(
                &tally,
                &args,
                &fingerprint,
                cases,
                Some((&budget, Path::new("baseline.json"))),
                &path,
            )
            .unwrap();
            assert_eq!(matched, expected, "{markdown}");
            assert!(markdown.contains(message), "{markdown}");
            let saved: baseline::Observation = baseline::read(&path).unwrap();
            assert_eq!(
                saved.non_certification,
                usize::from(different > 0 && kind.is_none())
            );
            let (strict, markdown) =
                config_save_result(&tally, &args, &fingerprint, cases, None, &path).unwrap();
            assert_eq!(strict, different == 0);
            assert!(markdown.is_empty());
        }
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn unrelated_marker_text_does_not_suppress_resolved_advice() {
        let mut case = Case::new("resolved-marker", "", "{}");
        case.known_difference = Some("unrelated text mentioning map order");
        let evaluated = Evaluated {
            outcome: Outcome::Identical,
            go_output: "{}".into(),
            go_value: Some(json!({})),
            rust_output: Ok(json!({})),
            certification_failure: None,
        };
        let mut tally = Tally::default();
        tally
            .record(&case, evaluated, Path::new("unused-no-failure"))
            .unwrap();
        assert_eq!(tally.resolved, ["resolved-marker"]);
    }

    #[test]
    fn suite_filter_rejects_unknown_names_during_argument_parsing() {
        let flags = [
            "--upstream",
            "missing-checkout",
            "--go",
            "missing-go",
            "--random",
            "18446744073709551615",
            "--suite",
            "typo",
        ];
        assert_eq!(
            Args::parse(flags.into_iter().map(str::to_owned))
                .err()
                .unwrap(),
            r#"unknown suite "typo"; use a suite directory slug"#
        );
    }

    #[test]
    fn suite_filter_rejects_live_mode_without_running_it() {
        let flags = [
            "--upstream",
            ".",
            "--live",
            "https://example.invalid",
            "--model",
            "m",
        ];
        assert!(Args::parse(flags.into_iter().map(str::to_owned)).is_ok());
        assert_eq!(
            Args::parse(
                flags
                    .into_iter()
                    .chain(["--suite", "config-save"])
                    .map(str::to_owned)
            )
            .err()
            .unwrap(),
            "--suite cannot be combined with --live"
        );
    }

    // Go iterates a map of aliases that overwrite the same destination key.
    // It can match or differ from Rust's stable order with the same seed; the
    // actual checked-in fixture must remain accepted in either outcome.
    #[test]
    fn map_order_case_cannot_be_an_unexpected_difference() {
        let case = interactions::suites(1, 0)
            .into_iter()
            .flat_map(|(_, cases, _)| cases)
            .find(|case| case.name == "settings-for-the-same-field")
            .expect("the ambiguous generation-settings fixture is still covered");
        assert_eq!(case.known_difference, Some(cases::MAP_ORDER_DEPENDENT));
        let dir = std::env::temp_dir().join(format!("parity-map-order-{}", std::process::id()));
        let mut tally = Tally::default();
        for outcome in [
            Outcome::Identical,
            Outcome::Equivalent(BTreeSet::new()),
            Outcome::Different(vec![Difference::whole(
                "one map order".into(),
                "another".into(),
            )]),
        ] {
            tally
                .record(
                    &case,
                    Evaluated {
                        outcome,
                        go_output: String::new(),
                        go_value: None,
                        rust_output: Ok(Value::Null),
                        certification_failure: None,
                    },
                    &dir,
                )
                .unwrap();
        }
        assert_eq!(tally.identical, 1);
        assert_eq!(tally.equivalent, 1);
        assert_eq!(tally.known.len(), 1);
        assert_eq!(tally.different, 0);
        assert_eq!(tally.failure_files, 0);
        assert!(
            tally.resolved.is_empty(),
            "a lucky map order must not advise removing its marker"
        );
        assert!(!dir.exists());
    }

    // Not upstream's: an unexpected difference fails the tally and its saved
    // input keeps every Case field needed to make a regression fixture.
    #[test]
    fn failure_artifact_keeps_complete_input() {
        let dir = std::env::temp_dir().join(format!("parity-failure-{}", std::process::id()));
        let mut case = Case::response("artifact-input", " {\"input\": 1e2} ", vec!["event".into()]);
        case.model = "synthetic-model".into();
        case.translated_request = "{\"translated\":true}".into();
        case.options = json!({"stream": true});
        let evaluated = || Evaluated {
            outcome: Outcome::Different(vec![Difference::whole("go".into(), "rust".into())]),
            go_output: "{\"side\":\"go\"}".into(),
            go_value: Some(json!({"side": "go"})),
            rust_output: Ok(json!({"side": "rust"})),
            certification_failure: None,
        };
        let mut tally = Tally::default();
        tally.record(&case, evaluated(), &dir).unwrap();
        assert_eq!(tally.different, 1);
        let report: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("artifact-input.json")).unwrap())
                .unwrap();
        assert_eq!(report["case"], case.name);
        assert_eq!(report["model"], case.model);
        assert_eq!(report["request_text"], case.request);
        assert_eq!(report["translated_request"], case.translated_request);
        assert_eq!(report["events"], json!(case.events));
        assert_eq!(report["options"], case.options);
        assert_eq!(report["upstream_output"], json!({"side": "go"}));
        assert_eq!(report["open_ferry_output"], json!({"side": "rust"}));

        // Only the checked-in marker accepts a difference; it neither adds
        // another failure nor emits a file for that accepted case.
        case.name = "accepted-difference".into();
        case.known_difference = Some("synthetic test marker");
        tally.record(&case, evaluated(), &dir).unwrap();
        assert_eq!(tally.different, 1);
        assert_eq!(tally.known.len(), 1);
        assert!(!dir.join("accepted-difference.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod embedded_json_paths;
