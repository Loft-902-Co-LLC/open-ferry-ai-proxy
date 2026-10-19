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
//! A supervised install switches workers on the same listener. Ctrl-C stops
//! following an accepted switch, without cancelling it; `-cancel` asks the
//! supervisor to cancel while it still can.
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use open_ferry_core::config::Config;
use open_ferry_core::config::save::update_nested_scalar;
use open_ferry_supervisor::client::{self, Detection};
use open_ferry_supervisor::endpoint::{Identity, ServiceKind};
use open_ferry_supervisor::timings::DETECT_TIMEOUT;
use open_ferry_update::background::http_fetch;
use open_ferry_update::install_lock::InstallLock;
use open_ferry_update::state::SwitchRecord;
use open_ferry_update::updater::Found;
use open_ferry_update::{
    CancelOutcome, DataDir, Install, ModeSource, SelfUpdateMode, Settings, Status, SwitchError,
    SwitchEvent, SwitchHandle, SwitchView, Switcher, UpdateError, UpdateService, Updater,
};
use serde::Serialize;

use crate::flags::{self, Definition, FlagError, Kind};
use crate::installed;
use crate::scope;

#[cfg(test)]
pub(crate) mod scope_probe;
#[cfg(test)]
mod tests;

/// The subcommand's name: the first argument that runs it.
pub const NAME: &str = "update";

/// The exit code for a newer release that wasn't installed.
const NOT_INSTALLED: u8 = 3;

/// What the command line asks for.
#[derive(Default)]
struct Options {
    cancel: bool,
    check: bool,
    config: String,
    json: bool,
    mode: Option<String>,
    rollback: bool,
    yes: bool,
}

/// The flags, sorted by name.
const DEFINITIONS: [Definition<Options>; 7] = [
    Definition {
        name: "cancel",
        usage: "Cancel a supervised switch if it has not taken over yet",
        kind: Kind::Bool(|options, value| options.cancel = value),
    },
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
         A supervisor switches versions on the same port; otherwise restart it yourself.\n\
         It works while automatic updates are off.\n\n\
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
    Cancel,
    Check,
    Update,
    Rollback,
    Mode(SelfUpdateMode),
}

impl Request {
    fn action(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Check => "check",
            Self::Update => "update",
            Self::Rollback => "rollback",
            Self::Mode(_) => "mode",
        }
    }
}

