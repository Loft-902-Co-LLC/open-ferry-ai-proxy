//! The results as Markdown: the machine, the settings, and a table for each
//! measure with a row per proxy. `--out` writes them between two markers of
//! a file, so a rerun can replace them in place.

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::load::{Outcome, Percentiles};
use crate::ports::PortRule;
use crate::proxy::Memory;

pub const START: &str = "<!-- bench-results:start -->";
pub const END: &str = "<!-- bench-results:end -->";

/// The machine the run was on.
pub struct Machine {
    pub os: String,
    pub kernel: String,
    pub cpu: String,
    pub cpus: usize,
    pub memory: u64,
    /// How busy all CPUs were, in percent, over a second before the run.
    pub busy: f32,
    /// What the person running it said about the machine (`--machine-note`).
    pub note: Option<String>,
}

impl Machine {
    /// Reads the machine's description, and watches its CPUs for a second.
    pub fn read(note: Option<String>) -> Self {
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        std::thread::sleep(Duration::from_secs(1).max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL));
        system.refresh_cpu_usage();
        let cpu = system
            .cpus()
            .first()
            .map(|cpu| cpu.brand().trim().to_owned())
            .unwrap_or_default();
        Self {
            os: os_name(System::long_os_version()),
            kernel: System::kernel_long_version(),
            cpu: if cpu.is_empty() {
                "an unknown CPU".to_owned()
            } else {
                cpu
            },
            cpus: system.cpus().len(),
            memory: system.total_memory(),
            busy: system.global_cpu_usage(),
            note,
        }
    }
}

/// The OS as sysinfo names it, but a Linux distribution's name without the
/// "Linux (…)" sysinfo puts around it, as the report gives the kernel next.
fn os_name(long: Option<String>) -> String {
    let Some(long) = long else {
        return "an unknown OS".to_owned();
    };
    if let Some(distribution) = long
        .strip_prefix("Linux (")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return distribution.to_owned();
    }
    long
}

/// What the run was asked to do, and with what.
pub struct Settings {
    pub delay: Duration,
    pub duration: Duration,
    pub concurrency: Vec<usize>,
    pub starts: usize,
    pub long_requests: usize,
    /// When a load level stops for the upstream connections it opened.
    pub ports: PortRule,
    /// The long conversation's size in bytes and its number of messages, in
    /// the Chat Completions and the Claude Messages formats, and its number
    /// of input items in the Responses format.
    pub long_chat: (usize, usize),
    pub long_claude: (usize, usize),
    pub long_responses: (usize, usize),
    /// `rustc -V` in this workspace, and the Go version CLIProxyAPI was
    /// built with, when known.
    pub rustc: Option<String>,
    pub go: Option<String>,
    /// Whether the benchmark itself is a debug build.
    pub debug: bool,
}

