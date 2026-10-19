//! The service on macOS: a launchd job.
//!
//! By default it is an agent, `~/Library/LaunchAgents/<label>.plist`,
//! loaded into the user's GUI domain (`launchctl bootstrap gui/<uid>`): it
//! runs as them while they are logged in. launchd starts it at load, and
//! again when it exits with a failure; what it prints goes to
//! `~/Library/Logs/open-ferry.log`. Installing with `-system` is refused;
//! the system-domain paths remain available to inspect and remove a daemon.

use std::fmt::Write as _;

use super::{Cmd, Context, Definition, Gone, Platform, Step, StopCheck};

/// The job's label.
pub(super) const LABEL: &str = "io.github.loft-902-co-llc.open-ferry";

/// Where the job's definition goes.
pub(super) fn plist_path(context: &Context, system: bool) -> Result<String, String> {
    if system {
        return Ok(format!("/Library/LaunchDaemons/{LABEL}.plist"));
    }
    let dir = Platform::MacOs.join(context.home()?, "Library/LaunchAgents");
    Ok(Platform::MacOs.join(&dir, &format!("{LABEL}.plist")))
}

/// Where what the job prints goes.
pub(super) fn log_path(context: &Context, system: bool) -> Result<String, String> {
    if system {
        return Ok("/Library/Logs/open-ferry.log".to_owned());
    }
    Ok(Platform::MacOs.join(context.home()?, "Library/Logs/open-ferry.log"))
}

/// The domain the job is loaded into: the user's GUI session, or the
/// system's.
pub(super) fn domain(system: bool, uid: u32) -> String {
    if system {
        "system".to_owned()
    } else {
        format!("gui/{uid}")
    }
}

/// The property list that runs `definition`, printing to `log`.
///
/// A supervised agent runs `-supervise -config <path> -data-dir <path>` and
/// has `ExitTimeOut` 60: launchd sends SIGTERM when it stops the job, and
/// SIGKILL that many seconds later. The supervisor waits for its workers for
/// at most `STOP_BUDGET` (50 s), inside the 60. `ExitTimeOut` promises no
/// orderly end past that: what is still running at the SIGKILL is killed. The
/// workers share the job's process group, and launchd kills what is left in
/// it when the job ends (`AbandonProcessGroup`, which is not set here,
/// switches that off); this has not been run on a Mac.
pub(super) fn plist(definition: &Definition, log: &str, system: bool) -> String {
    let flag = if system { " -system" } else { "" };
    let mut plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Installed by `open-ferry service install{flag}`; `open-ferry service uninstall{flag}` removes it. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
"#
    );
    let arguments: Vec<&str> = match &definition.data_dir {
        Some(data_dir) => vec![
            definition.exe.as_str(),
            "-supervise",
            "-config",
            definition.config.as_str(),
            "-data-dir",
            data_dir.as_str(),
        ],
        None => vec![
            definition.exe.as_str(),
            "-config",
            definition.config.as_str(),
        ],
    };
    for arg in arguments {
        let _ = writeln!(plist, "\t\t<string>{}</string>", escape(arg));
    }
    let _ = write!(
        plist,
        "\t</array>
\t<key>WorkingDirectory</key>
\t<string>{dir}</string>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
",
        dir = escape(&definition.dir),
    );
    if definition.data_dir.is_some() {
        plist.push_str("\t<key>ExitTimeOut</key>\n\t<integer>60</integer>\n");
    }
    let _ = write!(
        plist,
        "\t<key>StandardOutPath</key>
\t<string>{log}</string>
\t<key>StandardErrorPath</key>
\t<string>{log}</string>
</dict>
</plist>
",
        log = escape(log),
    );
    plist
}

/// `text` with the XML entities [`escape`] writes read back, and numeric
/// character references (`&#34;`, `&#x22;`), which other writers use. One
/// pass: what a reference decodes to is not decoded again. `None` for any
/// `&` that is not one of the five named entities or a valid numeric
/// reference, as a real XML parser rejects the document.
pub(super) fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let named = [
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
            ("&amp;", '&'),
        ]
        .into_iter()
        .find(|(name, _)| rest.starts_with(name))
        .map(|(name, c)| (c, name.len()));
        let (c, len) = named.or_else(|| numeric_reference(rest))?;
        out.push(c);
        rest = &rest[len..];
    }
    out.push_str(rest);
    Some(out)
}

