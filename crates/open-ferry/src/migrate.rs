//! `open-ferry migrate`: switches a CLIProxyAPI setup to open-ferry, and
//! back.
//!
//! - `open-ferry migrate [-config PATH] [-yes] [-dry-run] [-json]` finds
//!   CLIProxyAPI, running or set up to run, and what starts it (see
//!   [`discover`]); says what carries over and what doesn't, and what stops
//!   the switch (see [`assess`]); and shows the switch, step by step (see
//!   [`switch`]). Without `-yes` it asks before it changes anything, and
//!   with no one at a terminal to ask, it changes nothing. With `-dry-run`
//!   it only shows; `-json` shows the same as one line of JSON, for the
//!   install scripts, and changes nothing.
//! - `open-ferry migrate -undo [-restore] [-yes] [-dry-run]` switches back,
//!   as the switch's record says (see [`record`]). With `-restore` it also
//!   copies the backed-up config, `.env` files and credentials back.
//!
//! Before it changes anything, `migrate` backs up the config, its `.env`
//! files and the auth directory, and after the switch it waits for
//! open-ferry to answer on the config's address, and undoes the switch
//! when it doesn't. It exits with 0 when it did what it was asked, or when
//! it only showed the switch; 1 when it found nothing to switch, the switch
//! is blocked or failed, or there is no one to ask; and 2 for bad usage.
//!
//! Every process `migrate` reads or stops, file it reads or writes, and
//! command it runs goes through [`machine::Machine`], so the tests drive it
//! with a fake and never touch a real service manager or process.
//!
//! Deviations from upstream: upstream has no `migrate`, and no way to
//! switch to another proxy; this is open-ferry's own (see [`flags`] for
//! the subcommands).

mod assess;
mod discover;
mod machine;
mod record;
mod switch;
#[cfg(test)]
mod tests;

use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{Map, Value};

use self::assess::{Assessment, Kind};
use self::discover::Found;
use self::machine::{Host, Machine};
use self::record::Status;
use self::switch::Plan;
use crate::flags::{self, FlagError};
use crate::os_service::{Context, Platform, say};

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "migrate";

/// Runs `open-ferry migrate` with `args`, the arguments after `migrate`.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let request = match parse(args) {
        Ok(request) => request,
        Err(FlagError::Help) => {
            eprint!("{}", usage(program));
            return ExitCode::SUCCESS;
        }
        Err(FlagError::Invalid(message)) => {
            eprintln!("{message}");
            eprint!("{}", usage(program));
            return ExitCode::from(2);
        }
    };
    let context = match Context::host() {
        Ok(context) => context,
        Err(message) => {
            eprintln!("{NAME}: {message}");
            return ExitCode::FAILURE;
        }
    };
    let mut machine = Host::new();
    let mut out = io::stdout().lock();
    let mut err = io::stderr().lock();
    ExitCode::from(run(&mut machine, &context, &request, &mut out, &mut err))
}

/// The command line after `migrate`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Request {
    /// `-config`, as given.
    pub(crate) config: Option<String>,
    pub(crate) yes: bool,
    /// `-dry-run`, or `-json`.
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
    pub(crate) undo: bool,
    pub(crate) restore: bool,
}

/// What the flags ask for.
#[derive(Default)]
struct Options {
    config: String,
    yes: bool,
    dry_run: bool,
    json: bool,
    undo: bool,
    restore: bool,
}

/// Every flag, sorted by name.
const DEFINITIONS: [flags::Definition<Options>; 6] = [
    flags::Definition {
        name: "config",
        usage: "The config CLIProxyAPI runs with, when migrate can't tell it from its command line",
        kind: flags::Kind::String(|options, value| options.config = value),
    },
    flags::Definition {
        name: "dry-run",
        usage: "Show what would be switched, and change nothing",
        kind: flags::Kind::Bool(|options, value| options.dry_run = value),
    },
    flags::Definition {
        name: "json",
        usage: "Show what would be switched as one line of JSON, and change nothing",
        kind: flags::Kind::Bool(|options, value| options.json = value),
    },
    flags::Definition {
        name: "restore",
        usage: "With -undo, also copy the backed-up config, .env files and credentials back",
        kind: flags::Kind::Bool(|options, value| options.restore = value),
    },
    flags::Definition {
        name: "undo",
        usage: "Switch back to CLIProxyAPI, as the switch's record says",
        kind: flags::Kind::Bool(|options, value| options.undo = value),
    },
    flags::Definition {
        name: "yes",
        usage: "Switch without asking, and stop CLIProxyAPI when the switch needs it",
        kind: flags::Kind::Bool(|options, value| options.yes = value),
    },
];

