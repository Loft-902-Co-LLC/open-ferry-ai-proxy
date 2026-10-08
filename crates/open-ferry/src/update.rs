//! `open-ferry update`: checks for a release of open-ferry, installs it,
//! rolls back to the version before, or sets whether updates are
//! automatic (see `docs/updates.md`).
//!
//! - `update -check` looks for the latest release and says whether it is
//!   newer. It changes nothing, not even the update state.
//! - `update` looks, shows what it would do, asks, then downloads the
//!   release, checks its signature and SHA-256, runs it with `--version`,
//!   and puts it in place of the installed binary, keeping the old one for
//!   a rollback. It restarts nothing; it says how to.
//! - `update -rollback` puts the version before the last switch back.
//! - `update -mode off|notify|auto` writes `self-update.mode` to the
//!   config, keeping the rest of the file as it is.
//!
//! Without a terminal to ask in, and without `-yes`, nothing is installed
//! or rolled back: it says what it would do and exits with 3. It works
//! while updates are off, as you asked, and says so. `-json` prints one
//! JSON object, and nothing else, on standard output.
//!
//! The config is `-config`, else `config.yaml` in the working directory
//! when there is one, else the installed config (see [`installed`]) when
//! there is one. `.env` in the working directory is loaded first, as the
//! server loads it.
//!
//! Exit codes: 0 when it is up to date, or did what was asked; 1 when it
//! failed or refused; 2 for bad usage; 3 when a newer release is out and
//! wasn't installed.
//!
//! Upstream has no `update` (see [`flags`]).

use std::fmt::Write as _;
use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use open_ferry_core::config::Config;
use open_ferry_core::config::save::update_nested_scalar;
use open_ferry_update::background::http_fetch;
use open_ferry_update::state::SwitchRecord;
use open_ferry_update::updater::Found;
use open_ferry_update::{
    DataDir, Install, ModeSource, SelfUpdateMode, Settings, Status, UpdateError, Updater,
};
use serde::Serialize;

use crate::flags::{self, Definition, FlagError, Kind};
use crate::installed;

#[cfg(test)]
mod tests;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "update";

/// The exit code for a newer release that wasn't installed.
const NOT_INSTALLED: u8 = 3;

/// What the command line asks for.
#[derive(Default)]
struct Options {
    check: bool,
    config: String,
    json: bool,
    mode: Option<String>,
    rollback: bool,
    yes: bool,
}

/// The flags, sorted by name.
const DEFINITIONS: [Definition<Options>; 6] = [
    Definition {
        name: "check",
        usage: "Only say whether a newer release is out; change nothing",
        kind: Kind::Bool(|options, value| options.check = value),
    },
    Definition {
        name: "config",
        usage: "The config to read, and that -mode writes (default: config.yaml in the working directory, else the installed config)",
        kind: Kind::String(|options, value| options.config = value),
    },
    Definition {
        name: "json",
        usage: "Print one JSON object rather than lines of text",
        kind: Kind::Bool(|options, value| options.json = value),
    },
    Definition {
        name: "mode",
        usage: "Set self-update.mode in the config: off, notify or auto",
        kind: Kind::String(|options, value| options.mode = Some(value)),
    },
    Definition {
        name: "rollback",
        usage: "Put back the version before the last update",
        kind: Kind::Bool(|options, value| options.rollback = value),
    },
    Definition {
        name: "yes",
        usage: "Install or roll back without asking",
        kind: Kind::Bool(|options, value| options.yes = value),
    },
];

/// The usage text.
fn usage(program: &str) -> String {
    let mut out = format!(
        "Usage: {program} {NAME} [flags]\n\n\
         Installs the latest release of open-ferry in place of this one, after checking\n\
         its signature and SHA-256 and running it, and keeps this one for -rollback.\n\
         It restarts nothing. It works while automatic updates are off.\n\n\
         Turn automatic updates off: open-ferry {NAME} -mode off\n\n\
         Exit codes: 0 when up to date or done, 1 when it failed or refused, 2 for\n\
         bad usage, 3 when a newer release is out and wasn't installed.\n\nFlags:\n"
    );
    flags::write_defaults(&mut out, &DEFINITIONS, &[]);
    out
}

