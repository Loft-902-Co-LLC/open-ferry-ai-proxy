// Ported from CLIProxyAPI internal/registry/model_updater.go (modelStore,
// getModels), internal/registry/catalog_sources.go (publishCatalogBytes)
// and internal/registry/codex_client_models.go (codexClientCatalogStore,
// loadCodexClientModelsFromBytes) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The model catalogs in use, and publishing new ones.
//!
//! [`CatalogStore::global`] holds the process's: the general catalog
//! ([`StaticCatalog::current`]), the Codex client catalog
//! ([`CodexClientCatalog::current`]) and the translators' catalog, made from
//! the general one ([`TranslatorCatalog::current`]). Each starts as the
//! built-in one. A catalog is checked before it is published, then swapped
//! in whole: a reader holds the old catalog or the new one, never a mix, and
//! keeps the one it took for as long as it needs it.
//!
//! A general catalog without Meta models keeps the Meta models of the one
//! before, as upstream's does, and publishing it says which providers'
//! models changed ([`StaticCatalog::changed_providers`]), so their
//! credentials' models can be registered again. A Codex client catalog
//! changes when its bytes do. Each byte that isn't part of a UTF-8
//! character reads as U+FFFD, as Go's decoder reads it inside a string.
//!
//! Deviations from upstream: none.

use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use open_ferry_translate::models::ModelCatalog as TranslatorCatalog;

use super::codex_client::{self, CodexCatalogError, CodexClientCatalog};
use super::definitions::{CatalogError, StaticCatalog};
use crate::multipart::lossy;

/// The process's catalogs.
static GLOBAL: LazyLock<Arc<CatalogStore>> =
    LazyLock::new(|| Arc::new(CatalogStore::built_in(true)));

/// The general and Codex client model catalogs in use.
pub struct CatalogStore {
    general: RwLock<Arc<StaticCatalog>>,
    codex: RwLock<CodexState>,
    /// Whether publishing a general catalog also gives the translators its
    /// models: only the process's store does.
    translators: bool,
}

/// The Codex client catalog in use, and the bytes it was read from.
struct CodexState {
    data: Arc<[u8]>,
    catalog: Option<Arc<CodexClientCatalog>>,
}

