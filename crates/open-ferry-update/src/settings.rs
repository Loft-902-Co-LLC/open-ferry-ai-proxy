//! The update settings in force: the config's `self-update` section, and
//! `OPEN_FERRY_SELF_UPDATE`, which can only lower the mode.
//!
//! The mode is `auto` (the default), `notify` or `off`. The environment
//! variable takes `notify` or `off`; set to a mode the config already
//! lowers past, or to `auto`, it changes nothing, and a value that isn't
//! a mode is ignored with a warning, so a typo can't turn updates on.
//!
//! `check-every` is a Go duration, at least [`MIN_CHECK_EVERY`]; a shorter
//! one is raised to it, with a note. Each wait is the interval give or
//! take a tenth, so many servers started together don't check together.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::time::Duration;

use open_ferry_core::config::{SelfUpdate, SelfUpdateMode};

/// The shortest interval between checks.
pub const MIN_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// What set the mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeSource {
    /// Nothing did: the default, `auto`.
    Default,
    /// The config's `self-update.mode`.
    Config,
    /// `OPEN_FERRY_SELF_UPDATE`, which lowered it.
    Environment,
}

impl ModeSource {
    /// The source's name in the status.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Config => "config",
            Self::Environment => "environment",
        }
    }
}

impl fmt::Display for ModeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "the default",
            Self::Config => "self-update.mode in the config",
            Self::Environment => "OPEN_FERRY_SELF_UPDATE in the environment",
        })
    }
}

/// The settings in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The mode.
    pub mode: SelfUpdateMode,
    /// What set it.
    pub source: ModeSource,
    /// The interval between checks, at least [`MIN_CHECK_EVERY`].
    pub check_every: Duration,
    /// Notes on what was changed or ignored, for the log.
    pub notes: Vec<String>,
}

impl Settings {
    /// The settings from the config's section and the variable's value.
    pub fn resolve(section: &SelfUpdate, env: Option<&str>) -> Self {
        let mut notes = Vec::new();
        let (mut mode, mut source) = if section.mode.trim().is_empty() {
            (SelfUpdateMode::Auto, ModeSource::Default)
        } else {
            (section.mode(), ModeSource::Config)
        };
        if let Some(value) = env.map(str::trim).filter(|value| !value.is_empty()) {
            match SelfUpdateMode::parse(value) {
                Some(lowered) if lowered < mode => {
                    mode = lowered;
                    source = ModeSource::Environment;
                }
                Some(_) => {}
                None => notes.push(format!(
                    "{} is {value:?}, not off or notify; it is ignored",
                    crate::MODE_ENV
                )),
            }
        }
        let mut check_every = section.check_every();
        if check_every < MIN_CHECK_EVERY {
            notes.push(format!(
                "self-update.check-every is {}, under the least of 1h; open-ferry checks every hour",
                section.check_every.trim()
            ));
            check_every = MIN_CHECK_EVERY;
        }
        Self {
            mode,
            source,
            check_every,
            notes,
        }
    }

    /// The settings from the config's section and this process's
    /// environment.
    pub fn from_environment(section: &SelfUpdate) -> Self {
        let env = std::env::var(crate::MODE_ENV).ok();
        Self::resolve(section, env.as_deref())
    }

    /// How the mode reads to a person: on, notify-only or off.
    pub fn describe(&self) -> &'static str {
        describe(self.mode)
    }
}

/// How `mode` reads to a person.
pub fn describe(mode: SelfUpdateMode) -> &'static str {
    match mode {
        SelfUpdateMode::Auto => "on",
        SelfUpdateMode::Notify => "notify-only",
        SelfUpdateMode::Off => "off",
    }
}

/// `interval`, give or take a tenth, at random.
pub fn jittered(interval: Duration) -> Duration {
    let random = RandomState::new().hash_one(std::time::SystemTime::now());
    spread(interval, random)
}

/// `interval` moved by up to a tenth either way, by `random`.
pub fn spread(interval: Duration, random: u64) -> Duration {
    let tenth = interval / 10;
    let range = tenth.as_millis().saturating_mul(2).saturating_add(1);
    let offset = u128::from(random) % range;
    let offset = Duration::from_millis(u64::try_from(offset).unwrap_or(0));
    (interval.saturating_sub(tenth)).saturating_add(offset)
}

/// A random wait from `low` to `high`.
pub fn between(low: Duration, high: Duration) -> Duration {
    let random = RandomState::new().hash_one(std::time::SystemTime::now());
    let span = high.saturating_sub(low).as_millis().saturating_add(1);
    let offset = u64::try_from(u128::from(random) % span).unwrap_or(0);
    low.saturating_add(Duration::from_millis(offset))
}