/// What is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Check,
    Update,
    Rollback,
    Mode(SelfUpdateMode),
}

impl Request {
    fn action(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Update => "update",
            Self::Rollback => "rollback",
            Self::Mode(_) => "mode",
        }
    }
}

/// The request `options` make, or why they make none.
fn request(options: &Options) -> Result<Request, String> {
    let asked = [options.check, options.rollback, options.mode.is_some()]
        .iter()
        .filter(|asked| **asked)
        .count();
    if asked > 1 {
        return Err("use only one of -check, -rollback and -mode".to_owned());
    }
    if let Some(mode) = &options.mode {
        if mode.trim().is_empty() {
            return Err("-mode needs a value: off, notify or auto".to_owned());
        }
        return SelfUpdateMode::parse(mode)
            .map(Request::Mode)
            .ok_or_else(|| format!("-mode is {mode:?}; use off, notify or auto"));
    }
    Ok(if options.check {
        Request::Check
    } else if options.rollback {
        Request::Rollback
    } else {
        Request::Update
    })
}

/// Runs `open-ferry update` with `args`, the arguments after `update`.
/// `main` calls it before any thread starts, as `.env` is loaded here.
pub fn main<I>(program: &str, args: I) -> ExitCode
where
    I: IntoIterator<Item = String>,
{
    let options = match flags::parse_with(&DEFINITIONS, args) {
        Ok((options, rest)) => match rest.first() {
            None => options,
            Some(arg) => return usage_error(program, &format!("unexpected argument: {arg}")),
        },
        Err(FlagError::Help) => {
            eprint!("{}", usage(program));
            return ExitCode::SUCCESS;
        }
        Err(FlagError::Invalid(message)) => return usage_error(program, &message),
    };
    let request = match request(&options) {
        Ok(request) => request,
        Err(message) => return usage_error(program, &message),
    };
    let working_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{NAME}: failed to get working directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let dotenv = working_dir.join(".env");
    if let Err(error) = crate::load_dotenv(&dotenv)
        && !error.is_not_found()
    {
        eprintln!(
            "{NAME}: {} doesn't load, so none of it is set: {error}",
            dotenv.display()
        );
    }
    let path = config_path(&options.config, &working_dir, installed::config_path().ok());
    let env = std::env::var(open_ferry_update::MODE_ENV).ok();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut console = Console::new(options.json, &mut stdout, &mut stderr);
    let outcome = match request {
        Request::Mode(mode) => set_mode(&path, mode, env.as_deref(), &mut console),
        request => manual(request, &path, &options, &mut console),
    };
    console.finish(outcome)
}

/// Shows `message` and the usage, for bad usage.
fn usage_error(program: &str, message: &str) -> ExitCode {
    eprintln!("{message}");
    eprint!("{}", usage(program));
    ExitCode::from(2)
}

/// The config: `flag`, else `config.yaml` in `working_dir` when it
/// exists, else `installed` when it exists, else the one in `working_dir`.
fn config_path(flag: &str, working_dir: &Path, installed: Option<PathBuf>) -> PathBuf {
    if !flag.is_empty() {
        return PathBuf::from(flag);
    }
    let local = working_dir.join("config.yaml");
    if local.is_file() {
        return local;
    }
    installed.filter(|path| path.is_file()).unwrap_or(local)
}

/// Runs a check, an update or a rollback, with the updater for this
/// binary and the config at `path`.
fn manual(request: Request, path: &Path, options: &Options, console: &mut Console) -> Outcome {
    let config = if path.is_file() {
        match Config::load(path) {
            Ok(config) => Some(config),
            Err(error) => {
                let settings = Settings::from_environment(&Default::default());
                return Outcome::new(request.action(), &settings)
                    .failed(console, format!("{} doesn't load: {error}", path.display()));
            }
        }
    } else {
        None
    };
    let settings = Settings::from_environment(
        &config
            .as_ref()
            .map(|config| config.self_update.clone())
            .unwrap_or_default(),
    );
    console.mode_line(request, &settings, path, config.is_some());
    let proxy_url = config.map(|config| config.proxy_url).unwrap_or_default();
    let updater = match real_updater(&proxy_url) {
        Ok(updater) => updater,
        Err(error) => {
            return Outcome::new(request.action(), &settings).failed(console, error.to_string());
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Outcome::new(request.action(), &settings).failed(
                console,
                format!("failed to start the async runtime: {error}"),
            );
        }
    };
    let updates = Real(updater);
    let mut terminal = Terminal;
    runtime.block_on(run(
        request,
        &updates,
        &settings,
        options.yes,
        &mut terminal,
        console,
    ))
}

