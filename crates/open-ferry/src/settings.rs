//! Config files checked before a privileged process reads their settings.

use std::path::{Path, PathBuf};

use open_ferry_core::config::Config;

use crate::scope::{self, Probe};

/// The config named by `given`, relative to the working directory, or the
/// default there when none was given.
pub(crate) fn config_path(working_dir: &Path, given: &str) -> PathBuf {
    working_dir.join(if given.is_empty() {
        "config.yaml"
    } else {
        given
    })
}

/// Reads a config only after authorizing it. A missing file isn't reopened.
pub(crate) fn read(probe: &dyn Probe, path: &Path) -> Result<Vec<u8>, String> {
    read_with(probe, path, |path| {
        std::fs::read(path).map_err(|error| error.to_string())
    })
}

/// Reads an authorized config through the caller's file reader.
pub(crate) fn read_with(
    probe: &dyn Probe,
    path: &Path,
    read: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    if !scope::authorize_file(probe, path).map_err(|error| error.to_string())? {
        return Err(format!(
            "failed to read config file: {}: {}",
            path.display(),
            std::io::Error::from(std::io::ErrorKind::NotFound)
        ));
    }
    read(path).map_err(|error| format!("failed to read config file: {}: {error}", path.display()))
}

/// Loads an authorized config.
pub(crate) fn load(probe: &dyn Probe, path: &Path) -> Result<Config, String> {
    Config::load_bytes(&read(probe, path)?).map_err(|error| error.to_string())
}

/// [`load`] without the warnings a load from disk logs once, for callers
/// that re-read the config on a timer or only check it before a load that warns.
pub(crate) fn load_quietly(probe: &dyn Probe, path: &Path) -> Result<Config, String> {
    Config::load_bytes_quietly(&read(probe, path)?).map_err(|error| error.to_string())
}

/// Loads an authorized config and hashes exactly the contents it loaded.
pub(crate) fn load_with_sha256(probe: &dyn Probe, path: &Path) -> Result<(Config, String), String> {
    let data = read(probe, path)?;
    let config = Config::load_bytes(&data).map_err(|error| error.to_string())?;
    Ok((config, open_ferry_core::config::save::sha256_hex(&data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os_service::Platform;
    use crate::update::scope_probe::Fake;

    #[test]
    fn a_container_reads_config_settings_without_exempting_record_trees() {
        let mut probe = crate::scope::tests::Fake::new(Platform::Linux, true);
        probe.container = true;
        probe.file("/work/config.yaml").untrust("/work");
        let data = read_with(&probe, Path::new("/work/config.yaml"), |_| {
            Ok(b"auth-dir: /var/lib/open-ferry\n".to_vec())
        })
        .unwrap();
        let config = Config::load_bytes(&data).unwrap();
        assert!(scope::authorize_auth_dir(&probe, Path::new(&config.auth_dir)).is_err());
    }

    #[test]
    fn the_config_is_relative_to_the_working_directory_unless_it_is_absolute() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(config_path(&cwd, ""), cwd.join("config.yaml"));
        assert_eq!(config_path(&cwd, "other.yaml"), cwd.join("other.yaml"));
        let full = cwd.join("elsewhere/config.yaml");
        assert_eq!(config_path(&cwd, full.to_str().unwrap()), full);
    }

    #[test]
    fn missing_and_refused_configs_never_reach_the_reader() {
        for (platform, path) in [
            (Platform::Linux, "/work/config.yaml"),
            (Platform::Windows, "C:/work/config.yaml"),
            (Platform::MacOs, "/work/config.yaml"),
        ] {
            let mut probe = Fake::new(platform, true);
            let path = Path::new(path);
            let error = read_with(&probe, path, |_| panic!("missing config read")).unwrap_err();
            assert!(error.contains("failed to read config file"), "{error}");
            probe.file(path.to_str().unwrap());
            probe.untrust(path.to_str().unwrap());
            let error = read_with(&probe, path, |_| panic!("refused config read")).unwrap_err();
            assert!(error.contains("Refusing to load"), "{error}");
            probe.untrusted.clear();
            if platform == Platform::MacOs {
                assert!(read_with(&probe, path, |_| panic!("macOS config read")).is_err());
            } else {
                assert_eq!(
                    read_with(&probe, path, |_| Ok(b"trusted".to_vec())).unwrap(),
                    b"trusted"
                );
            }
            probe.privileged = Ok(false);
            assert!(read_with(&probe, path, |_| Ok(Vec::new())).is_ok());
        }
    }
}
