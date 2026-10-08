// Ported from CLIProxyAPI getRequestDetailsWithOptions,
// validateImageOnlyModel, isOpenAIImageOnlyModel, validateSpeechOnlyModel,
// isXAISpeechOnlyModel and routeModelBaseName in
// sdk/api/handlers/handlers_routing.go,
// responsesWebsocketResolvedModelName in
// sdk/api/handlers/openai/openai_responses_websocket_session.go,
// GetProviderName and ResolveAutoModel in internal/util/provider.go, and
// ParseSuffix in internal/thinking/suffix.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which providers serve the model a client asks for.

use open_ferry_core::exec::ProviderId;
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::go;
use serde_json::json;

use crate::errors::ErrorMessage;

/// Models only the image endpoints serve (`/v1/images/generations` and
/// `/v1/images/edits`).
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

/// Models only the speech endpoints serve (`/v1/audio/speech` and
/// `/v1/tts`).
const SPEECH_ONLY_MODELS: [&str; 2] = ["grok-tts", "grok-voice-tts-1.0"];

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

/// Routes `model`: resolves `auto`, turns away image-only models unless
/// `allow_image_model` (as only the image endpoints have it) and
/// speech-only models unless `allow_speech_model` (as only the speech
/// endpoints have it), and finds the providers that serve it.
pub(crate) fn route(
    catalog: &dyn ModelCatalog,
    model: &str,
    allow_image_model: bool,
    allow_speech_model: bool,
) -> Result<Route, ErrorMessage> {
    let resolved = resolve_model(catalog, model);
    let base_model = parse_suffix(&resolved).0.trim();
    validate_image_only(base_model, allow_image_model)?;
    validate_speech_only(base_model, allow_speech_model)?;

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
pub(crate) fn check_image_only(model: &str) -> Result<(), ErrorMessage> {
    validate_image_only(model, false)
}

/// Turns away models only the image endpoints serve, unless
/// `allow_image_model` (upstream's `validateImageOnlyModel`).
pub(crate) fn validate_image_only(
    model: &str,
    allow_image_model: bool,
) -> Result<(), ErrorMessage> {
    let mut base = parse_suffix(model).0.trim();
    if base.is_empty() {
        base = model.trim();
    }
    if is_image_only_model(base) && !allow_image_model {
        return Err(ErrorMessage::new(
            503,
            format!(
                "model {} is only supported on /v1/images/generations and /v1/images/edits",
                route_model_base_name(base)
            ),
        ));
    }
    Ok(())
}

/// Whether only the image endpoints serve `model`, which may name a provider
/// before a `/` (upstream's `isOpenAIImageOnlyModel`).
pub(crate) fn is_image_only_model(model: &str) -> bool {
    let name = route_model_base_name(model);
    IMAGE_ONLY_MODELS.contains(&go::to_lower(name.trim()).as_str())
}

/// Turns away models only the speech endpoints serve.
pub(crate) fn check_speech_only(model: &str) -> Result<(), ErrorMessage> {
    validate_speech_only(model, false)
}

/// Turns away models only the speech endpoints serve with a 400, unless
/// `allow_speech_model` (upstream's `validateSpeechOnlyModel`).
pub(crate) fn validate_speech_only(
    model: &str,
    allow_speech_model: bool,
) -> Result<(), ErrorMessage> {
    let mut base = parse_suffix(model).0.trim();
    if base.is_empty() {
        base = model.trim();
    }
    if is_speech_only_model(base) && !allow_speech_model {
        return Err(ErrorMessage::new(
            400,
            format!(
                "model {} is only supported on /v1/audio/speech and /v1/tts",
                route_model_base_name(base)
            ),
        ));
    }
    Ok(())
}

/// Whether only the speech endpoints serve `model`, which may name a
/// provider before a `/` (upstream's `isXAISpeechOnlyModel`).
pub(crate) fn is_speech_only_model(model: &str) -> bool {
    let name = route_model_base_name(model);
    SPEECH_ONLY_MODELS.contains(&go::to_lower(name.trim()).as_str())
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
        let route = |model| route(&catalog, model, false, false);

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
        assert_eq!(route(&catalog, "auto", false, false).unwrap().model, "auto");
    }

    /// The image-only models, with and without a provider, as upstream's
    /// image-only tests list them.
    const IMAGE_ONLY: [&str; 15] = [
        "gpt-image-1.5",
        "gpt-image-2",
        "codex/gpt-image-2",
        "gpt-image-2.5-flare",
        "codex/gpt-image-2.5-flare",
        "gpt-image-2.5-sunburst",
        "codex/gpt-image-2.5-sunburst",
        "gpt-image-2.5",
        "codex/gpt-image-2.5",
        "grok-imagine-image",
        "xai/grok-imagine-image",
        "grok-imagine-image-quality",
        "xai/grok-imagine-image-quality",
        "grok-imagine-image-2.0",
        "xai/grok-imagine-image-2.0",
    ];

    // Ports TestGetRequestDetails_ImageModelReturns503.
    #[test]
    fn image_only_models_get_a_503() {
        let catalog = FakeCatalog::new();
        for model in IMAGE_ONLY {
            let err = route(&catalog, model, false, false).unwrap_err();
            assert_eq!(err.status, 503, "{model}");
            assert!(err.text.contains("/v1/images/generations"), "{model}");
            assert!(err.text.contains("/v1/images/edits"), "{model}");
        }
    }

    // Ports TestValidateImageOnlyModel_AllowsImageEndpoints.
    #[test]
    fn the_image_endpoints_allow_image_only_models() {
        for model in IMAGE_ONLY {
            assert!(validate_image_only(model, true).is_ok(), "{model}");
            assert_eq!(validate_image_only(model, false).unwrap_err().status, 503);
        }
        // Not upstream's: with the models allowed, routing goes on to the
        // providers.
        let catalog = FakeCatalog::new().serve("gpt-image-2", &["codex"]);
        assert_eq!(
            route(&catalog, "gpt-image-2", true, false)
                .unwrap()
                .providers,
            ["codex"]
        );
    }

    // Ports TestIsOpenAIImageOnlyModel.
    #[test]
    fn knows_the_image_only_models() {
        let cases = [
            ("gpt-image-1.5", true),
            ("gpt-image-2", true),
            ("codex/gpt-image-1.5", true),
            ("gpt-image-2.5-flare", true),
            ("codex/gpt-image-2.5-flare", true),
            ("gpt-image-2.5-sunburst", true),
            ("codex/gpt-image-2.5-sunburst", true),
            ("gpt-image-2.5", true),
            ("codex/gpt-image-2.5", true),
            ("grok-imagine-image", true),
            ("xai/grok-imagine-image", true),
            ("XAI/Grok-Imagine-Image-Quality", true),
            ("grok-imagine-image-quality", true),
            ("grok-imagine-image-2.0", true),
            ("xai/grok-imagine-image-2.0", true),
            ("grok-3", false),
            ("gpt-5.2", false),
            ("grok-imagine-video", false),
        ];
        for (model, want) in cases {
            assert_eq!(is_image_only_model(model), want, "{model}");
        }
    }

    /// Upstream's `speechOnlyModels` (handlers_speech_only_test.go).
    const SPEECH_ONLY: [&str; 6] = [
        "grok-tts",
        "xai/grok-tts",
        "XAI/Grok-TTS",
        "grok-tts(auto)",
        "grok-voice-tts-1.0",
        "xai/grok-voice-tts-1.0",
    ];

    // Ports TestGetRequestDetails_SpeechOnlyModelReturns400 and the
    // forced-provider case of
    // TestHandlerProvidersForExecutionRejectsSpeechOnlyModelOnProviderRoute.
    #[test]
    fn speech_only_models_get_a_400() {
        let mut catalog = FakeCatalog::new();
        for model in SPEECH_ONLY {
            catalog = catalog.serve(model, &["xai"]);
        }
        for model in SPEECH_ONLY {
            let err = route(&catalog, model, false, false).unwrap_err();
            assert_eq!(err.status, 400, "{model}");
            assert!(err.text.contains("/v1/audio/speech"), "{model}");
            assert!(err.text.contains("/v1/tts"), "{model}");
            assert_eq!(check_speech_only(model).unwrap_err().status, 400);
            // Not upstream's: with the models allowed, routing goes on to
            // the providers.
            assert!(validate_speech_only(model, true).is_ok(), "{model}");
            assert_eq!(
                route(&catalog, model, false, true).unwrap().providers,
                ["xai"],
                "{model}"
            );
        }
        // Not upstream's: the message names the model as the client wrote
        // it, without its provider.
        assert_eq!(
            check_speech_only("XAI/Grok-TTS").unwrap_err().text,
            "model Grok-TTS is only supported on /v1/audio/speech and /v1/tts"
        );
    }

    // Ports TestSpeechOnlyModelsKeepImageAndVideoBehavior: the speech-only
    // models aren't image-only, and the video models aren't speech-only.
    #[test]
    fn speech_only_models_keep_image_and_video_behavior() {
        for model in SPEECH_ONLY {
            assert!(!is_image_only_model(model), "{model}");
            // As upstream's, the check reads the name without its suffix.
            assert!(is_speech_only_model(parse_suffix(model).0), "{model}");
        }
        assert!(validate_image_only("grok-imagine-video", false).is_ok());
        for model in ["grok-imagine-video", "grok-3", "gpt-image-2", "tts-1"] {
            assert!(!is_speech_only_model(model), "{model}");
        }
    }
}
