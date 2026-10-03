// Ported from CLIProxyAPI getRequestDetailsWithOptions and
// validateImageOnlyModel in sdk/api/handlers/handlers_routing.go,
// responsesWebsocketResolvedModelName in
// sdk/api/handlers/openai/openai_responses_websocket_session.go,
// GetProviderName and ResolveAutoModel in internal/util/provider.go, and
// ParseSuffix in internal/thinking/suffix.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which providers serve the model a client asks for.

use open_ferry_core::exec::ProviderId;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go;
use serde_json::json;

use crate::errors::ErrorMessage;

/// Models only the image endpoints serve, which upstream has and this port
/// doesn't yet.
const IMAGE_ONLY_MODELS: [&str; 8] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
    "grok-imagine-image",
    "grok-imagine-image-quality",
    "grok-imagine-image-2.0",
];

/// Where a call goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    /// The providers that serve the model, in order of preference.
    pub(crate) providers: Vec<ProviderId>,
    /// The model, with `auto` resolved and any thinking suffix kept.
    pub(crate) model: String,
}

/// `model` with `auto` resolved to the first model available and any
/// thinking suffix kept (`ResolveAutoModel`, as
/// `getRequestDetailsWithOptions` and `responsesWebsocketResolvedModelName`
/// call it).
pub(crate) fn resolve_model(catalog: &dyn ModelCatalog, model: &str) -> String {
    let (base, suffix) = parse_suffix(model);
    if base != "auto" {
        return model.to_owned();
    }
    let first = catalog
        .first_available_model()
        .unwrap_or_else(|| "auto".to_owned());
    match suffix {
        Some(raw) => format!("{first}({raw})"),
        None => first,
    }
}

/// Routes `model`: resolves `auto`, turns away image-only models, and finds
/// the providers that serve it.
pub(crate) fn route(catalog: &dyn ModelCatalog, model: &str) -> Result<Route, ErrorMessage> {
    let resolved = resolve_model(catalog, model);
    let base_model = parse_suffix(&resolved).0.trim();
    check_image_only(base_model)?;

    let mut providers = provider_names(catalog, base_model);
    if providers.is_empty() && base_model != resolved {
        providers = provider_names(catalog, &resolved);
    }
    if providers.is_empty() {
        let body = json!({"error": {
            "message": format!("unknown provider for model {model}"),
            "type": "invalid_request_error",
            "code": "model_not_found",
            "param": "model",
        }});
        return Err(ErrorMessage::new(400, body.to_string()));
    }
    Ok(Route {
        providers,
        model: resolved,
    })
}

/// A model name and its thinking suffix, as in `gpt-5(high)` (upstream's
/// `ParseSuffix`).
pub(crate) fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => {
            (&model[..open], Some(&model[open + 1..model.len() - 1]))
        }
        _ => (model, None),
    }
}

/// Turns away models only the image endpoints serve.
fn check_image_only(model: &str) -> Result<(), ErrorMessage> {
    let mut base = parse_suffix(model).0.trim();
    if base.is_empty() {
        base = model.trim();
    }
    let name = route_model_base_name(base);
    if IMAGE_ONLY_MODELS.contains(&go::to_lower(name.trim()).as_str()) {
        return Err(ErrorMessage::new(
            503,
            format!(
                "model {name} is only supported on /v1/images/generations and /v1/images/edits"
            ),
        ));
    }
    Ok(())
}

/// What follows the last `/` in a model name, unless the `/` ends it.
fn route_model_base_name(model: &str) -> &str {
    let model = model.trim();
    match model.rfind('/') {
        Some(slash) if slash < model.len() - 1 => model[slash + 1..].trim(),
        _ => model,
    }
}

/// The providers that serve `model`, trying it in lower case when it has
/// none as named (upstream's `GetProviderName`).
fn provider_names(catalog: &dyn ModelCatalog, model: &str) -> Vec<ProviderId> {
    if model.is_empty() {
        return Vec::new();
    }
    let mut providers = distinct(catalog.model_providers(model));
    let lower = go::to_lower(model);
    if providers.is_empty() && lower != model {
        providers = distinct(catalog.model_providers(&lower));
    }
    providers
}

/// `names` without empty or repeated ones.
fn distinct(names: Vec<ProviderId>) -> Vec<ProviderId> {
    let mut kept: Vec<ProviderId> = Vec::new();
    for name in names {
        if !name.is_empty() && !kept.contains(&name) {
            kept.push(name);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeCatalog;

    #[test]
    fn parses_suffixes_as_upstream_does() {
        assert_eq!(parse_suffix("gpt-5(high)"), ("gpt-5", Some("high")));
        assert_eq!(parse_suffix("a(b)(c)"), ("a(b)", Some("c")));
        assert_eq!(parse_suffix("()"), ("", Some("")));
        assert_eq!(parse_suffix("a(b"), ("a(b", None));
        assert_eq!(parse_suffix("a)"), ("a)", None));
        assert_eq!(parse_suffix("a)("), ("a)(", None));
    }

    #[test]
    fn routes_models() {
        let catalog = FakeCatalog::new()
            .serve("gpt-5", &["codex", "codex", "", "openai"])
            .serve("lower-only", &["claude"])
            .serve("custom(8192)", &["openai-compat"])
            .first("gpt-5");
        let route = |model| route(&catalog, model);

        assert_eq!(
            route("gpt-5(high)").unwrap(),
            Route {
                providers: vec!["codex".into(), "openai".into()],
                model: "gpt-5(high)".into(),
            }
        );
        assert_eq!(route("auto").unwrap().model, "gpt-5");
        assert_eq!(route("auto(low)").unwrap().model, "gpt-5(low)");
        assert_eq!(route("LOWER-only").unwrap().providers, ["claude"]);
        assert_eq!(route("custom(8192)").unwrap().providers, ["openai-compat"]);

        let unknown = route("nope \"x\"").unwrap_err();
        assert_eq!(unknown.status, 400);
        assert_eq!(
            unknown.text,
            r#"{"error":{"message":"unknown provider for model nope \"x\"","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
        );
        assert_eq!(route("").unwrap_err().status, 400);

        let image = route("openai/GPT-Image-2(high)").unwrap_err();
        assert_eq!(image.status, 503);
        assert_eq!(
            image.text,
            "model GPT-Image-2 is only supported on /v1/images/generations and /v1/images/edits"
        );
    }

    #[test]
    fn auto_stays_when_nothing_is_available() {
        let catalog = FakeCatalog::new().serve("auto", &["x"]);
        assert_eq!(route(&catalog, "auto").unwrap().model, "auto");
    }
}