/// The request `options` make, or why they make none.
fn request(options: &Options) -> Result<Request, String> {
    let asked = [
        options.cancel,
        options.check,
        options.rollback,
        options.mode.is_some(),
    ]
    .iter()
    .filter(|asked| **asked)
    .count();
    if asked > 1 {
        return Err("use only one of -cancel, -check, -rollback and -mode".to_owned());
    }
    if let Some(mode) = &options.mode {
        if mode.trim().is_empty() {
            return Err("-mode needs a value: off, notify or auto".to_owned());
        }
        return SelfUpdateMode::parse(mode)
            .map(Request::Mode)
            .ok_or_else(|| format!("-mode is {mode:?}; use off, notify or auto"));
    }
    Ok(if options.cancel {
        Request::Cancel
    } else if options.check {
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
    // Before `.env` is loaded: a `.env` in the working directory must not move
    // the data directory this command's supervisor check looks at.
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut console = Console::new(options.json, &mut stdout, &mut stderr);
    let binary = match std::env::current_exe() {
        Ok(binary) => binary,
        Err(error) => {
            let outcome = Outcome::new(
                request.action(),
                &Settings::resolve(&Default::default(), None),
            )
            .failed(
                &mut console,
                format!("can't find the installed binary: {error}"),
            );
            return console.finish(outcome);
        }
    };
    let home = match binary
        .to_str()
        .ok_or_else(|| "the installed binary isn't valid Unicode".to_owned())
        .and_then(|binary| {
            command_home(&scope::Host, binary, request, &options.config)
                .map_err(|error| error.to_string())
        }) {
        Ok(home) => home,
        Err(error) => {
            let outcome = Outcome::new(
                request.action(),
                &Settings::resolve(&Default::default(), None),
            )
            .failed(&mut console, error);
            return console.finish(outcome);
        }
    };
    say_machine_hint(&home, &mut console);
    let dotenv = working_dir.join(".env");
    if let Err(error) = crate::load_dotenv(&dotenv)
        && !error.is_not_found()
    {
        if error.is_refused() {
            let outcome = Outcome::new(
                request.action(),
                &Settings::resolve(&Default::default(), None),
            )
            .failed(&mut console, error.to_string());
            return console.finish(outcome);
        }
        eprintln!(
            "{NAME}: {} doesn't load, so none of it is set: {error}",
            dotenv.display()
        );
    }
    let path = match command_config(
        &scope::Host,
        &home,
        request,
        &options.config,
        &working_dir,
        installed::config_path().ok(),
    ) {
        Ok(path) => path,
        Err(error) => {
            let outcome = Outcome::new(
                request.action(),
                &Settings::resolve(&Default::default(), None),
            )
            .failed(&mut console, error);
            return console.finish(outcome);
        }
    };
    let env = std::env::var(open_ferry_update::MODE_ENV).ok();
    let outcome = match request {
        Request::Mode(mode) => set_mode(&scope::Host, &path, mode, env.as_deref(), &mut console),
        request => manual(&scope::Host, request, &path, &options, home, &mut console),
    };
    console.finish(outcome)
}

/// Finds the records before any config is read, requiring privilege for a change.
fn command_home(
    probe: &dyn scope::Probe,
    binary: &str,
    request: Request,
    config: &str,
) -> Result<scope::Home, scope::Refusal> {
    let home = scope::lookup(probe, binary)?;
    if request != Request::Check && !(matches!(request, Request::Mode(_)) && !config.is_empty()) {
        home.require_privilege()?;
    }
    Ok(home)
}

fn say_machine_hint(home: &scope::Home, console: &mut Console<'_>) {
    if let Some(hint) = &home.machine_hint {
        console.say(hint);
    }
}

/// Machine mode changes use the machine config, never a working directory's.
fn command_config(
    probe: &dyn scope::Probe,
    home: &scope::Home,
    request: Request,
    flag: &str,
    working_dir: &Path,
    installed: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if home.scope == scope::Scope::Machine && matches!(request, Request::Mode(_)) && flag.is_empty()
    {
        if let Some(path) = scope::machine_config(probe)?
            && probe.state(&path) == scope::State::File
        {
            return Ok(PathBuf::from(path));
        }
        return Err(
            "the machine config wasn't found: pass -config with the machine's config path"
                .to_owned(),
        );
    }
    if !flag.is_empty() && probe.privileged()? {
        return Ok(working_dir.join(flag));
    }
    Ok(config_path(flag, working_dir, installed))
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
        return open_ferry_core::auth::path::clean(&working_dir.join(flag));
    }
    let local = working_dir.join("config.yaml");
    if local.is_file() {
        return local;
    }
    let path = installed.filter(|path| path.is_file()).unwrap_or(local);
    open_ferry_core::auth::path::clean(&working_dir.join(path))
}

/// Runs a check, an update or a rollback, with the updater for this
/// binary and the config at `path`.
fn manual(
    probe: &dyn scope::Probe,
    request: Request,
    path: &Path,
    options: &Options,
    home: scope::Home,
    console: &mut Console,
) -> Outcome {
    let config = match load_config(probe, path) {
        Ok(config) => config,
        Err(error) => {
            let settings = Settings::from_environment(&Default::default());
            return Outcome::new(request.action(), &settings).failed(console, error);
        }
    };
    let settings = Settings::from_environment(
        &config
            .as_ref()
            .map(|config| config.self_update.clone())
            .unwrap_or_default(),
    );
    console.mode_line(request, &settings, path, config.is_some());
    let config = config.unwrap_or_default();
    let proxy_url = &config.proxy_url;
    let updater = match real_updater(home.data, proxy_url, request == Request::Check) {
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
    let updates = Real(updater, home.kind);
    let mut terminal = Terminal;
    runtime.block_on(async {
        match updates.supervised(request, &config).await {
            Ok(Some(supervised)) => {
                run(
                    request,
                    &supervised,
                    &settings,
                    options.yes,
                    &mut terminal,
                    console,
                )
                .await
            }
            Ok(None) => {
                run(
                    request,
                    &updates,
                    &settings,
                    options.yes,
                    &mut terminal,
                    console,
                )
                .await
            }
            Err(error) => Outcome::new(request.action(), &settings).failed_update(console, &error),
        }
    })
}

/// The updater for this binary, with the data directory found before `.env`
/// was loaded, downloading through `proxy_url`.
fn real_updater(data: DataDir, proxy_url: &str, read_only: bool) -> Result<Updater, UpdateError> {
    let fetch = http_fetch()(proxy_url).map_err(|error| UpdateError::Fetch {
        what: "the release list",
        error,
    })?;
    let mut updater = Updater::for_this_binary(fetch, data)?;
    updater.system = std::sync::Arc::new(open_ferry_update::install::RealSystem {
        write_probe: !read_only,
    });
    Ok(updater)
}

/// Whether the installed binary `installed` may be replaced by a caller that
/// holds `data`'s update lock: the binary's install lock, which the caller
/// keeps until the switch and its records are done. A supervisor of the data
/// directory, another install, a supervisor of another data directory, or an
/// answer that can't be had, change nothing: [`UpdateError::Supervised`]. The
/// reason, when there is one, comes before the hint on stopping the service.
async fn clear_to_replace_in(
    data: &Path,
    installed: &Path,
    kind: ServiceKind,
) -> Result<InstallLock, UpdateError> {
    let again = "run `open-ferry update` again";
    let hint = || crate::os_service::hint::stop_and_retry(kind, again);
    let identity = Identity::current(kind).map_err(|error| {
        UpdateError::Supervised(format!(
            "could not tell whether an open-ferry supervisor runs: {error}\n{}",
            hint()
        ))
    })?;
    let deadline = std::time::Instant::now() + DETECT_TIMEOUT;
    match client::detect(data, &identity, installed, deadline).await {
        Detection::Clear(lock) => Ok(lock),
        Detection::Supervised(_) => Err(UpdateError::Supervised(hint())),
        Detection::Busy(why) | Detection::Refuse(why) => {
            Err(UpdateError::Supervised(format!("{why}\n{}", hint())))
        }
    }
}

/// What the commands need of the updater.
trait Updates {
    /// What a check found, for an update to install.
    type Found;

    /// The status, for `settings`.
    fn status(&self, settings: &Settings) -> Status;

    fn is_supervised(&self) -> bool {
        false
    }

    async fn switch_view(&self) -> Result<Option<SwitchView>, UpdateError> {
        Ok(None)
    }

    fn record(&self, _id: &str) -> Option<SwitchRecord> {
        None
    }

    async fn cancel(&self) -> Result<CancelOutcome, UpdateError> {
        Ok(CancelOutcome::Nothing)
    }

    /// The latest release, checked; it changes nothing.
    async fn latest(&self) -> Result<Offer<Self::Found>, UpdateError>;

    /// What a rollback would do, or why there can be none.
    fn rollback_plan(&self) -> Result<RollbackPlan, UpdateError>;

    /// Downloads, checks and stages `found`, then switches to it.
    async fn update(&self, found: Self::Found) -> Result<Changed, UpdateError>;

    /// Switches back to the version before the last switch.
    async fn rollback(&self) -> Result<Changed, UpdateError>;
}

enum Changed {
    Replaced(SwitchRecord),
    Supervised {
        handle: SwitchHandle,
        record: SwitchRecord,
    },
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
struct Real(Updater, ServiceKind);

impl Real {
    /// Whether the installed binary may be replaced, asked right after the
    /// update lock is taken. `Ok` holds the binary's install lock, which the
    /// caller keeps until the switch and its records are done. An install
    /// that doesn't update itself is refused here
    /// ([`UpdateError::NotSelfUpdating`]), so that a replacement only ever
    /// runs holding the guard `detect` returned, whatever a later look at the
    /// install says. A supervisor of the data directory, another install, a
    /// supervisor of another data directory, or an answer that can't be had,
    /// change nothing: [`UpdateError::Supervised`].
    async fn clear_to_replace(&self) -> Result<InstallLock, UpdateError> {
        clear_to_replace_for(self.0.install(), self.0.data.root(), self.1).await
    }

    async fn supervised(
        &self,
        request: Request,
        config: &Config,
    ) -> Result<Option<Supervised>, UpdateError> {
        let identity = Identity::current(self.1)
            .map_err(|error| UpdateError::Supervised(error.to_string()))?;
        let switcher = Arc::new(client::EndpointSwitcher::new(
            self.0.data.root(),
            identity.clone(),
        ));
        let service = UpdateService::new(self.0.clone(), http_fetch(), config)
            .with_switcher(switcher.clone());
        if request != Request::Check {
            let binary = match self.0.install() {
                Install::SelfUpdating { binary } => binary,
                Install::NotifyOnly(_) if request == Request::Cancel => self
                    .0
                    .system
                    .current_exe()
                    .map_err(|error| UpdateError::Io {
                        what: "find this command's binary",
                        error: error.to_string(),
                    })?,
                Install::NotifyOnly(_) => return Ok(None),
            };
            let updater = self.0.clone();
            let runtime = tokio::runtime::Handle::current();
            let detection = tokio::task::spawn_blocking(move || {
                let _lock = updater.lock()?;
                Ok::<_, UpdateError>(runtime.block_on(client::detect(
                    updater.data.root(),
                    &identity,
                    &binary,
                    Instant::now() + DETECT_TIMEOUT,
                )))
            })
            .await
            .map_err(preparation_stopped)??;
            match detection {
                Detection::Clear(_) => return Ok(None),
                Detection::Supervised(_) => {}
                Detection::Busy(why) | Detection::Refuse(why) => {
                    return Err(UpdateError::Supervised(why));
                }
            }
        }
        let view = service.switch_view().await.map_err(switch_error)?;
        if request == Request::Check && view.supervisor.is_none() {
            let root = self.0.data.root().to_owned();
            let known = tokio::task::spawn_blocking(move || {
                root.join("supervisor.json").exists()
                    || root.join(open_ferry_update::journal::JOURNAL_FILE).exists()
            })
            .await
            .map_err(preparation_stopped)?;
            if !known {
                return Ok(None);
            }
        }
        Ok(Some(Supervised {
            real: Real(self.0.clone(), self.1),
            service,
            switcher,
            view,
            check: request == Request::Check,
        }))
    }
}

fn switch_error(error: SwitchError) -> UpdateError {
    match error {
        SwitchError::Update(error) => error,
        other => UpdateError::Supervised(other.to_string()),
    }
}

fn preparation_stopped(error: tokio::task::JoinError) -> UpdateError {
    UpdateError::Io {
        what: "prepare the supervised command",
        error: error.to_string(),
    }
}

/// [`clear_to_replace_in`] for `install`, which must be self-updating.
async fn clear_to_replace_for(
    install: Install,
    data: &Path,
    kind: ServiceKind,
) -> Result<InstallLock, UpdateError> {
    match install {
        Install::SelfUpdating { binary } => clear_to_replace_in(data, &binary, kind).await,
        Install::NotifyOnly(why) => Err(UpdateError::NotSelfUpdating(why)),
    }
}

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

    async fn update(&self, found: Found) -> Result<Changed, UpdateError> {
        let _lock = self.0.lock()?;
        let _install = self.clear_to_replace().await?;
        let mut state = self.0.state();
        state.latest = Some(found.release.version.clone());
        self.0.stage(&found, &mut state).await?;
        self.0.switch_to_staged(&mut state).map(Changed::Replaced)
    }

    async fn rollback(&self) -> Result<Changed, UpdateError> {
        let _lock = self.0.lock()?;
        let _install = self.clear_to_replace().await?;
        let mut state = self.0.state();
        self.0.rollback(&mut state).await.map(Changed::Replaced)
    }
}

struct Supervised {
    real: Real,
    service: UpdateService,
    switcher: Arc<client::EndpointSwitcher>,
    view: SwitchView,
    check: bool,
}

struct SupervisedRelease {
    version: String,
    found: Option<Found>,
}

impl Supervised {
    fn running(&self) -> &str {
        self.view
            .supervisor
            .as_ref()
            .and_then(|snapshot| snapshot["active_version"].as_str())
            .unwrap_or(&self.real.0.running)
    }

    fn binary(&self) -> Result<PathBuf, UpdateError> {
        match self.real.0.install() {
            Install::SelfUpdating { binary } => Ok(binary),
            Install::NotifyOnly(why) => Err(UpdateError::NotSelfUpdating(why)),
        }
    }

    fn changing(&self, handle: SwitchHandle, to: String, how: &str, binary: PathBuf) -> Changed {
        let record = SwitchRecord {
            id: handle.id.clone(),
            from: self.running().into(),
            to,
            how: how.into(),
            binary: binary.display().to_string(),
            blue_green: true,
            ..Default::default()
        };
        Changed::Supervised { handle, record }
    }
}

impl Updates for Supervised {
    type Found = SupervisedRelease;

    fn status(&self, settings: &Settings) -> Status {
        let mut status = self.real.status(settings);
        supervisor_status(&mut status, &self.view);
        status
    }

    fn is_supervised(&self) -> bool {
        true
    }

    async fn switch_view(&self) -> Result<Option<SwitchView>, UpdateError> {
        self.service
            .switch_view()
            .await
            .map(Some)
            .map_err(switch_error)
    }

    fn record(&self, id: &str) -> Option<SwitchRecord> {
        let state = self.real.0.state();
        let mut record = state
            .last_switch
            .into_iter()
            .chain(state.history)
            .find(|record| record.id == id);
        if let Ok(view) = open_ferry_update::journal::JournalView::load(
            &self
                .real
                .0
                .data
                .root()
                .join(open_ferry_update::journal::JOURNAL_FILE),
        ) && let Some(journal) = view.journal()
        {
            for outcome in journal
                .outcomes
                .iter()
                .filter(|outcome| outcome.record.id == id)
            {
                if let Some(record) = &mut record {
                    record.merge(&outcome.record);
                } else {
                    record = Some(outcome.record.clone());
                }
            }
        }
        record
    }

    async fn cancel(&self) -> Result<CancelOutcome, UpdateError> {
        self.service.cancel_switch().await.map_err(switch_error)
    }

    async fn latest(&self) -> Result<Offer<SupervisedRelease>, UpdateError> {
        let state = self.real.0.state();
        let installed = self.running().to_owned();
        // A staged release was already signature- and version-checked. Reuse
        // the latest known stage; pinning rechecks its pointer and bytes.
        if !self.check
            && let Some(version) = state.staged.as_ref().filter(|version| {
                state
                    .latest
                    .as_ref()
                    .is_none_or(|latest| latest == *version)
                    && open_ferry_update::release::valid_version(version)
            })
        {
            let newer = version.parse().is_ok_and(|latest| {
                open_ferry_update::release::compare(&installed, &latest)
                    == open_ferry_update::release::Verdict::Newer
            });
            return Ok(Offer {
                latest: version.clone(),
                installed,
                newer,
                failed_here: state.has_failed(version),
                rolled_back: state.rolled_back.as_ref() == Some(version),
                install: self.real.0.install(),
                found: SupervisedRelease {
                    version: version.clone(),
                    found: None,
                },
            });
        }
        let found = self.real.0.latest(&state).await?;
        let version = found.release.version.clone();
        Ok(Offer {
            newer: open_ferry_update::release::compare(&installed, &found.release.semver)
                == open_ferry_update::release::Verdict::Newer,
            failed_here: state.has_failed(&version),
            rolled_back: state.rolled_back.as_ref() == Some(&version),
            latest: version.clone(),
            installed,
            install: self.real.0.install(),
            found: SupervisedRelease {
                version,
                found: Some(found),
            },
        })
    }

    fn rollback_plan(&self) -> Result<RollbackPlan, UpdateError> {
        Ok(RollbackPlan {
            previous: self
                .view
                .rollback_target
                .clone()
                .ok_or(UpdateError::NoPrevious)?,
            installed: self.running().into(),
            binary: self.binary()?,
        })
    }

    async fn update(&self, release: SupervisedRelease) -> Result<Changed, UpdateError> {
        let binary = self.binary()?;
        let updater = self.real.0.clone();
        let version = release.version.clone();
        let runtime = tokio::runtime::Handle::current();
        let pin = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let _lock = updater.lock()?;
                let mut state = open_ferry_update::State::load_strict(&updater.data.state_file())
                    .map_err(|error| UpdateError::Io {
                    what: "read the update state",
                    error: error.to_string(),
                })?;
                if let Some(found) = release.found {
                    updater.stage(&found, &mut state).await?;
                }
                let sha = state
                    .staged_sha256
                    .as_deref()
                    .ok_or(UpdateError::NothingStaged)?;
                updater.pin_staged(&version, sha)
            })
        })
        .await
        .map_err(preparation_stopped)??;
        let handle = self
            .switcher
            .update(&pin, false, Instant::now() + Duration::from_secs(3))
            .await
            .map_err(switch_error)?;
        Ok(self.changing(handle, release.version, "update", binary))
    }

    async fn rollback(&self) -> Result<Changed, UpdateError> {
        let binary = self.binary()?;
        let to = self
            .view
            .rollback_target
            .clone()
            .ok_or(UpdateError::NoPrevious)?;
        let handle = self.service.roll_back().await.map_err(switch_error)?;
        Ok(self.changing(handle, to, "rollback", binary))
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
                Request::Cancel | Request::Rollback | Request::Mode(_) => {}
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
    /// `mode-set`, `supervised` or `error`.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    switch_view: Option<SwitchView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    progress: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor_result: Option<serde_json::Value>,
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
            switch_view: None,
            operation_id: None,
            progress: Vec::new(),
            supervisor_result: None,
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

    /// The outcome of an update that failed with `error`: `supervised`
    /// when it changed nothing for a supervisor's sake, else `error`.
    fn failed_update(self, console: &mut Console, error: &UpdateError) -> Self {
        let supervised = matches!(error, UpdateError::Supervised(_));
        let mut outcome = self.failed(console, error.to_string());
        if supervised {
            outcome.result = "supervised";
        }
        outcome
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
        Request::Cancel => cancel(updates, settings, console).await,
        Request::Check => check(updates, settings, console).await,
        Request::Update => update(updates, settings, yes, prompt, console).await,
        Request::Rollback => rollback(updates, settings, yes, prompt, console).await,
        Request::Mode(_) => Outcome::new(request.action(), settings)
            .failed(console, "-mode doesn't use the updater".to_owned()),
    }
}

