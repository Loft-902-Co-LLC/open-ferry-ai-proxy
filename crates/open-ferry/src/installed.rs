//! Where an installed open-ferry keeps its config: `open-ferry init` writes
//! it there when no `-config` is given.
//!
//! - Linux and macOS: `$XDG_CONFIG_HOME/open-ferry/config.yaml`, or
//!   `~/.config/open-ferry/config.yaml` when `XDG_CONFIG_HOME` isn't set or
//!   isn't an absolute path.
//! - Windows: `%APPDATA%\open-ferry\config.yaml`.
//!
//! The server doesn't look there: it reads `-config`, else `config.yaml` in
//! the working directory, as upstream does. Upstream has no installed
//! layout.

use std::ffi::OsString;
use std::path::PathBuf;

/// The directory below the user's config directory.
const DIR: &str = "open-ferry";

/// The config file's name.
const FILE: &str = "config.yaml";

/// The installed config's path on this system.
pub fn config_path() -> Result<PathBuf, String> {
    config_path_for(cfg!(windows), |name| std::env::var_os(name))
}

/// The installed config's path, on Windows or elsewhere, with the
/// environment `var`.
fn config_path_for(
    windows: bool,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    let set = |name| var(name).filter(|value| !value.is_empty());
    let base = if windows {
        set("APPDATA")
            .map(PathBuf::from)
            .ok_or("APPDATA isn't set, so there is no default config path; pass -config")?
    } else if let Some(dir) = set("XDG_CONFIG_HOME").filter(starts_with_slash) {
        PathBuf::from(dir)
    } else {
        set("HOME")
            .map(|home| PathBuf::from(home).join(".config"))
            .ok_or("HOME isn't set, so there is no default config path; pass -config")?
    };
    Ok(base.join(DIR).join(FILE))
}

/// Whether `path` is absolute on Unix, whatever system reads it.
fn starts_with_slash(path: &OsString) -> bool {
    path.to_string_lossy().starts_with('/')
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    fn path_for(windows: bool, vars: &[(&str, &str)]) -> Result<PathBuf, String> {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        config_path_for(windows, |name| vars.get(name).map(OsString::from))
    }

    // Not upstream's: the installed config's path on each system.
    #[test]
    fn finds_the_installed_config_path() {
        let roaming = r"C:\Users\u\AppData\Roaming";
        assert_eq!(
            path_for(true, &[("APPDATA", roaming), ("HOME", "/home/u")]).unwrap(),
            Path::new(roaming).join("open-ferry").join("config.yaml")
        );
        assert!(path_for(true, &[("APPDATA", "")]).is_err());
        assert!(path_for(true, &[]).is_err());

        assert_eq!(
            path_for(false, &[("HOME", "/home/u")]).unwrap(),
            Path::new("/home/u")
                .join(".config")
                .join("open-ferry")
                .join("config.yaml")
        );
        assert_eq!(
            path_for(false, &[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/xdg")]).unwrap(),
            Path::new("/xdg").join("open-ferry").join("config.yaml")
        );
        for xdg in ["", "relative/dir"] {
            assert_eq!(
                path_for(false, &[("HOME", "/home/u"), ("XDG_CONFIG_HOME", xdg)]).unwrap(),
                Path::new("/home/u")
                    .join(".config")
                    .join("open-ferry")
                    .join("config.yaml")
            );
        }
        assert!(path_for(false, &[("APPDATA", "/x")]).is_err());
    }
}
