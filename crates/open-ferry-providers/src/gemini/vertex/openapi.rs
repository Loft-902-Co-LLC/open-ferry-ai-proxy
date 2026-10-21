// Ported from CLIProxyAPI internal/runtime/executor/helps/vertex_openapi.go
// and gemini_vertex_executor.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Vertex partner/open models use Chat Completions with the existing token cache,
//! service-account validation, redaction and honest client identity.

use super::*;
use crate::openai_compat::OpenAiCompatExecutor;
use open_ferry_translate::go::{equal_fold, to_lower};

fn publisher_resource(model: &str) -> Option<(&str, &str)> {
    if !to_lower(model).starts_with("publishers/") {
        return None;
    }
    let (publisher, tail) = model[11..].trim().split_once('/')?;
    let tail = tail.trim();
    if publisher.trim().is_empty() || !to_lower(tail).starts_with("models/") {
        return None;
    }
    let id = tail[7..].trim();
    (!id.is_empty()).then_some((publisher.trim(), id))
}

fn is_partner(model: &str) -> bool {
    let model = base_model(model).trim();
    if let Some((publisher, id)) = publisher_resource(model) {
        return !equal_fold(publisher, "google") && !id.contains('/');
    }
    let Some((publisher, id)) = model.split_once('/') else {
        return false;
    };
    let publisher = publisher.trim();
    let id = id.trim();
    !publisher.is_empty()
        && !id.is_empty()
        && !id.contains('/')
        && !["google", "models", "publishers"]
            .iter()
            .any(|p| equal_fold(publisher, p))
}

pub(super) fn should_execute(auth: &Auth, model: &str) -> bool {
    let parse = |raw: &str| match to_lower(raw.trim()).as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    };
    let explicit = auth.attribute("openapi").and_then(parse).or_else(|| {
        auth.metadata
            .get("openapi")
            .and_then(|v| v.as_bool().or_else(|| v.as_str().and_then(parse)))
    });
    explicit.map_or_else(
        || is_partner(model),
        |on| on && !is_imagen(base_model(model)),
    )
}

fn normalize_model(model: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return String::new();
    }
    let parsed_base = base_model(model);
    let base = match parsed_base.trim() {
        "" => model,
        base => base,
    };
    let normalized = if let Some((publisher, id)) = publisher_resource(base) {
        format!("{}/{id}", to_lower(publisher))
    } else if to_lower(base).starts_with("models/") && !base[7..].trim().is_empty() {
        format!("google/{}", base[7..].trim())
    } else if let Some((publisher, id)) = base.split_once('/')
        && !publisher.trim().is_empty()
        && !id.trim().is_empty()
    {
        format!("{}/{}", publisher.trim(), id.trim())
    } else {
        format!("google/{base}")
    };
    let suffix = &model[parsed_base.len()..];
    format!("{normalized}{suffix}")
}

pub(super) fn normalize_request(request: &Request, options: &Options) -> (Request, Options) {
    let mut request = request.clone();
    let mut options = options.clone();
    let model = normalize_model(&request.model);
    if !model.is_empty() && model != request.model {
        if options.original_request.is_empty() {
            options.original_request = request.payload.clone();
        }
        request.model = model;
    }
    (request, options)
}

fn location(auth: &Auth, default: &str) -> String {
    for key in ["openapi_location", "openapi-location", "location"] {
        if (key != "location" || default.trim().is_empty())
            && let Some(value) = auth.attribute(key).map(str::trim).filter(|v| !v.is_empty())
        {
            return value.to_owned();
        }
    }
    for key in ["openapi_location", "openapi-location", "location"] {
        if (key != "location" || default.trim().is_empty())
            && let Some(value) = auth
                .metadata_str(key)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        {
            return value.to_owned();
        }
    }
    if default.trim().is_empty() {
        DEFAULT_LOCATION.to_owned()
    } else {
        default.trim().to_owned()
    }
}

