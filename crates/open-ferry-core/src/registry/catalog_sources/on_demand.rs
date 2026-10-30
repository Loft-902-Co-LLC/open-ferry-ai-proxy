// Ported from CLIProxyAPI internal/registry/catalog_sources.go
// (refreshOnDemand, RefreshModelCatalogsWithPolicy) (v8.0.47, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! On-demand file catalog refresh and publication fencing.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use super::*;

/// Cancels publication of a refresh, including a file read already in progress.
/// The last cancellation check starts publication. Cancelling never waits for
/// parsing already in progress, and fences subsequent catalog publications.
#[derive(Clone, Default)]
pub struct RefreshCancellation(Arc<AtomicBool>);

impl RefreshCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Default, Serialize)]
pub struct CatalogSourceStatus {
    pub source: String,
    pub refreshed: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed_providers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct CatalogRefreshResult {
    pub catalog: String,
    pub codex_catalog: String,
    pub devin_catalog: String,
    pub sources: BTreeMap<String, String>,
    pub catalogs: BTreeMap<String, CatalogSourceStatus>,
    pub changed_providers: Vec<String>,
}

impl CatalogRefreshResult {
    pub fn has_errors(&self) -> bool {
        self.catalogs.values().any(|status| status.error.is_some())
    }
}

impl CatalogRuntime {
    /// Refreshes file sources immediately. Active updaters take precedence over
    /// fallback settings; embedded, URL and unserved sources are skipped.
    pub fn refresh(
        &self,
        fallback: &CatalogSources,
        cancellation: &RefreshCancellation,
    ) -> Result<CatalogRefreshResult, String> {
        fallback.validate().map_err(|error| error.to_string())?;
        // Snapshot sources and generations together with runtime configuration.
        let running = lock(&self.inner.running);
        let general = self.inner.general.request(&fallback.catalog);
        let codex = self.inner.codex.request(&fallback.codex_catalog);
        drop(running);
        let general = self.inner.general.on_demand(general, cancellation);
        let codex = self.inner.codex.on_demand(codex, cancellation);
        let devin = CatalogSourceStatus {
            source: display_source(&fallback.devin_catalog),
            skipped: true,
            ..CatalogSourceStatus::default()
        };
        let changed = merge_provider_names(
            general.changed_providers.clone(),
            codex.changed_providers.clone(),
        );
        let catalogs: BTreeMap<_, _> = [
            ("catalog".to_owned(), general),
            ("codex-catalog".to_owned(), codex),
            ("devin-catalog".to_owned(), devin),
        ]
        .into();
        Ok(CatalogRefreshResult {
            catalog: catalogs["catalog"].source.clone(),
            codex_catalog: catalogs["codex-catalog"].source.clone(),
            devin_catalog: catalogs["devin-catalog"].source.clone(),
            sources: catalogs
                .iter()
                .map(|(name, status)| (name.clone(), status.source.clone()))
                .collect(),
            catalogs,
            changed_providers: changed,
        })
    }
}

fn display_source(source: &str) -> String {
    if source.is_empty() {
        "embed".to_owned()
    } else if is_url_source(source) {
        "URL (not fetched)".to_owned()
    } else {
        source.to_owned()
    }
}

pub(super) struct Request {
    source: String,
    generation: u64,
    seq: u64,
}

impl Updater {
    pub(super) fn next_sequence(&self) -> u64 {
        let mut state = lock(&self.state);
        state.request_seq += 1;
        state.request_seq
    }

    pub(super) fn request(&self, fallback: &str) -> Request {
        let mut state = lock(&self.state);
        state.request_seq += 1;
        Request {
            source: if state.active {
                state.source.clone()
            } else {
                fallback.to_owned()
            },
            generation: state.generation,
            seq: state.request_seq,
        }
    }

    pub(super) fn on_demand(
        &self,
        request: Request,
        cancellation: &RefreshCancellation,
    ) -> CatalogSourceStatus {
        let mut status = CatalogSourceStatus {
            source: display_source(&request.source),
            ..CatalogSourceStatus::default()
        };
        if request.source.is_empty() || is_url_source(&request.source) {
            status.skipped = true;
            return status;
        }
        match self.read_and_publish(
            &request.source,
            request.generation,
            request.seq,
            cancellation,
        ) {
            Ok(changed) => {
                status.refreshed = true;
                status.changed_providers = changed;
            }
            Err(error) => status.error = Some(error),
        }
        status
    }

    pub(super) fn read_and_publish(
        &self,
        source: &str,
        generation: u64,
        seq: u64,
        cancellation: &RefreshCancellation,
    ) -> Result<Vec<String>, String> {
        if cancellation.0.load(Ordering::Acquire) {
            return Err("model catalog refresh cancelled".to_owned());
        }
        let data = (self.fetch)(source)?;
        let mut state = lock(&self.state);
        if cancellation.0.load(Ordering::Acquire) {
            return Err("model catalog refresh cancelled".to_owned());
        }
        if state.generation != generation {
            return Err("model catalog source changed during refresh".to_owned());
        }
        if seq <= state.published_seq {
            return Ok(Vec::new());
        }
        let changed = (self.publish)(&data)?;
        state.published_seq = seq;
        drop(state);
        self.notifier.notify(changed.clone());
        Ok(changed)
    }
}