/// Reads the arguments after `migrate`, as the server's flags are read
/// (see [`flags`]). An argument after the flags is an error.
pub(crate) fn parse<I>(args: I) -> Result<Request, FlagError>
where
    I: IntoIterator<Item = String>,
{
    let (options, rest) = flags::parse_with(&DEFINITIONS, args)?;
    if let Some(arg) = rest.first() {
        return Err(FlagError::Invalid(format!("unexpected argument: {arg}")));
    }
    let request = Request {
        config: Some(options.config).filter(|config| !config.is_empty()),
        yes: options.yes,
        dry_run: options.dry_run || options.json,
        json: options.json,
        undo: options.undo,
        restore: options.restore,
    };
    if request.restore && !request.undo {
        return Err(FlagError::Invalid("-restore goes with -undo".to_owned()));
    }
    if request.undo && request.json {
        return Err(FlagError::Invalid("-json doesn't go with -undo".to_owned()));
    }
    if request.undo && request.config.is_some() {
        return Err(FlagError::Invalid(
            "-config doesn't go with -undo: the switch's record says which config".to_owned(),
        ));
    }
    Ok(request)
}

/// The usage text.
fn usage(program: &str) -> String {
    let mut out = format!(
        "Usage: {program} {NAME} [-config PATH] [-yes] [-dry-run] [-json]\n       \
         {program} {NAME} -undo [-restore] [-yes] [-dry-run]\n\n\
         Finds CLIProxyAPI and what starts it, says what carries over to open-ferry,\n\
         and switches it to open-ferry: its service is replaced by open-ferry's, or\n\
         its binary by open-ferry's. The config, the .env files and the auth directory\n\
         are backed up first. Asks before changing anything, unless -yes is given.\n\nFlags:\n"
    );
    flags::write_defaults(&mut out, &DEFINITIONS, &[]);
    out.push_str("\nSee docs/migrating-from-cliproxyapi.md.\n");
    out
}

/// A line of output; a closed output doesn't stop the work.
fn line(out: &mut dyn Write, text: &str) {
    say(out, format_args!("{text}"));
}

/// `text` as a sentence: capitalized, but for open-ferry's name, with a
/// full stop.
pub(crate) fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.starts_with("open-ferry") {
        let mut out = text.to_owned();
        if !out.ends_with(['.', '!', '?']) {
            out.push('.');
        }
        return out;
    }
    let mut chars = text.chars();
    let mut out = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// Carries out `request`, `migrate`'s work, on `machine`: what it says goes