/// The character reference at the start of `text` (`&#34;`, `&#x22;`, any
/// number of leading zeros) and the length it takes, if it names an XML 1.0
/// `Char`. Nothing else decodes: a missing `;`, a digit of another base, an
/// overflow, or a character XML doesn't allow.
fn numeric_reference(text: &str) -> Option<(char, usize)> {
    let number = text.strip_prefix("&#")?;
    let (digits, radix, prefix) = match number.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16, 3),
        None => (number, 10, 2),
    };
    let length = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    if length == 0 || !digits[length..].starts_with(';') {
        return None;
    }
    let mut code: u32 = 0;
    for digit in digits[..length].chars() {
        code = code
            .checked_mul(radix)?
            .checked_add(digit.to_digit(radix)?)?;
    }
    // #x9 | #xA | #xD | #x20-#xD7FF | #xE000-#xFFFD | #x10000-#x10FFFF
    let allowed = matches!(
        code,
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
    );
    let c = char::from_u32(code).filter(|_| allowed)?;
    Some((c, prefix + length + 1))
}

/// What the plist reader sees next.
#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    /// `<name ...>` or, when `true`, `<name .../>`.
    Open(&'a str, bool),
    /// `</name>`.
    Close(&'a str),
    /// Text between tags, as written.
    Text(&'a str),
    /// A comment or a processing instruction (the `<?xml ?>` declaration).
    Skipped,
    /// A `<!DOCTYPE>` or another `<!` declaration: skipped before the root,
    /// unreadable after it.
    Doctype,
    /// `<![CDATA[ .. ]]>`.
    Cdata,
}

/// Where the tag at the start of `text` ends (the `>`): one inside a quoted
/// attribute value doesn't.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote = None;
    for (at, byte) in text.bytes().enumerate() {
        match (quote, byte) {
            (Some(open), byte) if byte == open => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(byte),
            (None, b'>') => return Some(at),
            (None, _) => {}
        }
    }
    None
}

/// Whether `body` may be a comment's: `--` may not appear inside it, nor end
/// it (`--->`).
fn comment_ok(body: &str) -> bool {
    !body.contains("--") && !body.ends_with('-')
}

/// Where the `<!DOCTYPE ..>` at the start of `text` ends, after its `>`:
/// quoted literals, and the comments and processing instructions of an
/// internal `[ .. ]` subset, hide any `]` and `>` they hold.
fn doctype_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let (mut at, mut quote, mut subset) = (2, None, false);
    while at < bytes.len() {
        let byte = bytes[at];
        if let Some(open) = quote {
            if byte == open {
                quote = None;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = Some(byte);
        } else if subset && bytes[at..].starts_with(b"<!--") {
            let body = text[at + 4..].find("-->")?;
            if !comment_ok(&text[at + 4..at + 4 + body]) {
                return None;
            }
            at += 4 + body + 2;
        } else if subset && bytes[at..].starts_with(b"<?") {
            at += 2 + text[at + 2..].find("?>")? + 1;
        } else if byte == b'[' && !subset {
            subset = true;
        } else if byte == b']' && subset {
            subset = false;
        } else if byte == b'>' && !subset {
            return Some(at + 1);
        }
        at += 1;
    }
    None
}

/// The tags of an XML document, one at a time. It slices only at the ASCII
/// `<` and `>` it found, and reads each byte once.
struct Reader<'a> {
    rest: &'a str,
}