/// The updater for this binary, downloading through `proxy_url`.
fn real_updater(proxy_url: &str) -> Result<Updater, UpdateError> {
    let data = DataDir::for_this_user().map_err(UpdateError::NoDataDir)?;
    let fetch = http_fetch()(proxy_url).map_err(|error| UpdateError::Fetch {
        what: "the release list",
        error,
    })?;
    Updater::for_this_binary(fetch, data)
}

/// What the commands need of the updater.
trait Updates {
    /// What a check found, for an update to install.
    type Found;

    /// The status, for `settings`.
    fn status(&self, settings: &Settings) -> Status;

    /// The latest release, checked; it changes nothing.
    async fn latest(&self) -> Result<Offer<Self::Found>, UpdateError>;

    /// What a rollback would do, or why there can be none.
    fn rollback_plan(&self) -> Result<RollbackPlan, UpdateError>;

    /// Downloads, checks and stages `found`, then switches to it.
    async fn update(&self, found: Self::Found) -> Result<SwitchRecord, UpdateError>;

    /// Switches back to the version before the last switch.
    async fn rollback(&self) -> Result<SwitchRecord, UpdateError>;
}

/// The latest release, as a check found it.
#[derive(Clone, Debug)]
struct Offer<F> {
    latest: String,
    installed: String,
    newer: bool,
    /// It failed its check on this machine before.
    failed_here: bool,
    /// It was rolled back from.
    rolled_back: bool,
    install: Install,
    found: F,
}

/// What a rollback would do.
#[derive(Clone, Debug)]
struct RollbackPlan {
    previous: String,
    installed: String,
    binary: PathBuf,
}

/// The updater.
struct Real(Updater);

impl Updates for Real {
    type Found = Found;

    fn status(&self, settings: &Settings) -> Status {
        self.0.status(settings, &self.0.install())
    }

    async fn latest(&self) -> Result<Offer<Found>, UpdateError> {
        let state = self.0.state();
        let found = self.0.latest(&state).await?;
        let latest = found.release.version.clone();
        Ok(Offer {
            failed_here: state.has_failed(&latest),
            rolled_back: state.rolled_back.as_deref() == Some(latest.as_str()),
            installed: found.installed.clone(),
            newer: found.is_newer(),
            install: self.0.install(),
            latest,
            found,
        })
    }

    fn rollback_plan(&self) -> Result<RollbackPlan, UpdateError> {
        let binary = match self.0.install() {
            Install::SelfUpdating { binary } => binary,
            Install::NotifyOnly(why) => return Err(UpdateError::NotSelfUpdating(why)),
        };
        let state = self.0.state();
        let previous = state.previous.clone().ok_or(UpdateError::NoPrevious)?;
        if !self
            .0
            .data
            .binary(&previous, self.0.binary_name())
            .is_file()
        {
            return Err(UpdateError::NoPrevious);
        }
        Ok(RollbackPlan {
            previous,
            installed: self.0.installed_version(&state),
            binary,
        })
    }

    async fn update(&self, found: Found) -> Result<SwitchRecord, UpdateError> {
        let _lock = self.0.lock()?;
        let mut state = self.0.state();
        state.latest = Some(found.release.version.clone());
        self.0.stage(&found, &mut state).await?;
        self.0.switch_to_staged(&mut state)
    }

    async fn rollback(&self) -> Result<SwitchRecord, UpdateError> {
        let _lock = self.0.lock()?;
        let mut state = self.0.state();
        self.0.rollback(&mut state).await
    }
}

