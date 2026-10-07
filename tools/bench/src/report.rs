//! The results as Markdown: the machine, the settings, and a table for each
//! measure with a row per proxy. `--out` writes them between two markers of
//! a file, so a rerun can replace them in place.

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::load::Outcome;
use crate::proxy::Memory;

pub const START: &str = "<!-- bench-results:start -->";
pub const END: &str = "<!-- bench-results:end -->";

/// The machine the run was on.
pub struct Machine {
    pub os: String,
    pub cpu: String,
    pub cpus: usize,
    pub memory: u64,
    /// How busy all CPUs were, in percent, over a second before the run.
    pub busy: f32,
}

impl Machine {
    /// Reads the machine's description, and watches its CPUs for a second.
    pub fn read() -> Self {
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
            os: System::long_os_version().unwrap_or_else(|| "an unknown OS".to_owned()),
            cpu: if cpu.is_empty() {
                "an unknown CPU".to_owned()
            } else {
                cpu
            },
            cpus: system.cpus().len(),
            memory: system.total_memory(),
            busy: system.global_cpu_usage(),
        }
    }
}

/// What the run was asked to do.
pub struct Settings {
    pub delay: Duration,
    pub duration: Duration,
    pub concurrency: Vec<usize>,
    pub starts: usize,
    pub long_requests: usize,
    /// The long conversation's size in bytes and its number of messages, in
    /// the Chat Completions and the Claude Messages formats.
    pub long_chat: (usize, usize),
    pub long_claude: (usize, usize),
}