impl<'a> Reader<'a> {
    /// The next token, `Ok(None)` at the end, `Err` for a tag that is cut off.
    fn next(&mut self) -> Result<Option<Token<'a>>, ()> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        if !self.rest.starts_with('<') {
            let end = self.rest.find('<').unwrap_or(self.rest.len());
            let (text, rest) = self.rest.split_at(end);
            self.rest = rest;
            return Ok(Some(Token::Text(text)));
        }
        let skip = |reader: &mut Self,
                    open: usize,
                    close: &str,
                    token: Token<'a>|
         -> Result<Option<Token<'a>>, ()> {
            let end = reader.rest[open..].find(close).ok_or(())? + open + close.len();
            reader.rest = &reader.rest[end..];
            Ok(Some(token))
        };
        if self.rest.starts_with("<!--") {
            let body = &self.rest[4..];
            if !comment_ok(&body[..body.find("-->").ok_or(())?]) {
                return Err(());
            }
            return skip(self, 4, "-->", Token::Skipped);
        }
        if self.rest.starts_with("<?") {
            return skip(self, 2, "?>", Token::Skipped);
        }
        if self.rest.starts_with("<![CDATA[") {
            return skip(self, 9, "]]>", Token::Cdata);
        }
        if self.rest.starts_with("<!") {
            let end = doctype_end(self.rest).ok_or(())?;
            self.rest = &self.rest[end..];
            return Ok(Some(Token::Doctype));
        }
        let end = tag_end(self.rest).ok_or(())?;
        let inner = &self.rest[1..end];
        self.rest = &self.rest[end + 1..];
        if let Some(name) = inner.strip_prefix('/') {
            return Ok(Some(Token::Close(name.trim())));
        }
        let (inner, empty) = match inner.strip_suffix('/') {
            Some(inner) => (inner, true),
            None => (inner, false),
        };
        let name = inner.split(char::is_whitespace).next().unwrap_or_default();
        Ok(Some(Token::Open(name, empty)))
    }

    /// The next token that is not whitespace between elements or skipped.
    fn element(&mut self) -> Result<Option<Token<'a>>, ()> {
        loop {
            match self.next()? {
                Some(Token::Skipped | Token::Doctype) => {}
                Some(Token::Text(text)) if text.trim().is_empty() => {}
                other => return Ok(other),
            }
        }
    }

    /// Reads past the rest of the element just opened (`empty`: it has no
    /// content), whatever is inside, by an explicit stack of open tags.
    fn skip_element(&mut self, name: &'a str, empty: bool) -> Result<(), ()> {
        let mut open = if empty { Vec::new() } else { vec![name] };
        while !open.is_empty() {
            match self.next()?.ok_or(())? {
                Token::Open(name, false) => open.push(name),
                Token::Close(name) if open.pop() == Some(name) => {}
                Token::Close(_) => return Err(()),
                _ => {}
            }
        }
        Ok(())
    }

    /// The text of a `<name>` just opened, unescaped: plain text only.
    /// Markup inside it, a comment or CDATA included, is an `Err`.
    fn text(&mut self, name: &str, empty: bool) -> Result<String, ()> {
        if empty {
            return Ok(String::new());
        }
        let mut text = String::new();
        match self.next()?.ok_or(())? {
            Token::Close(close) if close == name => return Ok(text),
            Token::Text(found) => text.push_str(&unescape(found).ok_or(())?),
            _ => return Err(()),
        }
        match self.next()?.ok_or(())? {
            Token::Close(close) if close == name => Ok(text),
            _ => Err(()),
        }
    }

    /// The `<string>`s of an `<array>` just opened, which has nothing else.
    fn strings(&mut self, empty: bool) -> Result<Vec<String>, ()> {
        let mut words = Vec::new();
        if empty {
            return Ok(words);
        }
        loop {
            match self.element()?.ok_or(())? {
                Token::Close("array") => return Ok(words),
                Token::Open("string", empty) => words.push(self.text("string", empty)?),
                _ => return Err(()),
            }
        }
    }
}