/// Asks whether to go on.
trait Prompt {
    /// The answer to `question`: `None` when there is no one to ask.
    fn confirm(&mut self, question: &str) -> Option<bool>;
}

/// Asks on the terminal, when standard input is one.
struct Terminal;

impl Prompt for Terminal {
    fn confirm(&mut self, question: &str) -> Option<bool> {
        let stdin = io::stdin();
        if !stdin.is_terminal() {
            return None;
        }
        let mut stdout = io::stdout();
        let _ = write!(stdout, "{question} [y/N] ");
        let _ = stdout.flush();
        let mut line = String::new();
        stdin.lock().read_line(&mut line).ok()?;
        let answer = line.trim().to_ascii_lowercase();
        Some(answer == "y" || answer == "yes")
    }
}

/// Where the lines go: standard output, and errors to standard error, as
/// they are said; with `-json`, only the object at the end.
struct Console<'a> {
    json: bool,
    out: &'a mut dyn io::Write,
    err: &'a mut dyn io::Write,
    /// What was said, but for the mode line, for `-json`'s message.
    said: Vec<String>,
}

impl<'a> Console<'a> {
    fn new(json: bool, out: &'a mut dyn io::Write, err: &'a mut dyn io::Write) -> Self {
        Self {
            json,
            out,
            err,
            said: Vec::new(),
        }
    }

    /// Says `line`.
    fn say(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.json {
            let _ = writeln!(self.out, "{line}");
        }
        self.said.push(line);
    }

    /// Says `line`, which is an error.
    fn error(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.json {
            let _ = writeln!(self.err, "{NAME}: {line}");
        }
        self.said.push(line);
    }

    /// The first lines: whether updates are automatic and what set it, and
    /// any notes on the settings. They aren't in `-json`'s message, whose
    /// fields say the same.
    fn mode_line(&mut self, request: Request, settings: &Settings, path: &Path, config: bool) {
        if self.json {
            return;
        }
        let mut line = format!(
            "Automatic updates are {} ({}).",
            settings.describe(),
            source(settings.source, path, config)
        );
        if settings.mode == SelfUpdateMode::Off {
            match request {
                Request::Check => line.push_str(" Checking because you asked."),
                Request::Update => line.push_str(" Updating because you asked."),
                Request::Rollback | Request::Mode(_) => {}
            }
        }
        let _ = writeln!(self.out, "{line}");
        for note in &settings.notes {
            let _ = writeln!(self.out, "Note: {note}");
        }
    }

