//! Benchmarks open-ferry against CLIProxyAPI. Both run in turn in front of
//! the same fake upstream on 127.0.0.1, with the same config, and take the
//! same requests from a load generator in this process. See
//! `docs/benchmarks.md` for what it measures and how to read the results.

mod answer;
mod body;
mod fake;
mod load;
mod ports;
mod proxy;
mod report;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use body::Format;
use fake::Counts;
use load::{Percentiles, Request, Stop};
use ports::PortRule;
use proxy::{DeadPort, MemorySampler, Running, Setup, Stage, Target, Usage};
use report::{FakeSeen, Machine, Measured, ProxyResult, Settings, ShortRow};

const USAGE: &str = "\
Usage: open-ferry-bench [options]

Runs open-ferry's release build and CLIProxyAPI, one after the other, in front
of a fake upstream on 127.0.0.1, and prints the results as Markdown.

Options:
  --upstream <dir>        a CLIProxyAPI checkout to build CLIProxyAPI from
  --cliproxyapi <file>    a CLIProxyAPI binary to run instead of building one
  --go <file>             the go command to build with (default: go)
  --open-ferry <file>     open-ferry's binary (default: this workspace's
                          target/release/open-ferry)
  --work <dir>            where the runs and the CLIProxyAPI build go
                          (default: this workspace's target/bench)
  --port <n>              the port each proxy listens on, on 127.0.0.1
                          (default: 18317)
  --delay-ms <n>          the fake upstream's wait before each answer (default: 20)
  --duration <secs>       how long each load level runs (default: 10)
  --concurrency <list>    the numbers of clients at once (default: 1,16,64)
  --starts <n>            starts measured for the start time (default: 5)
  --long-requests <n>     how many times each kind of long conversation
                          request is sent (default: 30)
  --only <name>           run only open-ferry or only cliproxyapi
  --machine-note <text>   what the machine is, for the report: its cloud VM
                          size and region, say
  --prepare               build CLIProxyAPI, check both binaries, and exit
                          without measuring anything
  --out <file>            also write the results to <file>, replacing the part
                          between its bench-results markers
  -h, --help              show this
";

struct Args {
    upstream: Option<PathBuf>,
    cliproxyapi: Option<PathBuf>,
    go: PathBuf,
    open_ferry: PathBuf,
    work: PathBuf,
    port: u16,
    delay: Duration,
    duration: Duration,
    concurrency: Vec<usize>,
    starts: usize,
    long_requests: usize,
    only: Option<String>,
    machine_note: Option<String>,
    prepare: bool,
    out: Option<PathBuf>,
}

/// This workspace's root, from where this crate sits in it.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn parse_args() -> Result<Option<Args>, Box<dyn Error>> {
    let mut parsed = Args {
        upstream: None,
        cliproxyapi: None,
        go: PathBuf::from("go"),
        open_ferry: workspace()
            .join("target")
            .join("release")
            .join(format!("open-ferry{}", std::env::consts::EXE_SUFFIX)),
        work: workspace().join("target").join("bench"),
        port: 18317,
        delay: Duration::from_millis(20),
        duration: Duration::from_secs(10),
        concurrency: vec![1, 16, 64],
        starts: 5,
        long_requests: 30,
        only: None,
        machine_note: None,
        prepare: false,
        out: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{arg} needs a value\n\n{USAGE}"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--upstream" => parsed.upstream = Some(value()?.into()),
            "--cliproxyapi" => parsed.cliproxyapi = Some(value()?.into()),
            "--go" => parsed.go = value()?.into(),
            "--open-ferry" => parsed.open_ferry = value()?.into(),
            "--work" => parsed.work = value()?.into(),
            "--port" => parsed.port = value()?.parse()?,
            "--delay-ms" => parsed.delay = Duration::from_millis(value()?.parse()?),
            "--duration" => parsed.duration = Duration::from_secs(value()?.parse()?),
            "--concurrency" => {
                parsed.concurrency = value()?
                    .split(',')
                    .map(|n| n.trim().parse::<usize>())
                    .collect::<Result<_, _>>()?;
            }
            "--starts" => parsed.starts = value()?.parse()?,
            "--long-requests" => parsed.long_requests = value()?.parse()?,
            "--only" => parsed.only = Some(value()?.to_ascii_lowercase()),
            "--machine-note" => {
                let note = value()?.trim().to_owned();
                parsed.machine_note = (!note.is_empty()).then_some(note);
            }
            "--prepare" => parsed.prepare = true,
            "--out" => parsed.out = Some(value()?.into()),
            other => return Err(format!("unknown option {other}\n\n{USAGE}").into()),
        }
    }
    if parsed.port == 0 || parsed.port == 8317 || parsed.port == 8318 {
        return Err(format!(
            "port {} won't do: pick one other than 0, 8317 and 8318 with --port (8317 is \
             both proxies' default, which a proxy in use may hold)",
            parsed.port
        )
        .into());
    }
    if parsed.concurrency.is_empty() || parsed.concurrency.contains(&0) {
        return Err("--concurrency needs numbers above 0".into());
    }
    if let Some(only) = &parsed.only
        && only != "open-ferry"
        && only != "cliproxyapi"
    {
        return Err(format!("--only takes open-ferry or cliproxyapi, not {only}").into());
    }
    Ok(Some(parsed))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&args.work)?;
    let mut targets = Vec::new();
    let mut go = None;
    if args.only.as_deref() != Some("cliproxyapi") {
        targets.push(open_ferry(&args.open_ferry)?);
    }
    if args.only.as_deref() != Some("open-ferry") {
        let (target, version) = cliproxyapi(&args)?;
        go = version;
        targets.push(target);
    }
    if args.prepare {
        for target in &targets {
            eprintln!(
                "{}: {}, at {}",
                target.name,
                target.version,
                target.binary.display()
            );
        }
        eprintln!("Ready to run.");
        return Ok(());
    }

    let rules = PortRule::for_this_os();
    eprintln!("Watching the CPUs for a second before starting...");
    let machine = Machine::read(args.machine_note.clone());
    let fake = fake::start(args.delay, rules.window).await?;
    let dead_proxy = DeadPort::new()?;
    let setup = Setup {
        port: args.port,
        upstream: fake.addr,
        dead_proxy: dead_proxy.port,
        runs: args.work.join("runs"),
    };
    let mut results = Vec::new();
    for target in &targets {
        results.push(bench(target, &setup, &args, &fake.counts, &rules).await?);
    }

    let long = |format: Format| {
        (
            body::long(format, proxy::OPENAI_MODEL, false).len(),
            body::long_messages(format),
        )
    };
    let settings = Settings {
        delay: args.delay,
        duration: args.duration,
        concurrency: args.concurrency.clone(),
        starts: args.starts,
        long_requests: args.long_requests,
        ports: rules,
        long_chat: long(Format::Chat),
        long_claude: long(Format::Claude),
        long_responses: long(Format::Responses),
        rustc: command_output(Command::new("rustc").arg("-V").current_dir(workspace())),
        go,
        debug: cfg!(debug_assertions),
    };
    let counts = &fake.counts;
    let seen = FakeSeen {
        answered: counts.answered(),
        other: counts.other.load(Ordering::Relaxed),
        last_other: last(&counts.last_other),
        refused: counts.refused.load(Ordering::Relaxed),
        last_refused: last(&counts.last_refused),
        mean_delay: counts.mean_delay(),
    };
    let text = report::render(&machine, &settings, &results, &seen);
    print!("{text}");
    if let Some(out) = &args.out {
        report::write(out, &text)?;
        eprintln!("Results written to {}", out.display());
    }
    Ok(())
}