/// One proxy's results.
pub struct ProxyResult {
    pub name: &'static str,
    pub version: String,
    pub starts: Vec<Duration>,
    /// Per request kind, a row for each concurrency level.
    pub short: Vec<(&'static str, Vec<ShortRow>)>,
    pub memory: Memory,
    pub long: Vec<(&'static str, Measured)>,
}

/// Requests sent one after another: what they got, and the fake upstream's
/// own time for them.
pub struct Measured {
    pub outcome: Outcome,
    pub upstream: Option<Percentiles>,
}

/// Short requests from a number of clients at once: what they got, the fake
/// upstream's own time for them, the CPU time the proxy spent per request,
/// and the connections it opened to the fake upstream.
pub struct ShortRow {
    pub clients: usize,
    pub outcome: Outcome,
    pub upstream: Option<Percentiles>,
    pub cpu: Option<Duration>,
    pub connections: u64,
}

/// What the fake upstream saw over the whole run.
pub struct FakeSeen {
    pub answered: u64,
    pub other: u64,
    pub last_other: Option<String>,
    pub refused: u64,
    pub last_refused: Option<String>,
    pub mean_delay: Option<Duration>,
}

/// What a proxy added to its requests: the latency's p50 minus the fake
/// upstream's own p50 for the same requests.
fn adds(outcome: &Outcome, upstream: Option<Percentiles>) -> Option<Duration> {
    Some(outcome.total?.p50.saturating_sub(upstream?.p50))
}

pub fn render(
    machine: &Machine,
    settings: &Settings,
    proxies: &[ProxyResult],
    fake: &FakeSeen,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<!-- Written by tools/bench's --out option. Don't edit it by hand: rerun the tool. -->"
    );
    let _ = writeln!(
        out,
        "Run on {} on {} ({}), {} with {} logical CPUs and {} of memory. \
         Before the run its CPUs were {:.0}% busy.",
        today(),
        machine.os,
        machine.kernel,
        machine.cpu,
        machine.cpus,
        gib(machine.memory),
        machine.busy,
    );
    if let Some(note) = &machine.note {
        let _ = writeln!(out, "\nThe machine: {}", note.trim());
    }
    out.push('\n');
    for proxy in proxies {
        let _ = writeln!(out, "- {}: {}", proxy.name, proxy.version);
    }
    let rustc = settings
        .rustc
        .as_deref()
        .map_or_else(|| "unknown".to_owned(), |rustc| format!("`{rustc}`"));
    match &settings.go {
        Some(go) => {
            let _ = writeln!(out, "\nThe Rust toolchain was {rustc}, and Go {go}.");
        }
        None => {
            let _ = writeln!(out, "\nThe Rust toolchain was {rustc}.");
        }
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "The fake upstream waited {} before each answer. Each load level ran for {}. \
         For the start time each proxy was started {}, after a start that wasn't \
         measured. Each kind of long conversation request was sent {}, one after \
         another.",
        duration(settings.delay),
        duration(settings.duration),
        times(settings.starts),
        times(settings.long_requests),
    );
    if settings.debug {
        out.push_str(
            "\n**The benchmark itself was a debug build**, whose load generator and fake \
             upstream are slower than a release build's: these numbers show the tool working, \
             not the proxies' speed.\n",
        );
    }

    render_adds(&mut out, settings, proxies);

    out.push_str("\n#### Start time\n\nFrom starting the process to its first answer on `GET /v1/models`.\n\n");
    out.push_str("| Proxy | Median | Fastest | Slowest |\n|---|---:|---:|---:|\n");
    for proxy in proxies {
        let mut starts = proxy.starts.clone();
        starts.sort_unstable();
        let median = starts.get(starts.len() / 2).copied();
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            proxy.name,
            maybe(median),
            maybe(starts.first().copied()),
            maybe(starts.last().copied()),
        );
    }

    let ports = &settings.ports;
    let _ = writeln!(
        out,
        "\n#### Short requests\n\nA system prompt and a one-line question, sent again and \
         again by each number of clients at once. Latency is to the end of the answer; the \
         proxy adds the p50 minus the fake upstream's own p50 for the same requests. CPU is \
         the proxy's own user and system time, divided by the requests it answered. Upstream \
         connections are the connections the proxy opened to the fake upstream. Each closed \
         one holds a port of the machine for up to {}, and the machine has {} ports for \
         outgoing connections ({}). So a level stops early once the proxy has opened {} \
         within that time, a quarter of them, and the next waits until fewer than {} were.",
        duration_words(ports.window),
        thousands(ports.ports as u64),
        ports.source,
        thousands(ports.budget as u64),
        thousands((ports.budget / 2) as u64),
    );
    let kinds: Vec<&'static str> = proxies
        .first()
        .map(|proxy| proxy.short.iter().map(|(kind, _)| *kind).collect())
        .unwrap_or_default();
    let mut errors = Vec::new();
    let mut stopped = Vec::new();
    for kind in kinds {
        let _ = writeln!(out, "\n{kind}:\n");
        out.push_str(
            "| Clients | Proxy | Requests/s | p50 | p90 | p99 | Proxy adds | CPU per request | \
             Errors | Upstream connections |\n|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for &level in &settings.concurrency {
            for proxy in proxies {
                let Some(row) = short_row(proxy, kind, level) else {
                    continue;
                };
                let outcome = &row.outcome;
                let total = outcome.total;
                let _ = writeln!(
                    out,
                    "| {level} | {} | {}{} | {} | {} | {} | {} | {} | {} | {} |",
                    proxy.name,
                    thousands(outcome.per_second().round() as u64),
                    if outcome.stopped { "*" } else { "" },
                    maybe(total.map(|p| p.p50)),
                    maybe(total.map(|p| p.p90)),
                    maybe(total.map(|p| p.p99)),
                    maybe(adds(outcome, row.upstream)),
                    maybe(row.cpu),
                    thousands(outcome.errors as u64),
                    thousands(row.connections),
                );
                if outcome.stopped {
                    stopped.push(format!(
                        "{}, {kind}, {level} clients: after {}",
                        proxy.name,
                        duration(outcome.elapsed)
                    ));
                }
                if let Some(error) = &outcome.first_error {
                    errors.push(format!("{}, {kind}, {level} clients: {error}", proxy.name));
                }
            }
        }
    }