    /// Prints `outcome` with `-json`, and returns its exit code.
    fn finish(self, mut outcome: Outcome) -> ExitCode {
        if self.json {
            outcome.message = self.said.join("\n");
            match serde_json::to_string_pretty(&outcome) {
                Ok(json) => {
                    let _ = writeln!(self.out, "{json}");
                }
                Err(error) => {
                    let _ = writeln!(self.err, "{NAME}: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        let _ = self.out.flush();
        ExitCode::from(outcome.code)
    }
}

/// What set the mode, as the mode line says it.
fn source(source: ModeSource, path: &Path, config: bool) -> String {
    match source {
        ModeSource::Default if config => "the default".to_owned(),
        ModeSource::Default => format!("the default; there is no config at {}", path.display()),
        ModeSource::Config => format!("set by self-update.mode in {}", path.display()),
        ModeSource::Environment => format!("set by {source}"),
    }
}

/// What happened, for `-json` and the exit code.
#[derive(Debug, Serialize)]
struct Outcome {
    /// `check`, `update`, `rollback` or `mode`.
    action: &'static str,
    /// `up-to-date`, `update-available`, `cannot-update`,
    /// `needs-confirmation`, `declined`, `updated`, `rolled-back`,
    /// `mode-set` or `error`.
    result: &'static str,
    /// What the text output says.
    message: String,
    /// The mode in force: `auto`, `notify` or `off`.
    mode: &'static str,
    /// What set it: `default`, `config` or `environment`.
    mode_source: &'static str,
    /// The mode as people say it: `on`, `notify-only` or `off`.
    updates: &'static str,
    /// Notes on the settings.
    notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_version: Option<String>,
    /// The switch made.
    #[serde(skip_serializing_if = "Option::is_none")]
    switch: Option<SwitchRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// For `-check`: the status, as the server's status route gives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>,
    /// For `-mode`: the config written.
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<String>,
    #[serde(skip)]
    code: u8,
}

impl Outcome {
    fn new(action: &'static str, settings: &Settings) -> Self {
        Self {
            action,
            result: "error",
            message: String::new(),
            mode: settings.mode.as_str(),
            mode_source: settings.source.as_str(),
            updates: settings.describe(),
            notes: settings.notes.clone(),
            latest_version: None,
            installed_version: None,
            switch: None,
            error: None,
            status: None,
            config: None,
            code: 1,
        }
    }

    fn with(mut self, result: &'static str, code: u8) -> Self {
        self.result = result;
        self.code = code;
        self
    }

    /// The outcome of a failure, said.
    fn failed(mut self, console: &mut Console, error: String) -> Self {
        console.error(error.clone());
        self.error = Some(error);
        self.with("error", 1)
    }
}

/// Runs `request` (not `-mode`) with `updates`, asking `prompt` unless
/// `yes`.
async fn run<U: Updates>(
    request: Request,
    updates: &U,
    settings: &Settings,
    yes: bool,
    prompt: &mut dyn Prompt,
    console: &mut Console<'_>,
) -> Outcome {
    match request {
        Request::Check => check(updates, settings, console).await,
        Request::Update => update(updates, settings, yes, prompt, console).await,
        Request::Rollback => rollback(updates, settings, yes, prompt, console).await,
        Request::Mode(_) => Outcome::new(request.action(), settings)
            .failed(console, "-mode doesn't use the updater".to_owned()),
    }
}

/// `-check`.
async fn check<U: Updates>(updates: &U, settings: &Settings, console: &mut Console<'_>) -> Outcome {
    let outcome = Outcome::new("check", settings);
    let mut status = updates.status(settings);
    let offer = match updates.latest().await {
        Ok(offer) => offer,
        Err(error) => {
            let mut outcome = outcome.failed(console, error.to_string());
            outcome.status = Some(status);
            return outcome;
        }
    };
    status.latest_version = Some(offer.latest.clone());
    status.update_available = offer.newer;
    let mut outcome = if !offer.newer {
        console.say(up_to_date(&offer));
        outcome.with("up-to-date", 0)
    } else if let Some(why) = offer.install.why_not() {
        console.say(format!(
            "open-ferry {} is out ({} is installed), but this install doesn't update itself: {why}.",
            offer.latest, offer.installed
        ));
        outcome.with("cannot-update", NOT_INSTALLED)
    } else {
        console.say(format!(
            "open-ferry {} is out ({} is installed). Run `open-ferry update` to install it.",
            offer.latest, offer.installed
        ));
        say_skipped(&offer, console);
        outcome.with("update-available", NOT_INSTALLED)
    };
    if status.restart_needed {
        console.say(format!(
            "open-ferry {} is installed, but {} is running: restart open-ferry to run it.",
            status.installed_version, status.running_version
        ));
    }
    outcome.latest_version = Some(offer.latest);
    outcome.installed_version = Some(offer.installed);
    outcome.status = Some(status);
    outcome
}

/// The line for a latest release that isn't newer.
fn up_to_date<F>(offer: &Offer<F>) -> String {
    if offer.latest == offer.installed {
        format!(
            "open-ferry {} is installed, the latest release.",
            offer.installed
        )
    } else {
        format!(
            "open-ferry {} is installed; the latest release is {}, which isn't newer.",
            offer.installed, offer.latest
        )
    }
}

/// Says why automatic updates skipped `offer`, if they did.
fn say_skipped<F>(offer: &Offer<F>, console: &mut Console<'_>) {
    if offer.failed_here {
        console.say(
            "It didn't run on this machine before, so automatic updates skip it; `open-ferry update` tries it again.",
        );
    }
    if offer.rolled_back {
        console.say(
            "It was rolled back from, so automatic updates skip it; `open-ferry update` installs it again.",
        );
    }
}

/// `update`.
async fn update<U: Updates>(
    updates: &U,
    settings: &Settings,
    yes: bool,
    prompt: &mut dyn Prompt,
    console: &mut Console<'_>,
) -> Outcome {
    let mut outcome = Outcome::new("update", settings);
    let offer = match updates.latest().await {
        Ok(offer) => offer,
        Err(error) => return outcome.failed(console, error.to_string()),
    };
    outcome.latest_version = Some(offer.latest.clone());
    outcome.installed_version = Some(offer.installed.clone());
    if !offer.newer {
        console.say(up_to_date(&offer));
        return outcome.with("up-to-date", 0);
    }
    let binary = match &offer.install {
        Install::SelfUpdating { binary } => binary.clone(),
        Install::NotifyOnly(why) => {
            console.say(format!(
                "open-ferry {} is out ({} is installed), but this install doesn't update itself: {why}.",
                offer.latest, offer.installed
            ));
            return outcome.with("cannot-update", NOT_INSTALLED);
        }
    };
    console.say(format!(
        "open-ferry {} is out ({} is installed at {}).",
        offer.latest,
        offer.installed,
        binary.display()
    ));
    if offer.failed_here {
        console.say("It didn't run on this machine before; trying it again because you asked.");
    }
    if offer.rolled_back {
        console.say("It was rolled back from; installing it again because you asked.");
    }
    console.say(format!(
        "This downloads it, checks its signature and SHA-256, runs it with --version, and puts it in place of {}. {} is kept for `open-ferry update -rollback`. Nothing is restarted.",
        binary.display(),
        offer.installed
    ));
    if let Some(declined) = confirm(
        yes,
        "Install it?",
        "open-ferry update -yes",
        prompt,
        console,
    ) {
        return outcome.with(declined, NOT_INSTALLED);
    }
    match updates.update(offer.found).await {
        Ok(record) => {
            console.say(format!(
                "open-ferry {} is installed at {} (it was {}).",
                record.to, record.binary, record.from
            ));
            say_restart(&record, console);
            console.say(format!(
                "`open-ferry update -rollback` puts {} back.",
                record.from
            ));
            outcome.switch = Some(record);
            outcome.with("updated", 0)
        }
        Err(error) => outcome.failed(console, error.to_string()),
    }
}

/// `-rollback`.
async fn rollback<U: Updates>(
    updates: &U,
    settings: &Settings,
    yes: bool,
    prompt: &mut dyn Prompt,
    console: &mut Console<'_>,
) -> Outcome {
    let mut outcome = Outcome::new("rollback", settings);
    let plan = match updates.rollback_plan() {
        Ok(plan) => plan,
        Err(error) => return outcome.failed(console, error.to_string()),
    };
    outcome.installed_version = Some(plan.installed.clone());
    console.say(format!(
        "This puts open-ferry {} back in place of {} at {}, and keeps {} so `open-ferry update` can install it again; automatic updates skip it until a newer release. Nothing is restarted.",
        plan.previous,
        plan.installed,
        plan.binary.display(),
        plan.installed
    ));
    if let Some(declined) = confirm(
        yes,
        "Roll back?",
        "open-ferry update -rollback -yes",
        prompt,
        console,
    ) {
        // Not an update left out, so not 3.
        return outcome.with(declined, 1);
    }
    match updates.rollback().await {
        Ok(record) => {
            console.say(format!(
                "open-ferry {} is back at {} (it was {}).",
                record.to, record.binary, record.from
            ));
            say_restart(&record, console);
            outcome.switch = Some(record);
            outcome.with("rolled-back", 0)
        }
        Err(error) => outcome.failed(console, error.to_string()),
    }
}

/// Asks `question` unless `yes`: `None` to go on, else the result for
/// not going on. With `-json`, or no terminal, nothing is asked and
/// nothing changes; `command` is how to go on without being asked.
fn confirm(
    yes: bool,
    question: &str,
    command: &str,
    prompt: &mut dyn Prompt,
    console: &mut Console<'_>,
) -> Option<&'static str> {
    if yes {
        return None;
    }
    let answer = if console.json {
        None
    } else {
        prompt.confirm(question)
    };
    match answer {
        Some(true) => None,
        Some(false) => {
            console.say("Nothing was changed.");
            Some("declined")
        }
        None => {
            console.say(format!(
                "Nothing was changed: there is no terminal to ask in. Run `{command}` to go on without being asked."
            ));
            Some("needs-confirmation")
        }
    }
}

/// Says how to restart open-ferry, if the switch needs it.
fn say_restart(record: &SwitchRecord, console: &mut Console<'_>) {
    if !record.restart_needed {
        console.say(format!("open-ferry {} is running already.", record.to));
        return;
    }
    console.say(format!(
        "Restart open-ferry to run {}. {}",
        record.to,
        restart_hint()
    ));
}

/// How to restart open-ferry, on this system.
fn restart_hint() -> &'static str {
    if cfg!(target_os = "linux") {
        "As a service: `systemctl --user restart open-ferry`, or `sudo systemctl restart open-ferry` for the machine's. Otherwise stop it and start it again."
    } else if cfg!(target_os = "macos") {
        "As a service: `launchctl kickstart -k gui/$(id -u)/io.github.loft-902-co-llc.open-ferry`, or `sudo launchctl kickstart -k system/io.github.loft-902-co-llc.open-ferry` for the machine's. Otherwise stop it and start it again."
    } else if cfg!(windows) {
        "As a service: `schtasks /End /TN open-ferry` then `schtasks /Run /TN open-ferry`, or, as an administrator, `sc.exe stop open-ferry` then `sc.exe start open-ferry` for the machine's. Otherwise stop it and start it again."
    } else {
        "Stop it and start it again the way you started it."
    }
}

/// `-mode`: writes `mode` as `self-update.mode` in the config at `path`,
/// with `env` the value of `OPEN_FERRY_SELF_UPDATE`.
fn set_mode(
    path: &Path,
    mode: SelfUpdateMode,
    env: Option<&str>,
    console: &mut Console<'_>,
) -> Outcome {
    let shown = path.display();
    let mut outcome = Outcome::new("mode", &Settings::resolve(&Default::default(), env));
    outcome.config = Some(shown.to_string());
    if !path.is_file() {
        return outcome.failed(
            console,
            format!(
                "{shown} doesn't exist: write one with `open-ferry init`, or pass -config with your config's path"
            ),
        );
    }
    if let Err(error) = update_nested_scalar(path, &["self-update", "mode"], mode.as_str()) {
        return outcome.failed(console, format!("couldn't write {shown}: {error}"));
    }
    let settings = match Config::load(path) {
        Ok(config) => Settings::resolve(&config.self_update, env),
        Err(error) => {
            return outcome.failed(
                console,
                format!("{shown} was written but doesn't load: {error}"),
            );
        }
    };
    let config = outcome.config.take();
    outcome = Outcome::new("mode", &settings);
    outcome.config = config;
    console.say(format!("self-update.mode is {} in {shown}.", mode.as_str()));
    let mut line = String::new();
    match mode {
        SelfUpdateMode::Off => line.push_str(
            "Automatic updates are off: open-ferry makes no update request. `open-ferry update` still works when you run it.",
        ),
        SelfUpdateMode::Notify => line.push_str(
            "open-ferry only says when a release is out; `open-ferry update` installs it.",
        ),
        SelfUpdateMode::Auto => line.push_str(
            "open-ferry gets a newer release ready when one is out; `open-ferry update` installs it.",
        ),
    }
    if settings.mode < mode {
        let _ = write!(
            line,
            " But {} lowers it to {} where it is set, as it is here.",
            open_ferry_update::MODE_ENV,
            settings.mode.as_str()
        );
    }
    console.say(line);
    console.say("A running server that reads this config follows the change at once.");
    for note in &settings.notes {
        console.say(format!("Note: {note}"));
    }
    outcome.with("mode-set", 0)
}