fn auth_base_url(auth: &Auth) -> &str {
    auth.attribute("base_url")
        .into_iter()
        .chain(
            ["base_url", "base-url"]
                .into_iter()
                .filter_map(|k| auth.metadata_str(k)),
        )
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

fn google_host(base: &str) -> bool {
    if base.trim().is_empty() {
        return true;
    }
    match url::Url::parse(base.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        Some(host) => {
            host == "aiplatform.googleapis.com"
                || host.ends_with("-aiplatform.googleapis.com")
                || host.ends_with(".aiplatform.googleapis.com")
        }
        None => to_lower(base).contains("aiplatform.googleapis.com"),
    }
}

fn has_version(base: &str) -> bool {
    if base.contains("/v1/") || base.contains("/v1beta1/") {
        return true;
    }
    let segment = base.rsplit('/').next().unwrap_or_default();
    segment.starts_with('v')
        && segment.bytes().skip(1).any(|b| b.is_ascii_digit())
        && segment
            .bytes()
            .skip(1)
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'_' || b == b'-')
}

fn base_url(base: &str, project: &str, location: &str) -> String {
    let mut base = base.trim().trim_end_matches('/').to_owned();
    if base.is_empty() {
        base = if equal_fold(location, "global") {
            GLOBAL_BASE_URL.to_owned()
        } else {
            vertex_base_url(location)
        };
    }
    if base.ends_with("/endpoints/openapi") {
        return base;
    }
    if let Some(index) = base.find("/publishers/") {
        return format!("{}/endpoints/openapi", &base[..index]);
    }
    if base.contains("/projects/") && base.contains("/locations/") {
        return format!("{base}/endpoints/openapi");
    }
    if !has_version(&base) {
        base.push_str("/v1");
    }
    if !project.trim().is_empty() {
        base.push_str(&format!(
            "/projects/{}/locations/{location}",
            project.trim()
        ));
    }
    format!("{base}/endpoints/openapi")
}

impl VertexExecutor {
    pub(super) fn openapi_compat(&self) -> OpenAiCompatExecutor {
        self.openapi
            .get_or_init(|| {
                let config = self.config.clone().unwrap_or_else(|| {
                    let mut config = Config::default();
                    config.proxy_url = self.clients.effective_proxy("").to_owned();
                    Arc::new(config)
                });
                let executor = OpenAiCompatExecutor::new(PROVIDER, config);
                match &self.models {
                    Some(models) => executor.with_models(models.clone()),
                    None => executor,
                }
            })
            .clone()
    }

    pub(super) async fn openapi_executor(
        &self,
        auth: &Auth,
    ) -> Result<OpenAiCompatExecutor, ExecError> {
        let has_sa = auth
            .metadata
            .get("service_account")
            .and_then(Value::as_object)
            .is_some_and(|sa| !sa.is_empty());
        let key = auth
            .attribute("api_key")
            .filter(|v| !v.is_empty())
            .or_else(|| auth.metadata_str("access_token"))
            .unwrap_or_default();
        let (base, token) = if key.is_empty() || has_sa {
            let Target::ServiceAccount {
                project,
                location: default,
                account,
            } = service_account_target(&auth.metadata)
                .map_err(|m| ExecError::new(ErrorKind::Upstream, m))?
            else {
                unreachable!()
            };
            let location = location(auth, &default);
            let token = self
                .tokens
                .token(&self.clients.get(&auth.proxy_url), &account)
                .await
                .map_err(|_| {
                    tracing::error!(
                        error_kind = "token_fetch",
                        "{NAME}: access token request failed"
                    );
                    ExecError::from(StatusError::new(500, "internal server error"))
                })?;
            if token.trim().is_empty() {
                return Err(StatusError::new(401, "missing access token").into());
            }
            let base = self
                .service_account_base_url
                .as_deref()
                .unwrap_or_else(|| auth_base_url(auth));
            (base_url(base, &project, &location), token)
        } else {
            // Static keys follow vertexAPICreds: only configuration attributes
            // may override the destination, never credential-file metadata.
            let base = auth.attribute("base_url").unwrap_or_default().trim();
            if google_host(base) {
                return Err(StatusError::new(400, "vertex endpoints/openapi on aiplatform.googleapis.com requires service account credentials").into());
            }
            let project = auth
                .metadata_str("project_id")
                .filter(|v| !v.trim().is_empty())
                .or_else(|| auth.metadata_str("project"))
                .unwrap_or_default();
            (base_url(base, project, &location(auth, "")), key.to_owned())
        };
        Ok(self.openapi_compat().with_vertex_credentials(base, token))
    }
}

#[cfg(test)]
mod tests;