/// to `out`, its errors to `err`. Gives the exit code.
pub(crate) fn run(
    machine: &mut dyn Machine,
    context: &Context,
    request: &Request,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    if request.undo {
        return run_undo(machine, context, request, out, err);
    }
    let platform = context.platform;
    let given = match request.config.as_deref() {
        Some(config) => match platform.absolute(&context.cwd, config) {
            Ok(config) => Some(platform.clean(&config)),
            Err(error) => return fail(request, out, err, &error),
        },
        None => None,
    };
    let found = match discover::discover(machine, context, given.as_deref()) {
        Ok(Some(found)) => found,
        Ok(None) => {
            if request.json {
                line(out, r#"{"found":false}"#);
            } else {
                line(
                    out,
                    "CLIProxyAPI wasn't found: it isn't running, and no service, scheduled task or container of it is set up. Nothing was changed.",
                );
            }
            return 1;
        }
        Err(error) => return fail(request, out, err, &error),
    };
    let kind = Kind::of(platform, &found.starter);
    let assessment = assess::assess(machine, context, &found, kind, given.as_deref());
    let plan = switch::plan(
        machine,
        context,
        &found,
        &assessment,
        kind,
        given.as_deref(),
    );
    let blockers: Vec<String> = assessment
        .blockers
        .iter()
        .chain(&plan.blockers)
        .map(|blocker| sentence(blocker))
        .collect();
    if request.json {
        let json = json(&found, &assessment, &plan, &blockers);
        line(out, &json.to_string());
        return 0;
    }
    show(out, &found, &assessment, &plan, &blockers);
    if !blockers.is_empty() {
        line(
            out,
            &format!(
                "The switch is blocked: nothing was changed. Fix what is listed under Blockers, then run `{}` again.",
                plan.command
            ),
        );
        return 1;
    }
    if kind == Kind::Container {
        line(
            out,
            "open-ferry doesn't change containers or Compose files: make the changes above yourself.",
        );
        return 0;
    }
    if request.dry_run {
        line(out, "Dry run: nothing was changed.");
        return 0;
    }
    if !request.yes {
        if !machine.terminal() {
            line(
                out,
                &format!(
                    "Nothing was changed: there is no one at a terminal to ask. To switch, run: {} -yes",
                    plan.command
                ),
            );
            return 1;
        }
        if !machine.ask("Switch to open-ferry now? [y/N]") {
            line(out, "Nothing was changed.");
            return 0;
        }
    }
    match switch::execute(
        machine,
        context,
        &found,
        &assessment,
        &plan,
        request.yes,
        out,
    ) {
        Ok(()) => 0,
        Err(message) => {
            let _ = out.flush();
            say(err, format_args!("{NAME}: {message}"));
            1
        }
    }
}

/// Reports `error`, which stopped `migrate` before it found CLIProxyAPI.
fn fail(request: &Request, out: &mut dyn Write, err: &mut dyn Write, error: &str) -> u8 {
    if request.json {
        let mut json = Map::new();
        json.insert("found".to_owned(), Value::Bool(false));
        json.insert("error".to_owned(), Value::String(error.to_owned()));
        line(out, &Value::Object(json).to_string());
    } else {
        say(err, format_args!("{NAME}: {error}"));
    }
    1
}

/// One line on CLIProxyAPI for the install scripts' question: no quotes
/// and no control characters, so that a shell script can cut it out of
/// the JSON.
fn summary(found: &Found) -> String {
    let text = match &found.process {
        Some(process) => format!(
            "CLIProxyAPI (process {}), started by {}",
            process.pid,
            found.starter.describe()
        ),
        None => format!(
            "CLIProxyAPI, not running, set up as {}",
            found.starter.describe()
        ),
    };
    text.chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '"' { '\'' } else { c })
        .collect()
}