fn last(of: &std::sync::Mutex<Option<String>>) -> Option<String> {
    of.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// open-ferry's binary, with the commit of this checkout when it's the
/// default one.
fn open_ferry(binary: &Path) -> Result<Target, Box<dyn Error>> {
    if !binary.is_file() {
        return Err(format!(
            "no open-ferry binary at {}; build it with `cargo build --release -p open-ferry` \
             or pass --open-ferry",
            binary.display()
        )
        .into());
    }
    let default = binary.starts_with(workspace());
    let version = if default {
        let commit = git(
            &workspace(),
            &["describe", "--always", "--dirty", "--exclude", "*"],
        )
        .unwrap_or_else(|| "an unknown commit".to_owned());
        format!("release build of commit `{commit}`")
    } else {
        file_name(binary)
    };
    Ok(Target {
        name: "open-ferry",
        binary: binary.to_owned(),
        version,
    })
}

/// CLIProxyAPI's binary, and the Go version it was built with when known:
/// the binary given, or one built from the checkout into the work directory.
/// The build runs in the checkout and changes nothing in it. A build from
/// before is used again when it was of the same clean commit with the same
/// Go and flags.
fn cliproxyapi(args: &Args) -> Result<(Target, Option<String>), Box<dyn Error>> {
    if let Some(binary) = &args.cliproxyapi {
        if !binary.is_file() {
            return Err(format!("no CLIProxyAPI binary at {}", binary.display()).into());
        }
        let go = command_output(Command::new(&args.go).arg("version").arg(binary))
            .and_then(|text| go_of_binary(&text));
        let target = Target {
            name: "CLIProxyAPI",
            binary: binary.to_owned(),
            version: file_name(binary),
        };
        return Ok((target, go));
    }
    let Some(upstream) = &args.upstream else {
        return Err(
            "pass --upstream <CLIProxyAPI checkout>, --cliproxyapi <binary> or --only open-ferry"
                .into(),
        );
    };
    let describe = git(upstream, &["describe", "--tags", "--always", "--dirty"])
        .ok_or_else(|| format!("{} isn't a git checkout of CLIProxyAPI", upstream.display()))?;
    let commit = git(upstream, &["rev-parse", "HEAD"]).unwrap_or_default();
    let go = command_output(Command::new(&args.go).args(["env", "GOVERSION"]));
    let go_words = go.as_deref().unwrap_or("an unknown Go version");
    let binary = args
        .work
        .join(format!("cliproxyapi{}", std::env::consts::EXE_SUFFIX));
    // The flags of upstream's release builds, but without cgo, which
    // only its plugin loader needs.
    let ldflags = format!("-s -w -X main.Version={describe} -X main.Commit={commit}");
    let short = commit.get(..12).unwrap_or(&commit);
    let target = Target {
        name: "CLIProxyAPI",
        binary: binary.clone(),
        version: format!("{describe} (commit `{short}`), built with {go_words}"),
    };

    let stamp_file = args.work.join("cliproxyapi.stamp");
    let stamp = format!("{describe}\n{commit}\n{go_words}\nCGO_ENABLED=0 {ldflags}\n");
    let reusable = go.is_some()
        && !describe.ends_with("-dirty")
        && binary.is_file()
        && fs::read_to_string(&stamp_file).is_ok_and(|built| built == stamp);
    if reusable {
        eprintln!("Using the build of CLIProxyAPI {describe} with {go_words} from before.");
        return Ok((target, go));
    }
    let _ = fs::remove_file(&stamp_file);
    eprintln!("Building CLIProxyAPI {describe} with {go_words}...");
    let status = Command::new(&args.go)
        .current_dir(upstream)
        .env("GOTOOLCHAIN", "local")
        .env("CGO_ENABLED", "0")
        .args(["build", "-buildvcs=false", "-ldflags", &ldflags, "-o"])
        .arg(&binary)
        .arg("./cmd/server")
        .status()
        .map_err(|err| format!("couldn't run {}: {err}", args.go.display()))?;
    if !status.success() {
        return Err(format!("building CLIProxyAPI failed ({status})").into());
    }
    fs::write(&stamp_file, stamp)?;
    Ok((target, go))
}

/// The Go version in what `go version <binary>` prints, `<path>: go1.26.4`.
fn go_of_binary(text: &str) -> Option<String> {
    let (_, version) = text.trim().rsplit_once(": ")?;
    version.starts_with("go").then(|| version.to_owned())
}

fn file_name(binary: &Path) -> String {
    binary
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    command_output(Command::new("git").arg("-C").arg(dir).args(args))
}

fn command_output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// A kind of request: what the client sends, and where the proxy has to
/// send it on.
struct Kind {
    name: &'static str,
    /// The client's format.
    format: Format,
    /// The model asked for, which picks the provider.
    model: &'static str,
    stream: bool,
    /// The provider's format, which the fake upstream has to get the
    /// request in: [`Format::Chat`] for the OpenAI-compatible provider, or
    /// [`Format::Claude`].
    upstream: Format,
}

const fn kind(name: &'static str, format: Format, stream: bool, upstream: Format) -> Kind {
    let model = match upstream {
        Format::Claude => proxy::CLAUDE_MODEL,
        Format::Chat | Format::Responses => proxy::OPENAI_MODEL,
    };
    Kind {
        name,
        format,
        model,
        stream,
        upstream,
    }
}

/// The requests: each provider of the config in its own format, then the
/// three translations both proxies make, each streamed and not.
const KINDS: [Kind; 10] = [
    kind("Chat Completions", Format::Chat, false, Format::Chat),
    kind(
        "Chat Completions, streamed",
        Format::Chat,
        true,
        Format::Chat,
    ),
    kind("Claude Messages", Format::Claude, false, Format::Claude),
    kind(
        "Claude Messages, streamed",
        Format::Claude,
        true,
        Format::Claude,
    ),
    kind(
        "Chat Completions → Claude Messages",
        Format::Chat,
        false,
        Format::Claude,
    ),
    kind(
        "Chat Completions → Claude Messages, streamed",
        Format::Chat,
        true,
        Format::Claude,
    ),
    kind(
        "Claude Messages → Chat Completions",
        Format::Claude,
        false,
        Format::Chat,
    ),
    kind(
        "Claude Messages → Chat Completions, streamed",
        Format::Claude,
        true,
        Format::Chat,
    ),
    kind(
        "Responses → Chat Completions",
        Format::Responses,
        false,
        Format::Chat,
    ),
    kind(
        "Responses → Chat Completions, streamed",
        Format::Responses,
        true,
        Format::Chat,
    ),
];

fn request(port: u16, kind: &Kind, body: bytes::Bytes) -> Request {
    Request {
        url: format!("http://127.0.0.1:{port}{}", kind.format.path()),
        format: kind.format,
        stream: kind.stream,
        key: proxy::CLIENT_KEY.to_owned(),
        body,
    }
}

/// Requests of each kind sent before measuring, one after another.
const WARM_UP: u64 = 20;

/// What the fake upstream got, to tell where a proxy sent requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Tally {
    chat: u64,
    messages: u64,
    refused: u64,
    other: u64,
}

