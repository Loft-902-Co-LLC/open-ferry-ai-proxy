//! Benchmarks open-ferry against CLIProxyAPI. Both run in turn in front of
//! the same fake upstream on 127.0.0.1, with the same config, and take the
//! same requests from a load generator in this process. See
//! `docs/benchmarks.md` for what it measures and how to read the results.

mod body;
mod fake;
mod load;
mod proxy;
mod report;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use body::Format;
use fake::Counts;
use load::{Request, Stop};
use proxy::{DeadPort, MemorySampler, Running, Setup, Stage, Target, Usage};
use report::{FakeSeen, Machine, ProxyResult, Settings, ShortRow};

const USAGE: &str = "\
Usage: open-ferry-bench [options]

Runs open-ferry's release build and CLIProxyAPI, one after the other, in front
of a fake upstream on 127.0.0.1, and prints the results as Markdown.

Options:
  --upstream <dir>       a CLIProxyAPI checkout to build CLIProxyAPI from
  --cliproxyapi <file>   a CLIProxyAPI binary to run instead of building one
  --go <file>            the go command to build with (default: go)
  --open-ferry <file>    open-ferry's binary (default: this workspace's
                         target/release/open-ferry)
  --work <dir>           where the runs and the CLIProxyAPI build go
                         (default: this workspace's target/bench)
  --port <n>             the port each proxy listens on, on 127.0.0.1
                         (default: 18317)
  --delay-ms <n>         the fake upstream's wait before each answer (default: 20)
  --duration <secs>      how long each load level runs (default: 10)
  --concurrency <list>   the numbers of clients at once (default: 1,16,64)
  --starts <n>           starts measured for the start time (default: 5)
  --long-requests <n>    how many times the long conversation is sent each
                         way (default: 30)
  --only <name>          run only open-ferry or only cliproxyapi
  --out <file>           also write the results to <file>, replacing the part
                         between its bench-results markers
  -h, --help             show this
";

/// New connections to the fake upstream a proxy may open within
/// [`fake::TIME_WAIT`]. Each closed one holds a port of the machine for that
/// long: this is a quarter of Windows' default range of 16,384 ports for
/// outgoing connections, which every program on the machine shares.
const PORT_BUDGET: usize = 4_000;

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
    std::fs::create_dir_all(&args.work)?;
    let mut targets = Vec::new();
    if args.only.as_deref() != Some("cliproxyapi") {
        targets.push(open_ferry(&args.open_ferry)?);
    }
    if args.only.as_deref() != Some("open-ferry") {
        targets.push(cliproxyapi(&args)?);
    }

    eprintln!("Watching the CPUs for a second before starting...");
    let machine = Machine::read();
    let fake = fake::start(args.delay).await?;
    let dead_proxy = DeadPort::new()?;
    let setup = Setup {
        port: args.port,
        upstream: fake.addr,
        dead_proxy: dead_proxy.port,
        runs: args.work.join("runs"),
    };
    let mut results = Vec::new();
    for target in &targets {
        results.push(bench(target, &setup, &args, &fake.counts).await?);
    }

    let settings = Settings {
        delay: args.delay,
        duration: args.duration,
        concurrency: args.concurrency.clone(),
        starts: args.starts,
        long_requests: args.long_requests,
        port_budget: PORT_BUDGET,
        long_chat: (
            body::long(Format::Chat, proxy::OPENAI_MODEL, false).len(),
            body::long_messages(Format::Chat),
        ),
        long_claude: (
            body::long(Format::Claude, proxy::CLAUDE_MODEL, false).len(),
            body::long_messages(Format::Claude),
        ),
    };
    let seen = FakeSeen {
        answered: fake.counts.answered(),
        other: fake.counts.other.load(Ordering::Relaxed),
        last_other: fake.counts.last_other.lock().await.clone(),
        mean_delay: fake.counts.mean_delay(),
    };
    let text = report::render(&machine, &settings, &results, &seen);
    print!("{text}");
    if let Some(out) = &args.out {
        report::write(out, &text)?;
        eprintln!("Results written to {}", out.display());
    }
    Ok(())
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

/// CLIProxyAPI's binary: the one given, or one built from the checkout into
/// the work directory. The build runs in the checkout and changes nothing in
/// it.
fn cliproxyapi(args: &Args) -> Result<Target, Box<dyn Error>> {
    if let Some(binary) = &args.cliproxyapi {
        if !binary.is_file() {
            return Err(format!("no CLIProxyAPI binary at {}", binary.display()).into());
        }
        return Ok(Target {
            name: "CLIProxyAPI",
            binary: binary.to_owned(),
            version: file_name(binary),
        });
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
    let go_version = command_output(Command::new(&args.go).args(["env", "GOVERSION"]))
        .unwrap_or_else(|| "an unknown Go version".to_owned());
    let binary = args
        .work
        .join(format!("cliproxyapi{}", std::env::consts::EXE_SUFFIX));
    eprintln!("Building CLIProxyAPI {describe} with {go_version}...");
    // The flags of upstream's release builds, but without cgo, which
    // only its plugin loader needs.
    let ldflags = format!("-s -w -X main.Version={describe} -X main.Commit={commit}");
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
    let short = commit.get(..12).unwrap_or(&commit);
    Ok(Target {
        name: "CLIProxyAPI",
        binary,
        version: format!("{describe} (commit `{short}`), built with {go_version}"),
    })
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

/// The short requests: each provider of the config, streamed and not.
const SHORT: [(&str, Format, bool); 4] = [
    ("Chat Completions", Format::Chat, false),
    ("Chat Completions, streamed", Format::Chat, true),
    ("Claude Messages", Format::Claude, false),
    ("Claude Messages, streamed", Format::Claude, true),
];

fn request(port: u16, format: Format, body: bytes::Bytes) -> Request {
    let path = match format {
        Format::Chat => "/v1/chat/completions",
        Format::Claude => "/v1/messages",
    };
    Request {
        url: format!("http://127.0.0.1:{port}{path}"),
        format,
        key: proxy::CLIENT_KEY.to_owned(),
        body,
    }
}

fn model(format: Format) -> &'static str {
    match format {
        Format::Chat => proxy::OPENAI_MODEL,
        Format::Claude => proxy::CLAUDE_MODEL,
    }
}

/// Waits until fewer than half of [`PORT_BUDGET`] connections to the fake
/// upstream were opened within [`fake::TIME_WAIT`], so that the next load
/// level has room to run.
async fn wait_for_ports(counts: &Counts) {
    if counts.recent_connections() < PORT_BUDGET / 2 {
        return;
    }
    eprintln!("Waiting for the ports of closed upstream connections to be freed...");
    while counts.recent_connections() >= PORT_BUDGET / 2 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn bench(
    target: &Target,
    setup: &Setup,
    args: &Args,
    counts: &Arc<Counts>,
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
    // A few of each request first, which also shows that each gets through.
    for (kind, format, stream) in SHORT {
        let warm = request(
            setup.port,
            format,
            body::short(format, model(format), stream),
        );
        let outcome = load::sequential(&client, &warm, 20).await;
        if let Some(error) = outcome.first_error {
            return Err(format!("{name} failed a {kind} request: {error}").into());
        }
    }

    let mut short = Vec::new();
    for (kind, format, stream) in SHORT {
        let request = Arc::new(request(
            setup.port,
            format,
            body::short(format, model(format), stream),
        ));
        let mut rows = Vec::new();
        for &level in &args.concurrency {
            wait_for_ports(counts).await;
            // Stop before the proxy's closed upstream connections can take
            // more of the machine's ports than the budget.
            let stop: Stop = {
                let counts = Arc::clone(counts);
                Arc::new(move || counts.recent_connections() >= PORT_BUDGET)
            };
            let connections_before = counts.connections.load(Ordering::Relaxed);
            sampler.set(Stage::Load);
            let cpu_before = usage.cpu_ms();
            let outcome =
                load::closed_loop(&client, Arc::clone(&request), level, args.duration, stop).await;
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
                "{name}: {kind}, {level} clients: {:.0}/s, {} errors, {connections} upstream \
                 connections{}",
                outcome.per_second(),
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
                cpu,
                connections,
            });
            // Let connections the level left behind close.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        short.push((kind, rows));
    }

    let mut long = Vec::new();
    for (kind, format, stream) in SHORT {
        let request = request(
            setup.port,
            format,
            body::long(format, model(format), stream),
        );
        let _ = load::sequential(&client, &request, 2).await;
        let outcome = load::sequential(&client, &request, args.long_requests).await;
        eprintln!(
            "{name}: long {kind}: {:?} p50, {} errors",
            outcome.total.map(|p| p.p50),
            outcome.errors
        );
        long.push((kind, outcome));
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