/// What `-json` shows.
fn json(found: &Found, assessment: &Assessment, plan: &Plan, blockers: &[String]) -> Value {
    fn strings(list: &[String]) -> Value {
        Value::Array(list.iter().cloned().map(Value::String).collect())
    }
    fn text(value: Option<&str>) -> Value {
        value.map_or(Value::Null, |value| Value::String(value.to_owned()))
    }
    fn counts(map: &std::collections::BTreeMap<String, usize>) -> Value {
        Value::Object(
            map.iter()
                .map(|(name, count)| (name.clone(), Value::from(*count)))
                .collect(),
        )
    }
    let mut json = Map::new();
    let mut put = |key: &str, value: Value| {
        json.insert(key.to_owned(), value);
    };
    put("found", Value::Bool(true));
    put("summary", Value::String(summary(found)));
    put(
        "can_switch",
        Value::Bool(blockers.is_empty() && plan.kind != Kind::Container),
    );
    put("switch", Value::String(plan.kind.name().to_owned()));
    put(
        "target",
        match plan.kind {
            Kind::Service(target) => Value::String(record::target_name(target).to_owned()),
            Kind::DropIn | Kind::Container => Value::Null,
        },
    );
    put(
        "process",
        found.process.as_ref().map_or(Value::Null, |process| {
            let mut map = Map::new();
            map.insert("pid".to_owned(), Value::from(process.pid));
            map.insert("exe".to_owned(), text(process.exe.as_deref()));
            Value::Object(map)
        }),
    );
    put("exe", text(found.exe.as_deref()));
    let mut starter = Map::new();
    starter.insert(
        "kind".to_owned(),
        Value::String(found.starter.kind().to_owned()),
    );
    starter.insert(
        "description".to_owned(),
        Value::String(found.starter.describe()),
    );
    put("starter", Value::Object(starter));
    put("config", text(assessment.config.as_deref()));
    put("config_from", Value::String(assessment.config_from.clone()));
    put("working_dir", text(assessment.working_dir.as_deref()));
    put("auth_dir", text(assessment.auth_dir.as_deref()));
    put(
        "listen",
        assessment.listen.as_ref().map_or(Value::Null, |listen| {
            let mut map = Map::new();
            map.insert("host".to_owned(), Value::String(listen.host.clone()));
            map.insert("port".to_owned(), Value::from(listen.port));
            map.insert("tls".to_owned(), Value::Bool(listen.tls));
            map.insert("description".to_owned(), Value::String(listen.describe()));
            Value::Object(map)
        }),
    );
    let credentials = &assessment.credentials;
    let mut map = Map::new();
    map.insert("served".to_owned(), counts(&credentials.served));
    map.insert("not_served".to_owned(), counts(&credentials.not_served));
    map.insert(
        "other_files".to_owned(),
        Value::from(credentials.other_files),
    );
    map.insert("unreadable".to_owned(), Value::from(credentials.unreadable));
    put("credentials", Value::Object(map));
    let claude = assessment.claude_sign_ins;
    let mut map = Map::new();
    map.insert("count".to_owned(), Value::from(claude));
    map.insert(
        "warning".to_owned(),
        if claude > 0 {
            Value::String(assess::claude_warning(claude))
        } else {
            Value::Null
        },
    );
    put("claude_sign_ins", Value::Object(map));
    put("carries_over", strings(&assessment.carries_over));
    put(
        "does_not_carry_over",
        strings(&assessment.does_not_carry_over),
    );
    put(
        "check",
        Value::Array(
            assessment
                .check
                .iter()
                .map(|finding| {
                    let mut map = Map::new();
                    map.insert(
                        "level".to_owned(),
                        Value::String(finding.level.name().to_owned()),
                    );
                    map.insert("check".to_owned(), Value::String(finding.check.clone()));
                    map.insert("message".to_owned(), Value::String(finding.message.clone()));
                    map.insert("fix".to_owned(), Value::String(finding.fix.clone()));
                    Value::Object(map)
                })
                .collect(),
        ),
    );
    put("blockers", strings(blockers));
    put("warnings", strings(&assessment.warnings));
    put("plan", strings(&plan.steps));
    put("good_to_know", strings(&plan.good_to_know));
    put("backup_dir", text(plan.backup_dir.as_deref()));
    put("record", text(plan.record.as_deref().ok()));
    put("command", Value::String(plan.command.clone()));
    Value::Object(json)
}

/// A list under a heading, when it has anything.
fn section(out: &mut dyn Write, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    line(out, "");
    line(out, heading);
    for item in items {
        line(out, &format!("  - {}", sentence(item)));
    }
}