impl Default for CatalogStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogStore {
    /// The process's catalogs, which [`StaticCatalog::current`],
    /// [`CodexClientCatalog::current`] and the translators read.
    pub fn global() -> &'static Self {
        &GLOBAL
    }

    /// The process's catalogs, shared.
    pub(crate) fn global_shared() -> Arc<Self> {
        Arc::clone(&GLOBAL)
    }

    /// Catalogs of their own, starting as the built-in ones. Publishing to
    /// them changes nothing else; tests use them.
    pub fn new() -> Self {
        Self::built_in(false)
    }

    fn built_in(translators: bool) -> Self {
        Self {
            general: RwLock::new(StaticCatalog::embedded_shared()),
            codex: RwLock::new(CodexState {
                data: Arc::from(codex_client::EMBEDDED_CATALOG),
                catalog: CodexClientCatalog::embedded_shared(),
            }),
            translators,
        }
    }

    /// The general catalog in use.
    pub fn general(&self) -> Arc<StaticCatalog> {
        Arc::clone(&read(&self.general))
    }

    /// The Codex client catalog in use, if one loaded.
    pub fn codex(&self) -> Option<Arc<CodexClientCatalog>> {
        read(&self.codex).catalog.clone()
    }

    /// Publishes `data` as the general catalog if it is valid, and returns
    /// the providers whose models changed (upstream's
    /// `publishCatalogBytes`). `origin` names the catalog in errors.
    pub fn publish_general(&self, data: &[u8], origin: &str) -> Result<Vec<String>, CatalogError> {
        let mut catalog = StaticCatalog::from_json(&lossy(data), origin)?;
        let mut current = write(&self.general);
        catalog.keep_meta_of(&current);
        let changed = current.changed_providers(&catalog);
        let catalog = Arc::new(catalog);
        if self.translators {
            TranslatorCatalog::set_current(Arc::new(catalog.translator_catalog()));
        }
        *current = catalog;
        Ok(changed)
    }

    /// Publishes `data` as the Codex client catalog if it is valid, and
    /// returns whether it changed (upstream's
    /// `loadCodexClientModelsFromBytes` with the source `catalog`).
    pub fn publish_codex(&self, data: &[u8]) -> Result<bool, CodexCatalogError> {
        let catalog =
            CodexClientCatalog::from_json(data).map_err(|error| error.with_source("catalog"))?;
        let mut current = write(&self.codex);
        if *current.data == *data {
            return Ok(false);
        }
        *current = CodexState {
            data: Arc::from(data),
            catalog: Some(Arc::new(catalog)),
        };
        Ok(true)
    }
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CodexPlan;

    /// The built-in general catalog's text.
    fn built_in() -> &'static [u8] {
        open_ferry_translate::models::embedded_catalog_json().as_bytes()
    }

    /// The built-in general catalog with `model` added to `section`.
    fn with_model(section: &str, model: serde_json::Value) -> Vec<u8> {
        let mut root: serde_json::Value = serde_json::from_slice(built_in()).unwrap();
        root[section].as_array_mut().unwrap().push(model);
        serde_json::to_vec(&root).unwrap()
    }

    // Not upstream's: the built-in catalogs are in use until one is
    // published.
    #[test]
    fn a_store_starts_with_the_built_in_catalogs() {
        let store = CatalogStore::new();
        assert_eq!(*store.general(), *StaticCatalog::embedded());
        assert!(store.codex().is_some());
        assert_eq!(
            *CatalogStore::global().general(),
            *StaticCatalog::embedded()
        );
    }

    // Not upstream's, for publishCatalogBytes: a valid catalog is swapped
    // in and names the providers that changed; the same catalog again
    // changes nothing; an invalid one is refused and the last valid one
    // stays.
    #[test]
    fn publishing_a_general_catalog_names_the_changed_providers() {
        let store = CatalogStore::new();
        assert_eq!(
            store.publish_general(built_in(), "test"),
            Ok(Vec::new()),
            "the built-in catalog again"
        );
        let data = with_model("claude", serde_json::json!({"id": "claude-new"}));
        assert_eq!(
            store.publish_general(&data, "test"),
            Ok(vec!["claude".to_owned()])
        );
        assert!(store.general().lookup("claude-new").is_some());
        assert_eq!(store.publish_general(&data, "test"), Ok(Vec::new()));

        let error = store
            .publish_general(br#"{"claude":[{"id":""}]}"#, "test")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "test: validate models catalog: claude[0] has empty id"
        );
        assert!(store.general().lookup("claude-new").is_some());
        assert!(
            store.publish_general(b"{", "test").is_err(),
            "a catalog that doesn't decode"
        );
        assert!(store.general().lookup("claude-new").is_some());
    }

    // Not upstream's, for publishCatalogBytes: a catalog without Meta models
    // keeps the ones before.
    #[test]
    fn a_catalog_without_meta_models_keeps_the_last_ones() {
        let store = CatalogStore::new();
        let meta = store.general().meta_models();
        assert!(!meta.is_empty());
        let changed = store
            .publish_general(br#"{"codex-pro":[{"id":"gpt-new"}]}"#, "test")
            .unwrap();
        assert!(!changed.contains(&"meta".to_owned()), "{changed:?}");
        assert_eq!(store.general().meta_models(), meta);
        assert!(store.general().claude_models().is_empty());
        let codex: Vec<String> = store
            .general()
            .codex_models(CodexPlan::Pro)
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert!(codex.contains(&"gpt-new".to_owned()), "{codex:?}");
    }

    // Not upstream's: publishing to a store of its own leaves the
    // translators' catalog alone.
    #[test]
    fn a_store_of_its_own_leaves_the_translators_alone() {
        let store = CatalogStore::new();
        let data = with_model("claude", serde_json::json!({"id": "claude-private"}));
        store.publish_general(&data, "test").unwrap();
        assert!(
            TranslatorCatalog::current()
                .lookup("claude-private")
                .is_none()
        );
    }

    // Not upstream's, for loadCodexClientModelsFromBytes: the same bytes
    // are no change; other valid bytes are; invalid ones are refused, named
    // `catalog`, and the last valid catalog stays.
    #[test]
    fn publishing_a_codex_catalog_compares_bytes() {
        let store = CatalogStore::new();
        assert_eq!(
            store.publish_codex(codex_client::EMBEDDED_CATALOG),
            Ok(false)
        );
        let mut root: serde_json::Value =
            serde_json::from_slice(codex_client::EMBEDDED_CATALOG).unwrap();
        let models = root["models"].as_array_mut().unwrap();
        models.retain(|model| model["slug"] == codex_client::DEFAULT_TEMPLATE);
        let data = serde_json::to_vec(&root).unwrap();
        assert_eq!(store.publish_codex(&data), Ok(true));
        assert_eq!(store.codex().unwrap().templates().count(), 1);
        assert_eq!(store.publish_codex(&data), Ok(false));

        let error = store.publish_codex(br#"{"models":[]}"#).unwrap_err();
        assert_eq!(
            error.to_string(),
            "catalog: Codex client model catalog has no models"
        );
        assert_eq!(store.codex().unwrap().templates().count(), 1);
    }
}
