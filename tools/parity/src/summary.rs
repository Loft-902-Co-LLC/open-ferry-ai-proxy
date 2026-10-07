//! The Markdown summary `--summary` writes: the upstream pin, each suite with
//! its case count and result, and the totals. The README's parity section
//! holds it between two markers, so a run can replace it in place.

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Marks the start of the generated summary in a file that holds one.
pub const START: &str = "<!-- parity-summary:start -->";
/// Marks its end.
pub const END: &str = "<!-- parity-summary:end -->";

/// How one suite's cases came out.
pub struct SuiteResult {
    pub title: &'static str,
    /// All its cases, the hand-written ones among them.
    pub cases: usize,
    pub hand_written: usize,
    pub identical: usize,
    pub equivalent: usize,
    pub known: usize,
    pub different: usize,
}

/// What the run compared against, and how its random cases were made.
pub struct RunInfo<'a> {
    /// `git describe` of the upstream checkout, such as `v8.0.15`.
    pub version: &'a str,
    pub commit: &'a str,
    /// `go env GOVERSION`, such as `go1.26.4`.
    pub go_version: &'a str,
    pub random: usize,
    pub seed: u64,
}

/// Renders the summary, without the markers. The text depends only on the
/// run's inputs and results, so rerunning with the same seed changes nothing.
pub fn render(run: &RunInfo<'_>, suites: &[SuiteResult]) -> String {
    let total = |field: fn(&SuiteResult) -> usize| suites.iter().map(field).sum::<usize>();
    let commit = run.commit.get(..12).unwrap_or(run.commit);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "<!-- Written by tools/parity's --summary option. Don't edit it by hand: rerun the tool. -->"
    );
    let _ = writeln!(
        out,
        "Against CLIProxyAPI {} (commit `{commit}`), built with {}, with `--random {} \
         --seed {}`: {} hand-written and {} random cases in all.",
        run.version,
        run.go_version,
        run.random,
        run.seed,
        thousands(total(|s| s.hand_written)),
        thousands(total(|s| s.cases) - total(|s| s.hand_written)),
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Suites | Cases | Identical | Equivalent | Known | Different |"
    );
    let _ = writeln!(out, "|---:|---:|---:|---:|---:|---:|");
    let _ = writeln!(
        out,
        "| {} | {} | {} | {} | {} | {} |",
        suites.len(),
        thousands(total(|s| s.cases)),
        thousands(total(|s| s.identical)),
        thousands(total(|s| s.equivalent)),
        thousands(total(|s| s.known)),
        thousands(total(|s| s.different)),
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "<details>");
    let _ = writeln!(out, "<summary>Each suite</summary>");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Suite | Cases | Identical | Equivalent | Known | Different |"
    );
    let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|");
    for suite in suites {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            suite.title.replace("->", "→"),
            thousands(suite.cases),
            thousands(suite.identical),
            thousands(suite.equivalent),
            thousands(suite.known),
            thousands(suite.different),
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "</details>");
    out
}

/// Writes `summary` to `path`. An existing file must hold the markers, and
/// only the text between them is replaced; a missing file is created with
/// the summary between the markers.
pub fn write(path: &Path, summary: &str) -> Result<(), Box<dyn Error>> {
    let block = format!("{START}\n{summary}{END}");
    let text = match fs::read_to_string(path) {
        Ok(text) => splice(&text, &block).ok_or_else(|| {
            format!(
                "{} has no {START} ... {END} block to replace",
                path.display()
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => format!("{block}\n"),
        Err(err) => return Err(err.into()),
    };
    fs::write(path, text)?;
    Ok(())
}

/// Replaces the first marked block in `text` with `block`, or returns `None`
/// when `text` has no start marker followed by an end marker.
fn splice(text: &str, block: &str) -> Option<String> {
    let start = text.find(START)?;
    let end = start + text.get(start..)?.find(END)? + END.len();
    Some(format!("{}{block}{}", text.get(..start)?, text.get(end..)?))
}

/// Writes `n` with a comma between each group of three digits.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(title: &'static str, cases: usize, different: usize) -> SuiteResult {
        SuiteResult {
            title,
            cases,
            hand_written: 40,
            identical: cases - different - 1,
            equivalent: 1,
            known: 0,
            different,
        }
    }

    // Not upstream's: the summary's totals and rows.
    #[test]
    fn renders_totals_and_each_suite() {
        let run = RunInfo {
            version: "v8.0.15",
            commit: "0123456789abcdef0123",
            go_version: "go1.26.4",
            random: 5000,
            seed: 1,
        };
        let text = render(
            &run,
            &[
                suite("Claude -> Codex request", 5040, 0),
                suite("Config file writes", 1200, 2),
            ],
        );
        assert!(text.contains(
            "CLIProxyAPI v8.0.15 (commit `0123456789ab`), built with go1.26.4, with \
             `--random 5000 --seed 1`: 80 hand-written and 6,160 random cases in all."
        ));
        assert!(text.contains("| 2 | 6,240 | 6,236 | 2 | 0 | 2 |\n"));
        assert!(text.contains("| Claude → Codex request | 5,040 | 5,039 | 1 | 0 | 0 |\n"));
        assert!(text.contains("| Config file writes | 1,200 | 1,197 | 1 | 0 | 2 |\n"));
        assert!(text.ends_with("</details>\n"));
    }

    // Not upstream's: only the marked block changes, and a new file gets one.
    #[test]
    fn writes_between_the_markers() {
        let dir = std::env::temp_dir().join(format!("parity-summary-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("README.md");

        write(&path, "first\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{START}\nfirst\n{END}\n")
        );

        fs::write(
            &path,
            format!("# Title\n\nBefore.\n\n{START}\nold\n{END}\n\nAfter.\n"),
        )
        .unwrap();
        write(&path, "new\n").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("# Title\n\nBefore.\n\n{START}\nnew\n{END}\n\nAfter.\n")
        );

        fs::write(&path, "no markers\n").unwrap();
        assert!(write(&path, "new\n").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "no markers\n");

        fs::remove_dir_all(&dir).unwrap();
    }

    // Not upstream's: digit grouping.
    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(123_456_789), "123,456,789");
    }
}