/// `-check`.
async fn check<U: Updates>(updates: &U, settings: &Settings, console: &mut Console<'_>) -> Outcome {
    let mut outcome = Outcome::new("check", settings);
    match updates.switch_view().await {
        Ok(Some(view)) => {
            say_supervisor(&view, console);
            outcome.switch_view = Some(view);
        }
        Ok(None) => {}
        Err(error) => return outcome.failed(console, error.to_string()),
    }
    let mut status = updates.status(settings);
    if let Some(view) = &outcome.switch_view {
        supervisor_status(&mut status, view);
    }
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
        say_up_to_date(updates, &offer, console);
        outcome.with("up-to-date", 0)
    } else if let Some(why) = offer.install.why_not() {
        console.say(format!(
            "open-ferry {} is out ({} is installed), but this install doesn't update itself: {why}.",
            offer.latest, offer.installed
        ));
        outcome.with("cannot-update", NOT_INSTALLED)
    } else {
        let current = if updates.is_supervised() {
            "running"
        } else {
            "installed"
        };
        console.say(format!(
            "open-ferry {} is out ({} is {current}). Run `open-ferry update` to install it.",
            offer.latest, offer.installed,
        ));
        say_skipped(&offer, console);
        outcome.with("update-available", NOT_INSTALLED)
    };
    if status.restart_needed {
        if updates.is_supervised() {
            console.say("The supervisor needs a restart to support the new worker; restart the service when convenient.");
        } else {
            console.say(format!(
                "open-ferry {} is installed, but {} is running: restart open-ferry to run it.",
                status.installed_version, status.running_version
            ));
        }
    }
    outcome.latest_version = Some(offer.latest);
    outcome.installed_version = Some(if updates.is_supervised() {
        status.installed_version.clone()
    } else {
        offer.installed
    });
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

