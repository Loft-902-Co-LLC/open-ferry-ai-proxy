//! Differential tests of open-ferry's translators against upstream CLIProxyAPI.
//!
//! Runs the same input through upstream's Go translators and our Rust ports
//! and compares the results. See README.md.

mod cases;
mod compare;
mod generate;
mod live;
mod signature;
mod translator;
mod upstream;

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
  --show <n>     kinds of difference to print (default 10)
  --go <path>    Go binary (default: go on PATH)
  --live <url>   instead, send a few requests through a running CLIProxyAPI;
                 needs --model and the client API key in OPEN_FERRY_PARITY_API_KEY
  --model <name> model for --live";

/// Failing cases written to disk; the rest are only counted.
const MAX_FAILURE_FILES: usize = 200;

struct Args {
    upstream: PathBuf,
    random: usize,
    seed: u64,
    show: usize,
    go: PathBuf,
    live: Option<String>,
    model: Option<String>,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut upstream = None;
        let mut parsed = Self {
            upstream: PathBuf::new(),
            random: 5000,
            seed: 1,
            show: 10,
            go: "go".into(),
            live: None,
            model: None,
        };
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--upstream" => upstream = Some(PathBuf::from(value()?)),
                "--random" => parsed.random = parse_number(&flag, &value()?)?,
                "--seed" => parsed.seed = parse_number(&flag, &value()?)?,
                "--show" => parsed.show = parse_number(&flag, &value()?)?,
                "--go" => parsed.go = value()?.into(),
                "--live" => parsed.live = Some(value()?),
                "--model" => parsed.model = Some(value()?),
                "-h" | "--help" => return Err(String::new()),
                _ => return Err(format!("unknown argument {flag}")),
            }
        }
        parsed.upstream = upstream.ok_or("--upstream is required")?;
        if parsed.live.is_some() && parsed.model.is_none() {
            return Err("--live needs --model".into());
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

/// Returns whether every case matched upstream, allowing documented deviations.
fn run(args: &Args) -> Result<bool, Box<dyn Error>> {
    let work_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/parity is two levels below the workspace root")
        .join("target")
        .join("parity");
    fs::create_dir_all(&work_dir)?;
    let upstream = Upstream::build(&args.upstream, &args.go, &work_dir)?;
    if let (Some(url), Some(model)) = (&args.live, &args.model) {
        return run_live(url, model, &upstream, &work_dir);
    }

    let failures_dir = fresh_dir(&work_dir.join("failures"))?;
    let (seed, random) = (args.seed, args.random);
    let (streams, finals) = generate::response::cases(seed, random);
    let (responses_streams, responses_finals) = generate::responses::event_cases(seed, random);
    let (chat_streams, chat_finals) = generate::chat::event_cases(seed, random);
    let (claude_chat_streams, claude_chat_finals) =
        generate::claude_chat::event_cases(seed, random);
    let (claude_responses_streams, claude_responses_finals) =
        generate::claude_responses::event_cases(seed, random);
    let (registry_streams, registry_finals) = generate::registry::response_cases(seed, random);
    let suites = [
        (
            Translator::Request,
            cases::hand_written(),
            generate::cases(seed, random),
        ),
        (Translator::Stream, cases::hand_written_streams(), streams),
        (Translator::NonStream, cases::hand_written_finals(), finals),
        (
            Translator::ResponsesRequest,
            cases::responses::requests(),
            generate::responses::request_cases(seed, random),
        ),
        (
            Translator::ResponsesStream,
            cases::responses::streams(),
            responses_streams,
        ),
        (
            Translator::ResponsesNonStream,
            cases::responses::finals(),
            responses_finals,
        ),
        (
            Translator::ChatRequest,
            cases::chat::requests(),
            generate::chat::request_cases(seed, random),
        ),
        (Translator::ChatStream, cases::chat::streams(), chat_streams),
        (
            Translator::ChatNonStream,
            cases::chat::finals(),
            chat_finals,
        ),
        (
            Translator::RequestCompat,
            cases::hand_written(),
            generate::cases(seed, random),
        ),
        (
            Translator::ClaudeChatRequest,
            cases::claude_chat::requests(),
            generate::claude_chat::request_cases(seed, random),
        ),
        (
            Translator::ClaudeChatRequestCompat,
            cases::claude_chat::requests(),
            generate::claude_chat::request_cases(seed, random),
        ),
        (
            Translator::ClaudeChatStream,
            cases::claude_chat::streams(),
            claude_chat_streams,
        ),
        (
            Translator::ClaudeChatNonStream,
            cases::claude_chat::finals(),
            claude_chat_finals,
        ),
        (
            Translator::ClaudeResponsesRequest,
            cases::claude_responses::requests(),
            generate::claude_responses::request_cases(seed, random),
        ),
        (
            Translator::ClaudeResponsesRequestCompat,
            cases::claude_responses::requests(),
            generate::claude_responses::request_cases(seed, random),
        ),
        (
            Translator::ClaudeResponsesStream,
            cases::claude_responses::streams(),
            claude_responses_streams,
        ),
        (
            Translator::ClaudeResponsesNonStream,
            cases::claude_responses::finals(),
            claude_responses_finals,
        ),
        (
            Translator::SignatureInspect,
            cases::signature::inspect(),
            generate::signature::inspect_cases(seed, random),
        ),
        (
            Translator::ClaudeMessagesSignatures,
            cases::signature::claude_messages(),
            generate::signature::claude_messages_cases(seed, random),
        ),
        (
            Translator::GeminiSignatures,
            cases::signature::gemini(),
            generate::signature::gemini_cases(seed, random),
        ),
        (
            Translator::RegistryRequest,
            cases::registry::requests(),
            generate::registry::request_cases(seed, random),
        ),
        (
            Translator::RegistryStream,
            cases::registry::streams(),
            registry_streams,
        ),
        (
            Translator::RegistryNonStream,
            cases::registry::finals(),
            registry_finals,
        ),
        (
            Translator::RegistryLookup,
            cases::registry::lookups(),
            generate::registry::lookup_cases(seed, random),
        ),
        (
            Translator::CompletionsRequest,
            cases::completions::requests(),
            generate::completions::request_cases(seed, random),
        ),
        (
            Translator::CompletionsResponse,
            cases::completions::responses(),
            generate::completions::response_cases(seed, random),
        ),
        (
            Translator::CompletionsStreamChunk,
            cases::completions::stream_chunks(),
            generate::completions::chunk_cases(seed, random),
        ),
    ];

    println!("open-ferry parity");
    println!(
        "upstream    {} at {} ({})",
        upstream.dir.display(),
        upstream.version,
        upstream.commit
    );
    let mut all_match = true;
    for (translator, mut cases, random) in suites {
        let hand_written = cases.len();
        cases.extend(random);
        let started = Instant::now();
        let dir = failures_dir.join(translator.slug());
        let tally = check(translator, &cases, &upstream, &work_dir, &dir)?;
        println!();
        println!("{}", translator.title());
        println!(
            "cases       {hand_written} hand-written + {} random (seed {}), {:.1?}",
            args.random,
            args.seed,
            started.elapsed()
        );
        tally.print(args.show, &dir);
        all_match &= tally.different == 0;
    }
    Ok(all_match)
}

/// Runs `cases` through both sides of `translator`, writing failing cases to `dir`.
fn check(
    translator: Translator,
    cases: &[Case],
    upstream: &Upstream,
    work_dir: &Path,
    dir: &Path,
) -> Result<Tally, Box<dyn Error>> {
    let go_results = upstream.run(translator.key(), cases, work_dir)?;
    // Rust panics are caught and reported per case, so keep them off stderr.
    panic::set_hook(Box::new(|_| {}));
    let mut tally = Tally::default();
    let recorded = cases
        .iter()
        .zip(&go_results)
        .try_for_each(|(case, go)| tally.record(case, evaluate(translator, case, go), dir));
    let _ = panic::take_hook();
    recorded.map(|()| tally)
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
        let tally = check(translator, replies, upstream, work_dir, &dir)?;
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
}

fn evaluate(translator: Translator, case: &Case, go: &GoResult) -> Evaluated {
    let rust_output = panic::catch_unwind(AssertUnwindSafe(|| translator.run_rust(case)))
        .unwrap_or_else(|payload| {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(format!("panic: {message}"))
        });

    let (go_output, mut go_value) = match go {
        GoResult::Panic(message) => (format!("panic: {message}"), None),
        GoResult::Output(bytes) => (
            String::from_utf8_lossy(bytes).into_owned(),
            translator.read(case, bytes),
        ),
    };
    let omitted = go_value
        .as_mut()
        .and_then(|go| translator.drop_deliberate_omissions(case, go));

    let outcome = match (&go_value, &rust_output) {
        (Some(go), Ok(rust)) => {
            let mut comparison = compare::compare(go, rust, translator.embedded_json(case));
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
            (None, Some(_)) => self.resolved.push(case.name.clone()),
            (None, _) => {}
            (Some(_), Some(reason)) => self.known.push((case.name.clone(), reason)),
            (Some(differences), None) => {
                self.different += 1;
                let first = &differences[0];
                self.groups
                    .entry(first.shape())
                    .and_modify(|group| group.count += 1)
                    .or_insert_with(|| Group {
                        count: 1,
                        case: case.name.clone(),
                        difference: first.clone(),
                    });
                if self.failure_files < MAX_FAILURE_FILES {
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
    report["upstream_output"] = upstream_output;
    report["open_ferry_output"] = rust_output;
    fs::create_dir_all(dir)?;
    fs::write(
        dir.join(format!("{}.json", case.name)),
        serde_json::to_string_pretty(&report)?,
    )?;
    Ok(())
}

/// Shortens long values for the terminal, on a character boundary.
fn clip(text: &str) -> String {
    const MAX: usize = 160;
    match text.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}