/// What the switch would do, for a person.
fn show(
    out: &mut dyn Write,
    found: &Found,
    assessment: &Assessment,
    plan: &Plan,
    blockers: &[String],
) {
    line(out, "Found CLIProxyAPI:");
    let mut rows: Vec<(&str, String)> = Vec::new();
    match &found.process {
        Some(process) => rows.push((
            "Process",
            match &process.exe {
                Some(exe) => format!("{} ({exe})", process.pid),
                None => process.pid.to_string(),
            },
        )),
        None => rows.push((
            "Process",
            match &found.exe {
                Some(exe) => format!("not running (its binary: {exe})"),
                None => "not running".to_owned(),
            },
        )),
    }
    rows.push(("Started by", found.starter.describe()));
    if let Some(config) = &assessment.config {
        rows.push(("Config", format!("{config} ({})", assessment.config_from)));
    }
    if let Some(dir) = &assessment.working_dir {
        rows.push(("Working directory", dir.clone()));
    }
    if let Some(dir) = &assessment.auth_dir {
        rows.push(("Auth directory", dir.clone()));
    }
    if let Some(listen) = &assessment.listen {
        rows.push(("Listens on", listen.describe()));
    }
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 1;
    for (name, value) in rows {
        line(out, &format!("  {:width$} {value}", format!("{name}:")));
    }
    section(out, "Carries over:", &assessment.carries_over);
    section(out, "Doesn't carry over:", &assessment.does_not_carry_over);
    if assessment.claude_sign_ins > 0 {
        section(
            out,
            "Claude sign-ins:",
            &[assess::claude_warning(assessment.claude_sign_ins)],
        );
    }
    section(out, "Warnings:", &assessment.warnings);
    if !assessment.check.is_empty() {
        line(out, "");
        line(out, "Check:");
        for finding in &assessment.check {
            line(
                out,
                &format!(
                    "  - {}: {}",
                    finding.level.name(),
                    sentence(&finding.message)
                ),
            );
            if !finding.fix.is_empty() {
                line(out, &format!("    Fix: {}", sentence(&finding.fix)));
            }
        }
    }
    section(out, "Blockers:", blockers);
    line(out, "");
    let how = match plan.kind {
        Kind::Service(target) => format!("a service switch, to {}", target.kind()),
        Kind::DropIn => "a drop-in: open-ferry in place of CLIProxyAPI's binary".to_owned(),
        Kind::Container => "a container: the steps to take yourself".to_owned(),
    };
    line(out, &format!("The switch ({how}):"));
    for (number, step) in plan.steps.iter().enumerate() {
        line(out, &format!("  {}. {}", number + 1, sentence(step)));
    }
    section(out, "Good to know:", &plan.good_to_know);
    line(out, "");
}

/// `migrate -undo`.
fn run_undo(
    machine: &mut dyn Machine,
    context: &Context,
    request: &Request,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let platform = context.platform;
    let path = match record::path(context) {
        Ok(path) => path,
        Err(error) => return fail(request, out, err, &error),
    };
    let mut record = match record::load(machine, &path) {
        Ok(Some(record)) => record,
        Ok(None) => {
            let sudo = match platform {
                Platform::Windows => "",
                Platform::Linux | Platform::MacOs => {
                    " A switch made with sudo is undone with sudo, as its record is root's."
                }
            };
            return fail(
                request,
                out,
                err,
                &format!("there is no switch to undo: {path} doesn't exist.{sudo}"),
            );
        }
        Err(error) => return fail(request, out, err, &error),
    };
    if record.platform != record::platform_name(platform) {
        return fail(
            request,
            out,
            err,
            &format!(
                "the switch's record, {path}, is of a switch on {}",
                record.platform
            ),
        );
    }
    match record.status {
        Status::RolledBack => {
            return fail(
                request,
                out,
                err,
                &format!(
                    "the switch made at {} failed and was undone then: there is nothing to undo",
                    record.created
                ),
            );
        }
        Status::Undone => {
            return fail(
                request,
                out,
                err,
                &format!(
                    "the switch made at {} was already undone, at {}",
                    record.created, record.updated
                ),
            );
        }
        Status::Switching | Status::Switched => {}
    }
    line(
        out,
        &format!(
            "Switching back to CLIProxyAPI, as switched at {} (the record: {path}):",
            record.created
        ),
    );
    for (number, step) in switch::undo_steps(context, &record, request.restore)
        .iter()
        .enumerate()
    {
        line(out, &format!("  {}. {}", number + 1, sentence(step)));
    }
    if record.status == Status::Switching {
        line(
            out,
            "The switch didn't finish: -undo does what it can, and says what it couldn't.",
        );
    }
    if request.restore {
        line(out, &switch::restore_warning(&record));
    }
    line(out, "");
    if request.dry_run {
        line(out, "Dry run: nothing was changed.");
        return 0;
    }
    if !request.yes {
        if !machine.terminal() {
            line(
                out,
                &format!(
                    "Nothing was changed: there is no one at a terminal to ask. To switch back, run: {} {NAME} -undo{} -yes",
                    platform.quote(&context.exe),
                    if request.restore { " -restore" } else { "" }
                ),
            );
            return 1;
        }
        if !machine.ask("Switch back to CLIProxyAPI now? [y/N]") {
            line(out, "Nothing was changed.");
            return 0;
        }
    }
    match switch::undo(machine, context, &path, &mut record, request.restore, out) {
        Ok(()) => 0,
        Err(message) => {
            let _ = out.flush();
            say(err, format_args!("{NAME}: {message}"));
            1
        }
    }
}