/// One proxy's results.
pub struct ProxyResult {
    pub name: &'static str,
    pub version: String,
    pub starts: Vec<Duration>,
    /// Per request kind, a row for each concurrency level.
    pub short: Vec<(&'static str, Vec<ShortRow>)>,
    pub memory: Memory,
    pub long: Vec<(&'static str, Outcome)>,
}

/// Short requests from a number of clients at once: what they got, and the
/// CPU time the proxy spent per request.
pub struct ShortRow {
    pub clients: usize,
    pub outcome: Outcome,
    pub cpu: Option<Duration>,
}

/// What the fake upstream saw over the whole run.
pub struct FakeSeen {
    pub answered: u64,
    pub other: u64,
    pub last_other: Option<String>,
    pub mean_delay: Option<Duration>,
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
        "Run on {} on {}, {} with {} logical CPUs and {} of memory. \
         Before the run its CPUs were {:.0}% busy.",
        today(),
        machine.os,
        machine.cpu,
        machine.cpus,
        gib(machine.memory),
        machine.busy,
    );
    out.push('\n');
    for proxy in proxies {
        let _ = writeln!(out, "- {}: {}", proxy.name, proxy.version);
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

    out.push_str(
        "\n#### Short requests\n\nA system prompt and a one-line question, sent again and \
         again by each number of clients at once. Latency is to the end of the answer; CPU is \
         the proxy's own user and system time, divided by the requests it answered.\n",
    );
    let kinds: Vec<&'static str> = proxies
        .first()
        .map(|proxy| proxy.short.iter().map(|(kind, _)| *kind).collect())
        .unwrap_or_default();
    let mut errors = Vec::new();
    for kind in kinds {
        let _ = writeln!(out, "\n{kind}:\n");
        out.push_str(
            "| Clients | Proxy | Requests/s | p50 | p90 | p99 | CPU per request | Errors |\n\
             |---:|---|---:|---:|---:|---:|---:|---:|\n",
        );
        for &level in &settings.concurrency {
            for proxy in proxies {
                let Some(ShortRow { outcome, cpu, .. }) = proxy
                    .short
                    .iter()
                    .find(|(k, _)| *k == kind)
                    .and_then(|(_, rows)| rows.iter().find(|row| row.clients == level))
                else {
                    continue;
                };
                let total = outcome.total;
                let _ = writeln!(
                    out,
                    "| {level} | {} | {} | {} | {} | {} | {} | {} |",
                    proxy.name,
                    thousands(outcome.per_second().round() as u64),
                    maybe(total.map(|p| p.p50)),
                    maybe(total.map(|p| p.p90)),
                    maybe(total.map(|p| p.p99)),
                    maybe(*cpu),
                    thousands(outcome.errors as u64),
                );
                if let Some(error) = &outcome.first_error {
                    errors.push(format!("{}, {kind}, {level} clients: {error}", proxy.name));
                }
            }
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
         request ({} messages there), {} as a Claude Messages one.\n",
        settings.long_claude.1,
        kib(settings.long_chat.0),
        settings.long_chat.1,
        kib(settings.long_claude.0),
    );
    out.push_str(
        "| Request | Proxy | p50 | p90 | Slowest | First bytes p50 | Errors |\n\
         |---|---|---:|---:|---:|---:|---:|\n",
    );
    let kinds: Vec<&'static str> = proxies
        .first()
        .map(|proxy| proxy.long.iter().map(|(kind, _)| *kind).collect())
        .unwrap_or_default();
    for kind in kinds {
        for proxy in proxies {
            let Some((_, outcome)) = proxy.long.iter().find(|(k, _)| *k == kind) else {
                continue;
            };
            let _ = writeln!(
                out,
                "| {kind} | {} | {} | {} | {} | {} | {} |",
                proxy.name,
                maybe(outcome.total.map(|p| p.p50)),
                maybe(outcome.total.map(|p| p.p90)),
                maybe(outcome.total.map(|p| p.max)),
                maybe(outcome.first_byte.map(|p| p.p50)),
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
    use crate::load::Percentiles;

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
    }

    // Not upstream's: a report of one proxy, and writing it between markers.
    #[test]
    fn renders_and_writes() {
        let p = Percentiles::of_millis(&[21, 22, 30]);
        let outcome = |errors: usize| Outcome {
            ok: 3,
            errors,
            first_error: (errors > 0).then(|| "HTTP 502 Bad Gateway: no".to_owned()),
            elapsed: Duration::from_secs(1),
            total: p,
            first_byte: p,
        };
        let proxies = [ProxyResult {
            name: "open-ferry",
            version: "abc1234".to_owned(),
            starts: vec![Duration::from_millis(30), Duration::from_millis(10)],
            short: vec![(
                "Chat",
                vec![ShortRow {
                    clients: 1,
                    outcome: outcome(0),
                    cpu: Some(Duration::from_micros(300)),
                }],
            )],
            memory: Memory {
                idle: Some(10 << 20),
                load: Some(20 << 20),
                peak: Some(30 << 20),
            },
            long: vec![("Chat, streamed", outcome(1))],
        }];
        let machine = Machine {
            os: "Test OS".to_owned(),
            cpu: "Test CPU".to_owned(),
            cpus: 8,
            memory: 16 << 30,
            busy: 12.4,
        };
        let settings = Settings {
            delay: Duration::from_millis(20),
            duration: Duration::from_secs(10),
            concurrency: vec![1],
            starts: 2,
            long_requests: 3,
            long_chat: (300_000, 302),
            long_claude: (310_000, 241),
        };
        let fake = FakeSeen {
            answered: 6,
            other: 0,
            last_other: None,
            mean_delay: Some(Duration::from_micros(20_140)),
        };
        let text = render(&machine, &settings, &proxies, &fake);
        assert!(text.contains(
            "8 logical CPUs and 16 GiB of memory. Before the run its CPUs were 12% busy."
        ));
        assert!(text.contains("| open-ferry | 30.0 ms | 10.0 ms | 30.0 ms |\n"));
        assert!(
            text.contains("| 1 | open-ferry | 3 | 22.0 ms | 30.0 ms | 30.0 ms | 300 µs | 0 |\n")
        );
        assert!(text.contains("| open-ferry | 10.0 MiB | 20.0 MiB | 30.0 MiB |\n"));
        assert!(text.contains(
            "| Chat, streamed | open-ferry | 22.0 ms | 30.0 ms | 30.0 ms | 22.0 ms | 1 |\n"
        ));
        assert!(text.contains("- open-ferry, long Chat, streamed: HTTP 502 Bad Gateway: no\n"));
        assert!(text.contains("answered 6 requests, after 20.1 ms on average."));

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
