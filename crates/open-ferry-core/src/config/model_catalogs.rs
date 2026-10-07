// Ported from CLIProxyAPI internal/registry/catalog_config.go
// (CatalogSources, Validate) and internal/config/model_catalogs.go
// (ModelCatalogs) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `models` section: where each model catalog is read from.
//!
//! Each source is empty, which keeps the built-in catalog, an absolute path
//! to a local file, or an http(s) URL. [`CatalogSources::validate`] refuses
//! anything else, and a config holding it doesn't load. A path is absolute
//! as Go's `filepath.IsAbs` has it on the same platform: on Windows it needs
//! a drive or a share (`C:/models.json`), elsewhere a leading `/`.
//!
//! Deviations from upstream:
//! - The sources are checked in a fixed order (`catalog`, `codex-catalog`,
//!   `devin-catalog`), so with two bad sources the error always names the
//!   first. Upstream ranges over a Go map, whose order is random.

use std::fmt;
use std::path::Path;

use serde::Deserialize;

use super::RedactedUrl;
use super::diff::go_url;

/// Where each model catalog is read from (upstream's `CatalogSources`, or
/// `ModelCatalogs` in its config). An empty source keeps the built-in
/// catalog.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename = "registry.CatalogSources", rename_all = "kebab-case")]
pub struct CatalogSources {
    /// The general model catalog (`models.json`).
    pub catalog: String,
    /// The Codex client model catalog (`codex_client_models.json`).
    pub codex_catalog: String,
    /// The Devin model catalog. Read and checked, but Devin isn't ported, so
    /// nothing uses it.
    pub devin_catalog: String,
}

impl CatalogSources {
    /// The setting names and their sources, in the order they're checked.
    pub fn entries(&self) -> [(&'static str, &str); 3] {
        [
            ("catalog", &self.catalog),
            ("codex-catalog", &self.codex_catalog),
            ("devin-catalog", &self.devin_catalog),
        ]
    }

    /// Refuses a source that is neither empty, an absolute path nor an
    /// http(s) URL with a host (upstream's `Validate`).
    pub fn validate(&self) -> Result<(), CatalogSourceError> {
        for (name, source) in self.entries() {
            if !is_valid_source(source) {
                return Err(CatalogSourceError { name });
            }
        }
        Ok(())
    }
}

/// Whether `source` is empty, an absolute path, or an http(s) URL with a
/// host.
fn is_valid_source(source: &str) -> bool {
    if source.is_empty() || Path::new(source).is_absolute() {
        return true;
    }
    go_url::parse(source.as_bytes()).is_some_and(|url| {
        matches!(url.scheme.as_str(), "http" | "https") && !url.hostname().is_empty()
    })
}

/// Whether `source` names a URL rather than a file: a valid source that
/// isn't empty or an absolute path.
pub fn is_url_source(source: &str) -> bool {
    !source.is_empty() && !Path::new(source).is_absolute()
}

impl fmt::Debug for CatalogSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatalogSources")
            .field("catalog", &RedactedUrl(&self.catalog))
            .field("codex_catalog", &RedactedUrl(&self.codex_catalog))
            .field("devin_catalog", &RedactedUrl(&self.devin_catalog))
            .finish()
    }
}

/// A catalog source that is neither an http(s) URL nor an absolute path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSourceError {
    name: &'static str,
}

impl CatalogSourceError {
    /// The setting, such as `codex-catalog`.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl fmt::Display for CatalogSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "models.{} must be an http(s) URL or an absolute local path",
            self.name
        )
    }
}

impl std::error::Error for CatalogSourceError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(name: &str, source: &str) -> CatalogSources {
        let mut sources = CatalogSources::default();
        match name {
            "catalog" => sources.catalog = source.to_owned(),
            "codex-catalog" => sources.codex_catalog = source.to_owned(),
            _ => sources.devin_catalog = source.to_owned(),
        }
        sources
    }

    // Ported from internal/config/model_catalogs_test.go
    // (TestModelCatalogConfigValidation): the sources it refuses and
    // accepts, checked directly. The config-level cases are in load.rs.
    #[test]
    fn validate_refuses_relative_paths_and_other_schemes() {
        for name in ["catalog", "codex-catalog", "devin-catalog"] {
            for source in [
                "relative.json",
                "./models.json",
                "~/models.json",
                "ftp://example.com/models",
                "file:///tmp/models.json",
                "https:///models",
            ] {
                let error = sources(name, source).validate().unwrap_err();
                assert_eq!(error.name(), name);
                assert_eq!(
                    error.to_string(),
                    format!("models.{name} must be an http(s) URL or an absolute local path")
                );
            }
            for source in [
                "",
                "https://example.com/models.json",
                "http://127.0.0.1:8080/models.json",
                "HTTPS://Example.com/models.json?key=1",
            ] {
                assert_eq!(sources(name, source).validate(), Ok(()), "{source}");
            }
        }
    }

    // Not upstream's: an absolute path passes on this platform, as Go's
    // filepath.IsAbs passes it; a URL Go refuses to parse is refused.
    #[test]
    fn validate_reads_paths_and_urls_as_go_does() {
        let dir = std::env::temp_dir().join("models.json");
        let absolute = dir.to_string_lossy();
        assert_eq!(sources("catalog", &absolute).validate(), Ok(()));
        for source in [
            "https://exa mple.com/models.json",
            "https://example.com:port/models.json",
            "https://:443/models.json",
            "http//example.com/models.json",
            "/relative-on-windows-only",
        ] {
            let valid = sources("catalog", source).validate().is_ok();
            let want = source.starts_with('/') && cfg!(unix);
            assert_eq!(valid, want, "{source}");
        }
    }

    // Not upstream's: the first bad source in the fixed order is named.
    #[test]
    fn validate_names_the_first_bad_source() {
        let sources = CatalogSources {
            catalog: "https://example.com/models.json".to_owned(),
            codex_catalog: "relative.json".to_owned(),
            devin_catalog: "also-relative.json".to_owned(),
        };
        assert_eq!(sources.validate().unwrap_err().name(), "codex-catalog");
    }

    // Not upstream's: Debug hides a URL's query, which may hold a key.
    #[test]
    fn debug_hides_query_strings() {
        let sources = CatalogSources {
            catalog: "https://example.com/models.json?key=secret".to_owned(),
            ..CatalogSources::default()
        };
        let shown = format!("{sources:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("https://example.com/models.json?<redacted>"));
    }

    // Not upstream's: which valid sources are URLs.
    #[test]
    fn url_sources_are_neither_empty_nor_paths() {
        let path = std::env::temp_dir().join("models.json");
        assert!(!is_url_source(""));
        assert!(!is_url_source(&path.to_string_lossy()));
        assert!(is_url_source("https://example.com/models.json"));
    }
}