    if !stopped.is_empty() {
        out.push_str("\n\\* Stopped early, at the limit of upstream connections:\n\n");
        for line in &stopped {
            let _ = writeln!(out, "- {line}");
        }
    }

    out.push_str(
        "\n#### Memory\n\nResident memory (the working set on Windows), sampled every 50 ms.\n\n\
         | Proxy | Idle after start | Under load (median) | Peak |\n|---|---:|---:|---:|\n",
    );
    for proxy in proxies {
        let memory = &proxy.memory;
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            proxy.name,
            memory.idle.map_or_else(|| "-".to_owned(), mib),
            memory.load.map_or_else(|| "-".to_owned(), mib),
            memory.peak.map_or_else(|| "-".to_owned(), mib),
        );
    }

    let _ = writeln!(
        out,
        "\n#### Long conversation\n\nA coding agent's conversation: a system prompt, two \
         tools, and {} messages with a tool call every fourth turn; {} as a Chat Completions \
         request ({} messages there), {} as a Claude Messages one, and {} as a Responses one \
         ({} input items).\n",
        settings.long_claude.1,
        kib(settings.long_chat.0),
        settings.long_chat.1,
        kib(settings.long_claude.0),
        kib(settings.long_responses.0),
        settings.long_responses.1,
    );
    out.push_str(
        "| Request | Proxy | p50 | p90 | Slowest | First bytes p50 | Proxy adds | Errors |\n\
         |---|---|---:|---:|---:|---:|---:|---:|\n",
    );
    let kinds: Vec<&'static str> = proxies
        .first()
        .map(|proxy| proxy.long.iter().map(|(kind, _)| *kind).collect())
        .unwrap_or_default();
    for kind in kinds {
        for proxy in proxies {
            let Some((_, measured)) = proxy.long.iter().find(|(k, _)| *k == kind) else {
                continue;
            };
            let outcome = &measured.outcome;
            let _ = writeln!(
                out,
                "| {kind} | {} | {} | {} | {} | {} | {} | {} |",
                proxy.name,
                maybe(outcome.total.map(|p| p.p50)),
                maybe(outcome.total.map(|p| p.p90)),
                maybe(outcome.total.map(|p| p.max)),
                maybe(outcome.first_byte.map(|p| p.p50)),
                maybe(adds(outcome, measured.upstream)),
                thousands(outcome.errors as u64),
            );
            if let Some(error) = &outcome.first_error {
                errors.push(format!("{}, long {kind}: {error}", proxy.name));
            }
        }
    }

    let _ = writeln!(
        out,
        "\nThe fake upstream answered {} requests, after {} on average.",
        thousands(fake.answered),
        maybe(fake.mean_delay),
    );
    if fake.refused > 0 {
        let _ = writeln!(
            out,
            "It refused {} for not being in its own format, the last for {}.",
            thousands(fake.refused),
            fake.last_refused.as_deref().unwrap_or("?"),
        );
    }
    if fake.other > 0 {
        let _ = writeln!(
            out,
            "It was also asked {} times for a path it doesn't serve, last `{}`.",
            thousands(fake.other),
            fake.last_other.as_deref().unwrap_or("?"),
        );
    }
    if !errors.is_empty() {
        out.push_str("\nFirst error of each row with errors:\n\n");
        for error in errors {
            let _ = writeln!(out, "- {}", error.replace('\n', " "));
        }
    }
    out
}