fn say_up_to_date<U: Updates>(updates: &U, offer: &Offer<U::Found>, console: &mut Console<'_>) {
    if updates.is_supervised() {
        console.say(format!(
            "open-ferry {} is running; the latest release is {}.",
            offer.installed, offer.latest
        ));
    } else {
        console.say(up_to_date(offer));
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
    outcome.installed_version = Some(if updates.is_supervised() {
        updates.status(settings).installed_version
    } else {
        offer.installed.clone()
    });
    if !offer.newer {
        say_up_to_date(updates, &offer, console);
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
    if updates.is_supervised() {
        console.say(format!(
            "open-ferry {} is out ({} is running).",
            offer.latest, offer.installed
        ));
    } else {
        console.say(format!(
            "open-ferry {} is out ({} is installed at {}).",
            offer.latest,
            offer.installed,
            binary.display()
        ));
    }
    if offer.failed_here {
        console.say("It didn't run on this machine before; trying it again because you asked.");
    }
    if offer.rolled_back {
        console.say("It was rolled back from; installing it again because you asked.");
    }
    if updates.is_supervised() {
        console.say(format!(
            "This checks and stages the release, then asks the supervisor to switch versions on the same port. {} is kept for `open-ferry update -rollback`.",
            offer.installed
        ));
    } else {
        console.say(format!(
        "This downloads it, checks its signature and SHA-256, runs it with --version, and puts it in place of {}. {} is kept for `open-ferry update -rollback`. Nothing is restarted.",
        binary.display(),
        offer.installed
    ));
    }
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
        Ok(Changed::Supervised { handle, record }) => {
            follow_until(
                updates,
                handle,
                record,
                outcome,
                console,
                tokio::signal::ctrl_c(),
            )
            .await
        }
        Ok(Changed::Replaced(record)) => {
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
        Err(error) => outcome.failed_update(console, &error),
    }
}

fn supervisor_status(status: &mut Status, view: &SwitchView) {
    status.previous_version = view.rollback_target.clone();
    if let Some(snapshot) = &view.supervisor {
        if let Some(running) = snapshot["active_version"].as_str() {
            status.running_version = running.into();
        }
        if let Some(installed) = snapshot["installed_version"].as_str() {
            status.installed_version = installed.into();
        }
        status.restart_needed = snapshot["needs_restart"].as_bool().unwrap_or(false);
        if let Some(why) = snapshot["switching_disabled"].as_str() {
            status.can_update_itself = false;
            status.why_not = Some(why.into());
            status.why_not_code = Some("switching-disabled");
        }
    }
}

fn say_supervisor(view: &SwitchView, console: &mut Console<'_>) {
    if let Some(snapshot) = &view.supervisor {
        let phase = snapshot["switch"]["phase"]
            .as_str()
            .or_else(|| snapshot["state"].as_str())
            .unwrap_or("unknown");
        console.say(format!(
            "Supervisor: {phase}; running open-ferry {}.",
            snapshot["active_version"].as_str().unwrap_or("unknown")
        ));
        if let Some(until) = snapshot["switch"]["watch_until"].as_str() {
            console.say(format!("Watch deadline: {until}."));
        }
        if let Some(why) = snapshot["switching_disabled"].as_str() {
            console.say(format!("Switching is disabled: {why}"));
        } else {
            console.say("Switching is enabled.");
        }
    }
    if let Some(why) = &view.unreachable {
        console.say(format!("Supervisor unavailable: {why}."));
    }
    console.say(format!(
        "Rollback target: {}.",
        view.rollback_target.as_deref().unwrap_or("none")
    ));
    if let Some(last) = &view.last {
        console.say(format!(
            "Last switch: {} from {} to {}: {}.",
            last.how,
            last.from.as_deref().unwrap_or("unknown"),
            last.to,
            last.result
        ));
        if let Some(reason) = &last.reason {
            console.say(reason);
        }
    } else {
        console.say("Last switch: none.");
    }
}

async fn cancel<U: Updates>(
    updates: &U,
    settings: &Settings,
    console: &mut Console<'_>,
) -> Outcome {
    let outcome = Outcome::new("cancel", settings);
    match updates.cancel().await {
        Ok(CancelOutcome::Cancelled) => {
            console.say("The supervisor accepted the cancellation.");
            outcome.with("cancelled", 0)
        }
        Ok(CancelOutcome::TooLate) => {
            console.say("It is too late to cancel: the switch has taken over. Use `open-ferry update -check` to see its status.");
            outcome.with("too-late", 1)
        }
        Ok(CancelOutcome::Nothing) => {
            console.say("Nothing is switching.");
            outcome.with("nothing-switching", 0)
        }
        Err(error) => outcome.failed_update(console, &error),
    }
}

async fn follow_until<U: Updates>(
    updates: &U,
    mut handle: SwitchHandle,
    mut record: SwitchRecord,
    mut outcome: Outcome,
    console: &mut Console<'_>,
    stop: impl std::future::Future<Output = io::Result<()>>,
) -> Outcome {
    outcome.operation_id = Some(handle.id.clone());
    console.say(format!("Switch {} accepted. Ctrl-C stops following; the supervisor continues the switch. Use `open-ferry update -cancel` to ask it to cancel.", handle.id));
    tokio::pin!(stop);
    loop {
        let event = tokio::select! {
            biased;
            stopped = &mut stop => {
                if let Err(error) = stopped { return outcome.failed(console, format!("could not listen for Ctrl-C: {error}; the supervisor continues the switch")); }
                console.say("Stopped following. The supervisor continues the switch; it was not cancelled. Use `open-ferry update -check` to see its status.");
                return outcome.with("following-stopped", 0);
            }
            event = handle.next(Instant::now() + Duration::from_secs(30)) => event,
        };
        match event {
            Ok(Some(SwitchEvent::Progress { phase })) => {
                console.say(format!("Switch {}: {phase}.", handle.id));
                outcome.progress.push(phase);
            }
            Ok(Some(SwitchEvent::Done { result })) => {
                let received = result["result"].as_str().unwrap_or("unknown").to_owned();
                outcome.supervisor_result = Some(result.clone());
                if let Some(current) = updates.record(&handle.id) { record = current; }
                else { record.result = Some(received); }
                let word = record.result.as_deref().unwrap_or("unknown").to_owned();
                outcome.switch = Some(record.clone());
                if matches!(word.as_str(), "switched" | "rolled-back") {
                    console.say(format!("open-ferry {} is running (it was {}).", record.to, record.from));
                    console.say(if record.confirmed { "The supervisor confirmed the version." } else { "The supervisor watches it before confirming the installed file." });
                    console.say("Use `open-ferry update -check` for the watch deadline and last outcome.");
                    if record.restart_needed { console.say("Restart the service when convenient to update its supervisor."); }
                    return outcome.with(if word == "rolled-back" { "rolled-back" } else { "updated" }, 0);
                }
                let reason = record.why.as_deref().or_else(|| result["why"].as_str()).unwrap_or(&word);
                return outcome.failed(console, format!("the supervised switch ended: {reason}"));
            }
            Err(SwitchError::TimedOut) => {}
            Err(error) => return outcome.failed(console, format!("{error}. Use `open-ferry update -check` before trying again.")),
            Ok(None) => return outcome.failed(console, "the switch stream ended without an outcome; use `open-ferry update -check` before trying again".into()),
        }
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
    outcome.installed_version = Some(if updates.is_supervised() {
        updates.status(settings).installed_version
    } else {
        plan.installed.clone()
    });
    if updates.is_supervised() {
        console.say(format!(
            "This asks the supervisor to switch from open-ferry {} back to {} on the same port. Automatic updates skip the version you rolled back from.",
            plan.installed, plan.previous
        ));
    } else {
        console.say(format!(
        "This puts open-ferry {} back in place of {} at {}, and keeps {} so `open-ferry update` can install it again; automatic updates skip it until a newer release. Nothing is restarted.",
        plan.previous,
        plan.installed,
        plan.binary.display(),
        plan.installed
    ));
    }
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
        Ok(Changed::Supervised { handle, record }) => {
            follow_until(
                updates,
                handle,
                record,
                outcome,
                console,
                tokio::signal::ctrl_c(),
            )
            .await
        }
        Ok(Changed::Replaced(record)) => {
            console.say(format!(
                "open-ferry {} is back at {} (it was {}).",
                record.to, record.binary, record.from
            ));
            say_restart(&record, console);
            outcome.switch = Some(record);
            outcome.with("rolled-back", 0)
        }
        Err(error) => outcome.failed_update(console, &error),
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

/// Reads settings only after the file is authorized; a missing file has none.
fn load_config(probe: &dyn scope::Probe, path: &Path) -> Result<Option<Config>, String> {
    if !scope::authorize_file(probe, path).map_err(|refusal| refusal.to_string())?
        || !path.is_file()
    {
        return Ok(None);
    }
    Config::load(path)
        .map(Some)
        .map_err(|error| format!("{} doesn't load: {error}", path.display()))
}

/// `-mode`: writes `mode` as `self-update.mode` in the config at `path`,
/// with `env` the value of `OPEN_FERRY_SELF_UPDATE`.
fn set_mode(
    probe: &dyn scope::Probe,
    path: &Path,
    mode: SelfUpdateMode,
    env: Option<&str>,
    console: &mut Console<'_>,
) -> Outcome {
    let shown = path.display();
    let mut outcome = Outcome::new("mode", &Settings::resolve(&Default::default(), env));
    outcome.config = Some(shown.to_string());
    let present = match scope::authorize_file(probe, path) {
        Ok(present) => present,
        Err(refusal) => return outcome.failed(console, refusal.to_string()),
    };
    if !present || !path.is_file() {
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