impl Tally {
    fn of(counts: &Counts) -> Self {
        Self {
            chat: counts.chat.load(Ordering::Relaxed),
            messages: counts.messages.load(Ordering::Relaxed),
            refused: counts.refused.load(Ordering::Relaxed),
            other: counts.other.load(Ordering::Relaxed),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            chat: self.chat.saturating_sub(before.chat),
            messages: self.messages.saturating_sub(before.messages),
            refused: self.refused.saturating_sub(before.refused),
            other: self.other.saturating_sub(before.other),
        }
    }
}

/// What's wrong with where a proxy sent [`WARM_UP`] requests of `kind`, if
/// anything: each has to reach the provider's own endpoint, in the
/// provider's format, and nothing else may reach the fake upstream.
fn routing_problem(kind: &Kind, sent: Tally) -> Option<String> {
    let (to, elsewhere) = match kind.upstream {
        Format::Claude => (sent.messages, sent.chat),
        Format::Chat | Format::Responses => (sent.chat, sent.messages),
    };
    if to == WARM_UP && elsewhere == 0 && sent.refused == 0 && sent.other == 0 {
        return None;
    }
    let mut problem = format!(
        "for {WARM_UP} requests the fake upstream answered {} at /v1/chat/completions and {} \
         at /v1/messages, where all should have reached {}",
        sent.chat,
        sent.messages,
        kind.upstream.path(),
    );
    if sent.refused > 0 {
        problem.push_str(&format!(
            "; it refused {} for not being in its format",
            sent.refused
        ));
    }
    if sent.other > 0 {
        problem.push_str(&format!(
            "; it was asked {} times for a path it doesn't serve",
            sent.other
        ));
    }
    Some(problem)
}