/// A proxy's row for `kind` at `level` clients.
fn short_row<'a>(proxy: &'a ProxyResult, kind: &str, level: usize) -> Option<&'a ShortRow> {
    proxy
        .short
        .iter()
        .find(|(k, _)| *k == kind)
        .and_then(|(_, rows)| rows.iter().find(|row| row.clients == level))
}

/// The table of what each proxy adds: to short requests at the fewest
/// clients, and to the long conversation.
fn render_adds(out: &mut String, settings: &Settings, proxies: &[ProxyResult]) {
    let Some(fewest) = settings.concurrency.iter().min().copied() else {
        return;
    };
    let _ = writeln!(
        out,
        "\n#### What each proxy adds\n\nThe time a proxy adds to a request, on top of the fake \
         upstream's: the latency at the 50th percentile, minus the fake upstream's own time for \
         the same requests at the 50th percentile, which it measures from reading the request \
         to handing over the last of its answer. Short requests are with {} at once, the long \
         conversation one request after another. The tables below give it for every level.\n",
        if fewest == 1 {
            "one client".to_owned()
        } else {
            format!("{fewest} clients")
        },
    );
    out.push_str("| Request |");
    for part in ["short", "long"] {
        for proxy in proxies {
            let _ = write!(out, " {}, {part} |", proxy.name);
        }
    }
    out.push_str("\n|---|");
    for _ in 0..proxies.len() * 2 {
        out.push_str("---:|");
    }
    out.push('\n');
    let kinds: Vec<&'static str> = proxies
        .first()
        .map(|proxy| proxy.short.iter().map(|(kind, _)| *kind).collect())
        .unwrap_or_default();
    for kind in kinds {
        let _ = write!(out, "| {kind} |");
        for proxy in proxies {
            let added =
                short_row(proxy, kind, fewest).and_then(|row| adds(&row.outcome, row.upstream));
            let _ = write!(out, " {} |", maybe(added));
        }
        for proxy in proxies {
            let added = proxy
                .long
                .iter()
                .find(|(k, _)| *k == kind)
                .and_then(|(_, measured)| adds(&measured.outcome, measured.upstream));
            let _ = write!(out, " {} |", maybe(added));
        }
        out.push('\n');
    }
}