/// The `Program` and the `ProgramArguments` of the job dict at the top of
/// a plist: only its own keys, so a `Program` inside `EnvironmentVariables`
/// or any other value is not taken. `Err` for a document that is cut off or
/// mismatched, a key given twice, or either value of another type or with
/// markup in a string.
fn job_keys(plist: &str) -> Result<(Option<String>, Option<Vec<String>>), ()> {
    // One byte order mark may begin the document.
    let mut reader = Reader {
        rest: plist.strip_prefix('\u{FEFF}').unwrap_or(plist),
    };
    let Some(Token::Open("plist", false)) = reader.element()? else {
        return Err(());
    };
    let Some(Token::Open("dict", false)) = reader.element()? else {
        return Err(());
    };
    let (mut program, mut arguments) = (None, None);
    let mut seen = std::collections::HashSet::new();
    loop {
        let key = match reader.element()?.ok_or(())? {
            Token::Close("dict") => break,
            Token::Open("key", empty) => reader.text("key", empty)?,
            _ => return Err(()),
        };
        // Which of two equal keys launchd takes is its own affair.
        if !seen.insert(key.clone()) {
            return Err(());
        }
        let Some(Token::Open(kind, empty)) = reader.element()? else {
            return Err(());
        };
        match key.as_str() {
            "Program" if kind == "string" && program.is_none() => {
                program = Some(reader.text("string", empty)?);
            }
            "ProgramArguments" if kind == "array" && arguments.is_none() => {
                arguments = Some(reader.strings(empty)?);
            }
            "Program" | "ProgramArguments" => return Err(()),
            _ => reader.skip_element(kind, empty)?,
        }
    }
    let Some(Token::Close("plist")) = reader.element()? else {
        return Err(());
    };
    // Only whitespace, comments and processing instructions may follow.
    loop {
        match reader.next()? {
            None => return Ok((program, arguments)),
            Some(Token::Skipped) => {}
            Some(Token::Text(text)) if text.trim().is_empty() => {}
            Some(_) => return Err(()),
        }
    }
}

/// The executable and arguments a plist runs, unescaped; the executable
/// is the first word. launchd runs `Program` when there is one, with
/// `ProgramArguments` as `argv` (so its first string is `argv[0]`, not the
/// executable), else `ProgramArguments[0]`. `None` when the plist has
/// neither, or can't be read.
pub(super) fn program_arguments(plist: &str) -> Option<Vec<String>> {
    match job_keys(plist).ok()? {
        (Some(program), arguments) => {
            let mut words = vec![program];
            words.extend(arguments.into_iter().flatten().skip(1));
            Some(words)
        }
        (None, arguments) => arguments,
    }
}

/// `text` escaped for XML.
pub(super) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

fn service(domain: &str) -> String {
    format!("{domain}/{LABEL}")
}

/// Writes the job's definition at `path` and loads it into `domain`,
/// which starts it.
pub(super) fn install_steps(
    path: &str,
    domain: &str,
    definition: &Definition,
    log: &str,
) -> Vec<Step> {
    vec![
        Step::Write {
            path: path.to_owned(),
            text: plist(definition, log, domain == "system"),
            utf16: false,
        },
        // A job that was once disabled stays so until it is enabled, even
        // after it is removed and loaded again.
        Step::Run(Cmd::new("launchctl", &["enable"]).arg(service(domain))),
        Step::Run(Cmd::new("launchctl", &["bootstrap", domain]).arg(path)),
    ]
}

/// Unloads the job from `domain`, which stops it, and removes its
/// definition at `path`.
pub(super) fn uninstall_steps(path: &str, domain: &str) -> Vec<Step> {
    vec![
        Step::TryRun(Cmd::new("launchctl", &["bootout"]).arg(service(domain))),
        Step::Remove(path.to_owned()),
    ]
}

/// What replaces [`uninstall_steps`]' first step for a job that may run the
/// supervisor: where the `bootout` fails, `print` must say the job isn't
/// loaded before the rest goes on.
pub(super) fn stop_check(domain: &str) -> StopCheck {
    StopCheck {
        stop: Cmd::new("launchctl", &["bootout"]).arg(service(domain)),
        query: status(domain),
        gone: Gone::Launchd,
    }
}

pub(super) fn status(domain: &str) -> Cmd {
    Cmd::new("launchctl", &["print"]).arg(service(domain))
}

pub(super) fn notes(system: bool, log: &str) -> String {
    if system {
        format!(
            "open-ferry is installed and started, as a launchd daemon: it starts at boot, and again when it fails.\nIts output: {log}"
        )
    } else {
        format!(
            "open-ferry is installed and started, as a launchd agent: it starts when you log in, and again when it fails.\nIts output: {log}"
        )
    }
}