/// Waits until fewer than half of the budget of connections to the fake
/// upstream were opened within the TIME_WAIT time, so that the next load
/// level has room to run.
async fn wait_for_ports(counts: &Counts, rules: &PortRule) {
    if counts.recent_connections() < rules.budget / 2 {
        return;
    }
    eprintln!("Waiting for the ports of closed upstream connections to be freed...");
    while counts.recent_connections() >= rules.budget / 2 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The time a proxy added, for the progress lines.
fn adds(outcome: &load::Outcome, upstream: Option<Percentiles>) -> String {
    match (outcome.total, upstream) {
        (Some(total), Some(upstream)) => {
            format!("adds {:.2?}", total.p50.saturating_sub(upstream.p50))
        }
        _ => "adds ?".to_owned(),
    }
}

async fn bench(
    target: &Target,
    setup: &Setup,
    args: &Args,
    counts: &Arc<Counts>,
    rules: &PortRule,
) -> Result<ProxyResult, Box<dyn Error>> {
    let name = target.name;
    let run_name = name.to_ascii_lowercase();

    // Start time. The first start isn't measured: it puts the binary in the
    // OS's file cache, and past any scan of a new file.
    let mut starts = Vec::new();
    for round in 0..=args.starts {
        let mut running = Running::start(target, setup, &run_name)?;
        let ready = running.wait_ready(setup.port).await?;
        running.stop()?;
        if round > 0 {
            eprintln!("{name}: start {round}/{}: {ready:?}", args.starts);
            starts.push(ready);
        }
    }

    let mut running = Running::start(target, setup, &run_name)?;
    running.wait_ready(setup.port).await?;
    let pid = running.pid();
    let sampler = MemorySampler::start(pid);
    let mut usage = Usage::new(pid);
    tokio::time::sleep(Duration::from_secs(2)).await;
    sampler.set(Stage::Other);

    let client = load::client()?;
    // A few of each kind first, which also shows that each gets through, to
    // the provider it's for and in that provider's format.
    for kind in &KINDS {
        let warm = request(
            setup.port,
            kind,
            body::short(kind.format, kind.model, kind.stream),
        );
        let before = Tally::of(counts);
        let outcome = load::sequential(&client, &warm, WARM_UP as usize).await;
        if let Some(error) = outcome.first_error {
            return Err(format!("{name} failed a {} request: {error}", kind.name).into());
        }
        if let Some(problem) = routing_problem(kind, Tally::of(counts).since(before)) {
            return Err(format!(
                "{name} didn't send {} requests where it should: {problem} (the last refused: \
                 {}; the last other path: {})",
                kind.name,
                last(&counts.last_refused).unwrap_or_else(|| "none".to_owned()),
                last(&counts.last_other).unwrap_or_else(|| "none".to_owned()),
            )
            .into());
        }
    }
    eprintln!("{name}: each kind of request reached its provider, in the provider's format");

    let mut short = Vec::new();
    for kind in &KINDS {
        let request = Arc::new(request(
            setup.port,
            kind,
            body::short(kind.format, kind.model, kind.stream),
        ));
        let mut rows = Vec::new();
        for &level in &args.concurrency {
            wait_for_ports(counts, rules).await;
            // Stop before the proxy's closed upstream connections can take
            // more of the machine's ports than the budget.
            let stop: Stop = {
                let counts = Arc::clone(counts);
                let budget = rules.budget;
                Arc::new(move || counts.recent_connections() >= budget)
            };
            let connections_before = counts.connections.load(Ordering::Relaxed);
            sampler.set(Stage::Load);
            let cpu_before = usage.cpu_ms();
            counts.take_served();
            let outcome =
                load::closed_loop(&client, Arc::clone(&request), level, args.duration, stop).await;
            let upstream = Percentiles::of(counts.take_served());
            let cpu_after = usage.cpu_ms();
            sampler.set(Stage::Other);
            let connections = counts
                .connections
                .load(Ordering::Relaxed)
                .saturating_sub(connections_before);
            let cpu = match (cpu_before, cpu_after) {
                (Some(before), Some(after)) if outcome.ok > 0 && after > before => Some(
                    Duration::from_micros(after.saturating_sub(before) * 1000 / outcome.ok as u64),
                ),
                _ => None,
            };
            eprintln!(
                "{name}: {}, {level} clients: {:.0}/s, {}, {} errors, {connections} upstream \
                 connections{}",
                kind.name,
                outcome.per_second(),
                adds(&outcome, upstream),
                outcome.errors,
                if outcome.stopped {
                    format!(", stopped after {:.1?}", outcome.elapsed)
                } else {
                    String::new()
                },
            );
            rows.push(ShortRow {
                clients: level,
                outcome,
                upstream,
                cpu,
                connections,
            });
            // Let connections the level left behind close.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        short.push((kind.name, rows));
    }

    let mut long = Vec::new();
    for kind in &KINDS {
        let request = request(
            setup.port,
            kind,
            body::long(kind.format, kind.model, kind.stream),
        );
        let _ = load::sequential(&client, &request, 2).await;
        counts.take_served();
        let outcome = load::sequential(&client, &request, args.long_requests).await;
        let upstream = Percentiles::of(counts.take_served());
        eprintln!(
            "{name}: long {}: {:?} p50, {}, {} errors",
            kind.name,
            outcome.total.map(|p| p.p50),
            adds(&outcome, upstream),
            outcome.errors
        );
        long.push((kind.name, Measured { outcome, upstream }));
    }

    let memory = sampler.finish();
    running.stop()?;
    Ok(ProxyResult {
        name,
        version: target.version.clone(),
        starts,
        short,
        memory,
        long,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: each kind asks for the model of the provider whose
    // format it has to reach, and the names are unique, as the report finds
    // rows by them.
    #[test]
    fn kinds() {
        for kind in &KINDS {
            let model = match kind.upstream {
                Format::Claude => proxy::CLAUDE_MODEL,
                Format::Chat => proxy::OPENAI_MODEL,
                Format::Responses => panic!("no provider takes Responses: {}", kind.name),
            };
            assert_eq!(kind.model, model, "{}", kind.name);
            assert_eq!(kind.name.ends_with(", streamed"), kind.stream);
            assert_eq!(
                KINDS.iter().filter(|other| other.name == kind.name).count(),
                1
            );
        }
        let translated = KINDS
            .iter()
            .filter(|kind| kind.format != kind.upstream)
            .count();
        assert_eq!(translated, 6);
    }

    // Not upstream's: where a proxy sent the warm-up's requests, right and
    // wrong.
    #[test]
    fn checks_routing() {
        let [_, _, _, _, to_claude, _, to_chat, ..] = &KINDS;
        let right = Tally {
            messages: WARM_UP,
            ..Tally::default()
        };
        assert_eq!(routing_problem(to_claude, right), None);
        assert!(routing_problem(to_chat, right).is_some());
        let refused = Tally {
            messages: WARM_UP - 1,
            refused: 1,
            ..Tally::default()
        };
        assert_eq!(
            routing_problem(to_claude, refused).as_deref(),
            Some(
                "for 20 requests the fake upstream answered 0 at /v1/chat/completions and 19 \
                 at /v1/messages, where all should have reached /v1/messages; it refused 1 for \
                 not being in its format"
            )
        );
        let extra = Tally {
            chat: WARM_UP,
            other: 2,
            ..Tally::default()
        };
        assert!(
            routing_problem(to_chat, extra)
                .unwrap()
                .ends_with("; it was asked 2 times for a path it doesn't serve")
        );
        let before = Tally {
            chat: 5,
            messages: 7,
            refused: 1,
            other: 0,
        };
        let after = Tally { chat: 25, ..before };
        assert_eq!(
            after.since(before),
            Tally {
                chat: 20,
                ..Tally::default()
            }
        );
    }

    // Not upstream's: the Go version in `go version <binary>`'s output.
    #[test]
    fn reads_go_versions() {
        assert_eq!(
            go_of_binary("C:\\bench\\cliproxyapi.exe: go1.26.4\n").as_deref(),
            Some("go1.26.4")
        );
        assert_eq!(
            go_of_binary("/w/cliproxyapi: go1.26.4").as_deref(),
            Some("go1.26.4")
        );
        assert_eq!(go_of_binary("not a Go binary"), None);
        assert_eq!(go_of_binary("/w/x: could not read Go build info"), None);
    }
}