/// Writes `results` between the markers of the file at `path`, or makes the
/// file if there is none. A file without the markers is left as it is.
pub fn write(path: &Path, results: &str) -> Result<(), Box<dyn Error>> {
    let block = format!("{START}\n{results}{END}");
    let text = match fs::read_to_string(path) {
        Ok(text) => splice(&text, &block).ok_or_else(|| {
            format!(
                "{} has no {START} and {END} lines to write between",
                path.display()
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => format!("{block}\n"),
        Err(err) => return Err(err.into()),
    };
    fs::write(path, text)?;
    Ok(())
}

fn splice(text: &str, block: &str) -> Option<String> {
    let start = text.find(START)?;
    let end = start + text.get(start..)?.find(END)? + END.len();
    Some(format!("{}{block}{}", text.get(..start)?, text.get(end..)?))
}

fn maybe(duration_opt: Option<Duration>) -> String {
    duration_opt.map_or_else(|| "-".to_owned(), duration)
}

/// A duration with three significant digits or so.
fn duration(d: Duration) -> String {
    let micros = d.as_secs_f64() * 1e6;
    if micros < 1000.0 {
        format!("{micros:.0} µs")
    } else if micros < 10_000.0 {
        format!("{:.2} ms", micros / 1000.0)
    } else if micros < 1_000_000.0 {
        format!("{:.1} ms", micros / 1000.0)
    } else {
        format!("{:.2} s", micros / 1e6)
    }
}

/// A whole number of seconds or minutes, in words.
fn duration_words(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        60 => "a minute".to_owned(),
        s if s > 60 && s.is_multiple_of(60) => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
}

fn times(n: usize) -> String {
    match n {
        1 => "once".to_owned(),
        2 => "twice".to_owned(),
        n => format!("{n} times"),
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn gib(bytes: u64) -> String {
    format!("{:.0} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn kib(bytes: usize) -> String {
    format!("{} KiB", thousands((bytes as u64).div_ceil(1024)))
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Today's date in UTC, as YYYY-MM-DD.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs() / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil(i64::try_from(days).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}")
}

/// The calendar date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the OS's name as the report gives it, before the
    // kernel; sysinfo's names as it gives them on Linux, Windows and macOS.
    #[test]
    fn names_the_os() {
        let os = |long: &str| os_name(Some(long.to_owned()));
        assert_eq!(os("Linux (Ubuntu 24.04)"), "Ubuntu 24.04");
        assert_eq!(os("Linux (Debian GNU/Linux 12)"), "Debian GNU/Linux 12");
        assert_eq!(os("Windows 11 Pro"), "Windows 11 Pro");
        assert_eq!(os("macOS 15.0 Sequoia"), "macOS 15.0 Sequoia");
        assert_eq!(os("Linux"), "Linux");
        assert_eq!(os_name(None), "an unknown OS");
    }

    // Not upstream's: dates, durations and counts as the report shows them.
    #[test]
    fn formats() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_733), (2026, 10, 7));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(duration(Duration::from_micros(420)), "420 µs");
        assert_eq!(duration(Duration::from_micros(2_345)), "2.35 ms");
        assert_eq!(duration(Duration::from_micros(23_456)), "23.5 ms");
        assert_eq!(duration(Duration::from_millis(1_500)), "1.50 s");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(999), "999");
        assert_eq!(mib(3 * 1024 * 1024 / 2), "1.5 MiB");
        assert_eq!(kib(330_000), "323 KiB");
        assert_eq!(duration_words(Duration::from_secs(120)), "2 minutes");
        assert_eq!(duration_words(Duration::from_secs(60)), "a minute");
        assert_eq!(duration_words(Duration::from_secs(30)), "30 seconds");
    }

    // Not upstream's: a report of one proxy, and writing it between markers.
    #[test]
    fn renders_and_writes() {
        let p = Percentiles::of_millis(&[21, 22, 30]);
        let upstream = Percentiles::of_millis(&[20, 20, 21]);
        let outcome = |errors: usize| Outcome {
            ok: 3,
            errors,
            first_error: (errors > 0).then(|| "HTTP 502 Bad Gateway: no".to_owned()),
            elapsed: Duration::from_secs(1),
            stopped: errors > 0,
            total: p,
            first_byte: p,
        };
        let proxies = [ProxyResult {
            name: "open-ferry",
            version: "abc1234".to_owned(),
            starts: vec![Duration::from_millis(30), Duration::from_millis(10)],
            short: vec![
                (
                    "Chat",
                    vec![ShortRow {
                        clients: 1,
                        outcome: outcome(0),
                        upstream,
                        cpu: Some(Duration::from_micros(300)),
                        connections: 1,
                    }],
                ),
                (
                    "Claude",
                    vec![ShortRow {
                        clients: 1,
                        outcome: outcome(1),
                        upstream: None,
                        cpu: None,
                        connections: 4_000,
                    }],
                ),
            ],
            memory: Memory {
                idle: Some(10 << 20),
                load: Some(20 << 20),
                peak: Some(30 << 20),
            },
            long: vec![(
                "Chat",
                Measured {
                    outcome: outcome(1),
                    upstream,
                },
            )],
        }];
        let machine = Machine {
            os: "Test OS".to_owned(),
            kernel: "Linux 6.8.0".to_owned(),
            cpu: "Test CPU".to_owned(),
            cpus: 8,
            memory: 16 << 30,
            busy: 12.4,
            note: Some("Azure Standard_D8as_v5, eastus ".to_owned()),
        };
        let settings = Settings {
            delay: Duration::from_millis(20),
            duration: Duration::from_secs(10),
            concurrency: vec![1],
            starts: 2,
            long_requests: 3,
            ports: PortRule::new(
                28_232,
                Duration::from_secs(60),
                "the test's range".to_owned(),
            ),
            long_chat: (300_000, 302),
            long_claude: (310_000, 241),
            long_responses: (305_000, 301),
            rustc: Some("rustc 1.99.0 (abc 2026-09-01)".to_owned()),
            go: Some("go1.26.4".to_owned()),
            debug: true,
        };
        let fake = FakeSeen {
            answered: 6,
            other: 0,
            last_other: None,
            refused: 2,
            last_refused: Some("/v1/messages: no model".to_owned()),
            mean_delay: Some(Duration::from_micros(20_140)),
        };
        let text = render(&machine, &settings, &proxies, &fake);
        assert!(text.contains(
            "on Test OS (Linux 6.8.0), Test CPU with 8 logical CPUs and 16 GiB of memory. Before \
             the run its CPUs were 12% busy.\n\nThe machine: Azure Standard_D8as_v5, eastus\n"
        ));
        assert!(text.contains(
            "The Rust toolchain was `rustc 1.99.0 (abc 2026-09-01)`, and Go go1.26.4.\n"
        ));
        assert!(text.contains("**The benchmark itself was a debug build**"));
        assert!(text.contains(
            "| Request | open-ferry, short | open-ferry, long |\n|---|---:|---:|\n\
             | Chat | 2.00 ms | 2.00 ms |\n| Claude | - | - |\n"
        ));
        assert!(text.contains("| open-ferry | 30.0 ms | 10.0 ms | 30.0 ms |\n"));
        assert!(text.contains(
            "| 1 | open-ferry | 3 | 22.0 ms | 30.0 ms | 30.0 ms | 2.00 ms | 300 µs | 0 | 1 |\n"
        ));
        assert!(text.contains(
            "for up to a minute, and the machine has 28,232 ports for outgoing connections (the \
             test's range). So a level stops early once the proxy has opened 7,000 within that \
             time, a quarter of them, and the next waits until fewer than 3,500 were."
        ));
        assert!(text.contains(
            "| 1 | open-ferry | 3* | 22.0 ms | 30.0 ms | 30.0 ms | - | - | 1 | 4,000 |\n"
        ));
        assert!(text.contains(
            "\\* Stopped early, at the limit of upstream connections:\n\n\
             - open-ferry, Claude, 1 clients: after 1.00 s\n"
        ));
        assert!(text.contains("- open-ferry, Claude, 1 clients: HTTP 502 Bad Gateway: no\n"));
        assert!(text.contains("| open-ferry | 10.0 MiB | 20.0 MiB | 30.0 MiB |\n"));
        assert!(text.contains("and 298 KiB as a Responses one (301 input items)."));
        assert!(text.contains(
            "| Chat | open-ferry | 22.0 ms | 30.0 ms | 30.0 ms | 22.0 ms | 2.00 ms | 1 |\n"
        ));
        assert!(text.contains("- open-ferry, long Chat: HTTP 502 Bad Gateway: no\n"));
        assert!(text.contains("answered 6 requests, after 20.1 ms on average."));
        assert!(text.contains(
            "It refused 2 for not being in its own format, the last for /v1/messages: no model."
        ));

        let dir = std::env::temp_dir().join(format!("bench-report-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("benchmarks.md");
        write(&path, "first\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{START}\nfirst\n{END}\n")
        );
        fs::write(&path, format!("# Title\n\n{START}\nold\n{END}\n\nAfter.\n")).unwrap();
        write(&path, "new\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("# Title\n\n{START}\nnew\n{END}\n\nAfter.\n")
        );
        fs::write(&path, "no markers\n").unwrap();
        assert!(write(&path, "new\n").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "no markers\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
