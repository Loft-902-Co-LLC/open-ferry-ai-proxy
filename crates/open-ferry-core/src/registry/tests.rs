// Ported from CLIProxyAPI internal/registry/model_registry_safety_test.go,
// model_registry_cache_test.go, model_registry_grok_test.go,
// model_registry_credential_quota_regression_test.go and
// model_registry_quota_refresh_regression_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests for the model registry.
//!
//! Upstream's tests that check returned values are copies keep their names,
//! though a Rust caller can't change the registry through them anyway. Tests
//! that set a quota mark in the past instead ask at a later time. Dropped:
//! the `LookupModelInfo` tests (not ported; the Claude Sonnet 5 one checks
//! the static catalog instead), the `ApplyClientModelCapabilities`, probed
//! capability and web search tests (not ported), the hook tests (no hooks),
//! and the cache expiry check (no cache).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::*;
use crate::models::ThinkingSupport;

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        ..ModelInfo::default()
    }
}

fn ids(maps: &[Map<String, Value>]) -> Vec<&str> {
    maps.iter()
        .filter_map(|map| map.get("id").and_then(Value::as_str))
        .collect()
}

#[test]
fn get_model_info_returns_clone() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "gemini",
        &[ModelInfo {
            display_name: "Model One".into(),
            thinking: Some(ThinkingSupport {
                min: 1,
                max: 2,
                levels: vec!["low".into(), "high".into()],
                ..ThinkingSupport::default()
            }),
            ..model("m1")
        }],
    );

    let mut first = registry.model_info("m1", "gemini").unwrap();
    first.display_name = "mutated".into();
    first.thinking.as_mut().unwrap().levels[0] = "mutated".into();

    let second = registry.model_info("m1", "gemini").unwrap();
    assert_eq!(second.display_name, "Model One");
    assert_eq!(second.thinking.unwrap().levels[0], "low");
}

#[test]
fn get_models_for_client_returns_clones() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "gemini",
        &[ModelInfo {
            display_name: "Model One".into(),
            thinking: Some(ThinkingSupport {
                levels: vec!["low".into(), "high".into()],
                ..ThinkingSupport::default()
            }),
            ..model("m1")
        }],
    );

    let mut first = registry.models_for_client("client-1");
    assert_eq!(first.len(), 1);
    first[0].display_name = "mutated".into();

    let second = registry.models_for_client("client-1");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].display_name, "Model One");
    assert_eq!(second[0].thinking.as_ref().unwrap().levels[0], "low");
}

#[test]
fn get_available_models_by_provider_returns_clones() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "gemini",
        &[ModelInfo {
            display_name: "Model One".into(),
            thinking: Some(ThinkingSupport {
                levels: vec!["low".into(), "high".into()],
                ..ThinkingSupport::default()
            }),
            ..model("m1")
        }],
    );

    let mut first = registry.available_models_by_provider("gemini");
    assert_eq!(first.len(), 1);
    first[0].display_name = "mutated".into();

    let second = registry.available_models_by_provider("gemini");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].display_name, "Model One");
    assert_eq!(second[0].thinking.as_ref().unwrap().levels[0], "low");
}

#[test]
fn cleanup_expired_quotas_keeps_the_model_listed() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "openai",
        &[ModelInfo {
            created: 1,
            ..model("m1")
        }],
    );
    registry.set_model_quota_exceeded("client-1", "m1");
    assert_eq!(registry.available_model_maps("openai").len(), 1);
    assert_eq!(registry.model_count("m1"), 0);

    let generation = registry.generation();
    registry.cleanup_expired_quotas_at(Instant::now() + Duration::from_secs(6 * 60));
    assert_ne!(registry.generation(), generation);
    assert!(!registry.is_model_quota_exceeded_for_client("client-1", "m1"));

    assert_eq!(registry.model_count("m1"), 1);
    let models = registry.available_model_maps("openai");
    assert_eq!(ids(&models), ["m1"]);
}

#[test]
fn get_available_models_returns_cloned_supported_parameters() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "openai",
        &[ModelInfo {
            display_name: "Model One".into(),
            supported_parameters: vec!["temperature".into(), "top_p".into()],
            ..model("m1")
        }],
    );

    let mut first = registry.available_model_maps("openai");
    assert_eq!(first.len(), 1);
    first[0].insert(
        "supported_parameters".into(),
        serde_json::json!(["mutated"]),
    );

    let second = registry.available_model_maps("openai");
    assert_eq!(
        second[0]["supported_parameters"],
        serde_json::json!(["temperature", "top_p"])
    );
}

#[test]
fn get_available_models_includes_max_context_length_override() {
    let registry = ModelRegistry::new();
    const WANT: u64 = 1_048_576;
    registry.register_client(
        "client-1",
        "openai",
        &[ModelInfo {
            context_length: WANT,
            max_context_length: WANT,
            ..model("deepseek-v4-flash")
        }],
    );

    let models = registry.available_model_maps("openai");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["context_length"], Value::from(WANT));
    assert_eq!(models[0]["max_context_length"], Value::from(WANT));
}

#[test]
fn static_catalog_includes_claude_sonnet_5() {
    let model = StaticCatalog::embedded()
        .claude_models()
        .into_iter()
        .find(|model| model.id == "claude-sonnet-5")
        .expect("Claude Sonnet 5 static model");
    assert_eq!(model.model_type, "claude");
    assert_eq!(model.context_length, 1_000_000);
    assert_eq!(model.max_completion_tokens, 128_000);
    let thinking = model.thinking.unwrap();
    assert!(thinking.zero_allowed && thinking.dynamic_allowed);
    assert_eq!((thinking.min, thinking.max), (0, 0));
    assert_eq!(thinking.levels, ["low", "medium", "high", "xhigh", "max"]);
}

#[test]
fn get_available_models_returns_cloned_snapshots() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "OpenAI",
        &[ModelInfo {
            owned_by: "team-a".into(),
            display_name: "Model One".into(),
            ..model("m1")
        }],
    );

    let mut first = registry.available_model_maps("openai");
    assert_eq!(first.len(), 1);
    first[0].insert("id".into(), "mutated".into());
    first[0].insert("display_name".into(), "Mutated".into());

    let second = registry.available_model_maps("openai");
    assert_eq!(second[0]["id"], "m1");
    assert_eq!(second[0]["display_name"], "Model One");
    assert_eq!(registry.providers_for_model("m1"), ["openai"]);
}

#[test]
fn get_available_models_claude_includes_token_limits() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "client-1",
        "Claude",
        &[
            ModelInfo {
                owned_by: "anthropic".into(),
                model_type: "claude".into(),
                created: 1_771_372_800,
                context_length: 200_000,
                max_completion_tokens: 64_000,
                ..model("claude-sonnet-4-6")
            },
            ModelInfo {
                owned_by: "anthropic".into(),
                model_type: "claude".into(),
                ..model("claude-no-limits")
            },
        ],
    );

    let models = registry.available_model_maps("claude");
    let by_id: HashMap<&str, &Map<String, Value>> = models
        .iter()
        .map(|model| (model["id"].as_str().unwrap(), model))
        .collect();

    let with_limits = by_id["claude-sonnet-4-6"];
    assert_eq!(with_limits["max_input_tokens"], 200_000);
    assert_eq!(with_limits["max_tokens"], 64_000);
    assert_eq!(with_limits["created_at"], "2026-02-18T00:00:00Z");

    let with_defaults = by_id["claude-no-limits"];
    assert_eq!(
        with_defaults["max_input_tokens"],
        DEFAULT_CLAUDE_MAX_INPUT_TOKENS
    );
    assert_eq!(
        with_defaults["max_tokens"],
        DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS
    );
    assert_eq!(with_defaults["display_name"], "claude-no-limits");
    assert_eq!(with_defaults["type"], "model");
    assert!(!with_defaults.contains_key("created_at"));
}

#[test]
fn get_available_models_follows_registry_changes() {
    let registry = ModelRegistry::new();
    let register = |display_name: &str| {
        registry.register_client(
            "client-1",
            "OpenAI",
            &[ModelInfo {
                owned_by: "team-a".into(),
                display_name: display_name.into(),
                ..model("m1")
            }],
        );
    };
    register("Model One");
    let models = registry.available_model_maps("openai");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["display_name"], "Model One");

    register("Model One Updated");
    let models = registry.available_model_maps("openai");
    assert_eq!(models[0]["display_name"], "Model One Updated");

    registry.suspend_client_model("client-1", "m1", "manual");
    assert!(registry.available_model_maps("openai").is_empty());

    registry.resume_client_model("client-1", "m1");
    assert_eq!(registry.available_model_maps("openai").len(), 1);
}

#[test]
fn get_available_model_infos_preserves_metadata_and_availability() {
    let registry = ModelRegistry::new();
    registry.register_client(
        "openai-client",
        "openai",
        &[ModelInfo {
            display_name: "Z Model".into(),
            context_length: 1000,
            ..model("z-model")
        }],
    );
    registry.register_client(
        "claude-client",
        "claude",
        &[ModelInfo {
            display_name: "A Model".into(),
            context_length: 2000,
            thinking: Some(ThinkingSupport {
                levels: vec!["low".into(), "high".into()],
                ..ThinkingSupport::default()
            }),
            ..model("a-model")
        }],
    );
    registry.register_client("xai-client", "xai", &[model("x-model")]);
    registry.register_client("suspended-client", "xai", &[model("hidden-model")]);
    registry.suspend_client_model("suspended-client", "hidden-model", "manual");

    let mut models = registry.available_model_infos();
    let order: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(order, ["a-model", "x-model", "z-model"]);
    assert_eq!(models[0].thinking.as_ref().unwrap().levels, ["low", "high"]);

    models[0].thinking.as_mut().unwrap().levels[0] = "mutated".into();
    let fresh = registry.available_model_infos();
    assert_eq!(fresh[0].thinking.as_ref().unwrap().levels[0], "low");
}

#[test]
fn get_available_model_infos_honors_quota_and_suspension_availability() {
    struct Case {
        name: &'static str,
        clients: usize,
        quota_exceeded: bool,
        quota_suspended: bool,
        manual_suspended: bool,
        want_available: bool,
    }
    let cases = [
        Case {
            name: "quota cooldown remains listed",
            clients: 1,
            quota_exceeded: true,
            quota_suspended: false,
            manual_suspended: false,
            want_available: true,
        },
        Case {
            name: "quota suspension reason remains listed",
            clients: 1,
            quota_exceeded: false,
            quota_suspended: true,
            manual_suspended: false,
            want_available: true,
        },
        Case {
            name: "quota and non-quota suspensions are hidden",
            clients: 2,
            quota_exceeded: true,
            quota_suspended: true,
            manual_suspended: true,
            want_available: false,
        },
    ];
    const MODEL: &str = "shared-model";
    for case in cases {
        let registry = ModelRegistry::new();
        registry.register_client("quota-client", "openai", &[model(MODEL)]);
        if case.clients > 1 {
            registry.register_client("manual-client", "openai", &[model(MODEL)]);
        }
        if case.quota_exceeded {
            registry.set_model_quota_exceeded("quota-client", MODEL);
        }
        if case.quota_suspended {
            registry.suspend_client_model("quota-client", MODEL, "quota");
        }
        if case.manual_suspended {
            registry.suspend_client_model("manual-client", MODEL, "manual");
        }

        let infos = registry.available_model_infos();
        let info_available = infos.len() == 1 && infos[0].id == MODEL;
        assert_eq!(info_available, case.want_available, "{}", case.name);

        let models = registry.available_model_maps("openai");
        assert_eq!(
            ids(&models) == [MODEL],
            case.want_available,
            "{}",
            case.name
        );
    }
}

#[test]
fn available_models_keep_healthy_provider_during_credential_quota() {
    let registry = ModelRegistry::new();
    let luna = ModelInfo {
        owned_by: "openai".into(),
        model_type: "openai".into(),
        ..model("gpt-5.6-luna")
    };
    registry.register_client("healthy-api-provider", "codex", std::slice::from_ref(&luna));
    registry.register_client(
        "exhausted-oauth-account",
        "codex",
        std::slice::from_ref(&luna),
    );

    let (_, epoch) = registry.models_and_epoch_for_client("exhausted-oauth-account");
    assert!(registry.apply_client_model_projections(
        "exhausted-oauth-account",
        epoch,
        1,
        &[ClientModelProjection {
            model_id: luna.id.clone(),
            suspended: true,
            suspend_reason: "credential_quota".into(),
            quota_exceeded: true,
        }],
    ));
    assert_eq!(registry.available_model_maps("openai").len(), 1);
    assert_eq!(registry.available_models_by_provider("codex").len(), 1);
    assert_eq!(registry.available_model_infos().len(), 1);
    assert_eq!(registry.model_count(&luna.id), 1);

    let (_, epoch) = registry.models_and_epoch_for_client("healthy-api-provider");
    assert!(registry.apply_client_model_projections(
        "healthy-api-provider",
        epoch,
        1,
        &[ClientModelProjection {
            model_id: luna.id.clone(),
            suspended: true,
            suspend_reason: "manual".into(),
            quota_exceeded: false,
        }],
    ));
    assert!(registry.available_model_maps("openai").is_empty());
    assert!(registry.available_models_by_provider("codex").is_empty());
    assert_eq!(registry.model_count(&luna.id), 0);
}

#[test]
fn credential_quota_keeps_healthy_catalog_across_refresh() {
    let base = Instant::now();
    let registration = ModelRegistration {
        count: 2,
        quota_exceeded_clients: HashMap::from([("oauth".to_owned(), base)]),
        suspended_clients: HashMap::from([("oauth".to_owned(), "credential_quota".to_owned())]),
        ..ModelRegistration::default()
    };
    for offset in [
        Duration::ZERO,
        QUOTA_WINDOW - Duration::from_nanos(1),
        QUOTA_WINDOW,
        Duration::from_secs(6 * 60),
    ] {
        assert!(registration.available(base + offset), "at {offset:?}");
    }

    // Re-registering clears the old quota mark; projecting the cooldown again
    // starts a new window.
    let registry = ModelRegistry::new();
    let models = [ModelInfo {
        owned_by: "openai".into(),
        ..model("audit-luna")
    }];
    registry.register_client("healthy", "codex", &models);
    registry.register_client("oauth", "codex", &models);
    let projection = [ClientModelProjection {
        model_id: "audit-luna".into(),
        suspended: true,
        suspend_reason: "credential_quota".into(),
        quota_exceeded: true,
    }];
    let (_, epoch) = registry.models_and_epoch_for_client("oauth");
    assert!(registry.apply_client_model_projections("oauth", epoch, 1, &projection));
    assert_eq!(registry.available_model_maps("openai").len(), 1);

    let later = Instant::now() + Duration::from_secs(6 * 60);
    assert_eq!(registry.available_model_maps_at("openai", later).len(), 1);

    registry.register_client("oauth", "codex", &models);
    assert!(!registry.is_model_quota_exceeded_for_client("oauth", "audit-luna"));
    assert!(!registry.is_model_suspended_for_client("oauth", "audit-luna"));
    let (_, epoch) = registry.models_and_epoch_for_client("oauth");
    assert!(registry.apply_client_model_projections("oauth", epoch, 2, &projection));
    assert_eq!(registry.available_model_maps("openai").len(), 1);
}

#[test]
fn re_registering_reconciles_counts() {
    let registry = ModelRegistry::new();
    registry.register_client("a", "codex", &[model("m1"), model("m1"), model("m2")]);
    assert_eq!(registry.model_count("m1"), 2);
    assert_eq!(registry.model_count("m2"), 1);
    assert_eq!(registry.models_for_client("a").len(), 2);

    registry.register_client("a", "codex", &[model("m1"), model("m3")]);
    assert_eq!(registry.model_count("m1"), 1);
    assert_eq!(registry.model_count("m2"), 0);
    assert_eq!(registry.model_count("m3"), 1);
    assert_eq!(registry.providers_for_model("m1"), ["codex"]);
    let listed: Vec<String> = registry
        .available_model_infos()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(listed, ["m1", "m3"]);

    // No models unregisters the client.
    registry.register_client("a", "codex", &[model("")]);
    assert!(registry.available_model_infos().is_empty());
    assert!(registry.models_for_client("a").is_empty());
}

#[test]
fn re_registering_under_another_provider_moves_the_models() {
    let registry = ModelRegistry::new();
    registry.register_client("a", "codex", &[model("m1")]);
    registry.register_client("b", "codex", &[model("m1")]);
    assert_eq!(registry.providers_for_model("m1"), ["codex"]);

    let renamed = ModelInfo {
        display_name: "From Claude".into(),
        ..model("m1")
    };
    registry.register_client("a", "claude", std::slice::from_ref(&renamed));
    assert_eq!(registry.model_count("m1"), 2);
    assert_eq!(registry.providers_for_model("m1"), ["claude", "codex"]);
    assert_eq!(registry.model_info("m1", "claude"), Some(renamed));
    assert_eq!(registry.available_models_by_provider("codex").len(), 1);

    registry.unregister_client("b");
    assert_eq!(registry.providers_for_model("m1"), ["claude"]);
    assert!(registry.available_models_by_provider("codex").is_empty());
}

#[test]
fn providers_come_most_registrations_first() {
    let registry = ModelRegistry::new();
    registry.register_client("c1", "claude", &[model("m1")]);
    registry.register_client("c2", "codex", &[model("m1")]);
    registry.register_client("c3", "codex", &[model("m1")]);
    assert_eq!(registry.providers_for_model("m1"), ["codex", "claude"]);
    assert_eq!(
        ModelCatalog::model_providers(&registry, "m1"),
        ["codex", "claude"]
    );
    assert!(registry.providers_for_model("missing").is_empty());
}

#[test]
fn first_available_model_is_the_newest_with_a_free_client() {
    let registry = ModelRegistry::new();
    assert_eq!(
        registry.first_available_model_for("openai"),
        Err(FirstModelError::NoModels("openai".into()))
    );
    registry.register_client(
        "a",
        "codex",
        &[
            ModelInfo {
                created: 100,
                ..model("old")
            },
            ModelInfo {
                created: 200,
                ..model("new")
            },
            model("undated"),
        ],
    );
    assert_eq!(registry.first_available_model_for("openai").unwrap(), "new");
    assert_eq!(
        ModelCatalog::first_available_model(&registry).unwrap(),
        "new"
    );

    registry.suspend_client_model("a", "new", "manual");
    assert_eq!(registry.first_available_model_for("").unwrap(), "old");

    registry.set_model_quota_exceeded("a", "old");
    registry.set_model_quota_exceeded("a", "undated");
    let err = registry.first_available_model_for("openai").unwrap_err();
    assert_eq!(err, FirstModelError::NoAvailableClients("openai".into()));
    assert_eq!(
        err.to_string(),
        "no available clients for any model in handler type: openai"
    );
    assert_eq!(
        FirstModelError::NoModels("claude".into()).to_string(),
        "no models available for handler type: claude"
    );
}

#[test]
fn unregistering_removes_the_client() {
    let registry = ModelRegistry::new();
    registry.register_client("a", "codex", &[model("m1")]);
    registry.register_client("b", "codex", &[model("m1")]);
    let epoch = registry.client_registration_epoch("a");
    let registrations = registry.registration_epoch();

    registry.unregister_client("a");
    assert_eq!(registry.model_count("m1"), 1);
    assert!(!registry.client_supports_model("a", "m1"));
    assert!(registry.models_for_client("a").is_empty());
    assert_ne!(registry.client_registration_epoch("a"), epoch);
    assert_ne!(registry.registration_epoch(), registrations);

    registry.unregister_client("b");
    assert!(ModelCatalog::available_models(&registry).is_empty());
    assert_eq!(registry.model_info("m1", ""), None);
}

#[test]
fn projections_need_the_current_registration() {
    let registry = ModelRegistry::new();
    let quota = |id: &str| ClientModelProjection {
        model_id: id.into(),
        quota_exceeded: true,
        ..ClientModelProjection::default()
    };
    assert!(!registry.apply_client_model_projections("a", 0, 0, &[quota("m1")]));

    registry.register_client("a", "codex", &[model("m1")]);
    let (_, epoch) = registry.models_and_epoch_for_client("a");
    assert!(!registry.apply_client_model_projections(" ", epoch, 1, &[quota("m1")]));
    assert!(!registry.apply_client_model_projections("a", epoch + 1, 1, &[quota("m1")]));
    assert!(!registry.apply_client_model_projections("a", epoch, 1, &[quota("other")]));
    assert!(!registry.apply_client_model_projections("a", epoch, 1, &[]));

    let generation = registry.generation();
    assert!(registry.apply_client_model_projections(" a ", epoch, 2, &[quota(" m1 ")]));
    assert!(registry.is_model_quota_exceeded_for_client("a", "m1"));
    assert_ne!(registry.generation(), generation);

    // An older generation is stale; the same one applies again.
    assert!(!registry.apply_client_model_projections("a", epoch, 1, &[model_cleared("m1")]));
    assert!(registry.apply_client_model_projections("a", epoch, 2, &[model_cleared("m1")]));
    assert!(!registry.is_model_quota_exceeded_for_client("a", "m1"));

    // Projecting the same state again changes nothing.
    let generation = registry.generation();
    assert!(registry.apply_client_model_projections("a", epoch, 3, &[model_cleared("m1")]));
    assert_eq!(registry.generation(), generation);
}

fn model_cleared(id: &str) -> ClientModelProjection {
    ClientModelProjection {
        model_id: id.into(),
        ..ClientModelProjection::default()
    }
}

#[test]
fn suspensions_keep_their_first_reason() {
    let registry = ModelRegistry::new();
    registry.register_client("a", "codex", &[model("m1")]);
    registry.suspend_client_model("a", "m1", "quota");
    registry.suspend_client_model("a", "m1", "manual");
    // Still a quota cooldown, so still listed.
    assert_eq!(registry.available_model_infos().len(), 1);
    assert_eq!(registry.model_count("m1"), 0);
    assert!(registry.is_model_suspended_for_client(" a ", " m1 "));

    registry.suspend_client_model("a", "missing", "manual");
    registry.resume_client_model("a", "m1");
    assert_eq!(registry.model_count("m1"), 1);
}

#[test]
fn client_supports_model_ignores_case_and_whitespace() {
    let registry = ModelRegistry::new();
    registry.register_client("a", "codex", &[model(" GPT-5 ")]);
    assert!(registry.client_supports_model(" a ", "gpt-5"));
    assert!(!registry.client_supports_model("a", "gpt-6"));
    assert!(!registry.client_supports_model("a", " "));
    assert!(!registry.client_supports_model("b", "gpt-5"));
    // Go's strings.EqualFold keeps the dotless i apart from I.
    registry.register_client("t", "claude", &[model("m\u{131}")]);
    assert!(!registry.client_supports_model("t", "mI"));
    assert!(registry.client_supports_model("t", "M\u{131}"));
}

#[test]
fn model_maps_follow_the_handler_type() {
    let info = ModelInfo {
        owned_by: "google".into(),
        model_type: "gemini".into(),
        created: 5,
        display_name: "Gemini".into(),
        version: "001".into(),
        supported_input_modalities: vec!["TEXT".into()],
        ..model("gemini-x")
    };
    let gemini = model_to_map(&info, "gemini");
    assert_eq!(
        Value::Object(gemini),
        serde_json::json!({
            "name": "gemini-x",
            "version": "001",
            "displayName": "Gemini",
            "supportedInputModalities": ["TEXT"],
        })
    );
    let generic = model_to_map(&info, "");
    assert_eq!(
        Value::Object(generic),
        serde_json::json!({
            "id": "gemini-x",
            "object": "model",
            "owned_by": "google",
            "type": "gemini",
            "created": 5,
        })
    );
    // Keys come sorted, as Go writes a map's.
    let openai = model_to_map(&info, "openai");
    let keys: Vec<&String> = openai.keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn rfc3339_matches_go() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_771_372_861), "2026-02-18T00:01:01Z");
    assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    // Go writes years before 1 with a minus sign and four digits.
    assert_eq!(rfc3339(-62_167_219_200), "0000-01-01T00:00:00Z");
    assert_eq!(rfc3339(-62_167_219_200 - 86_400), "-0001-12-31T00:00:00Z");
}

#[test]
fn equal_fold_follows_simple_case_folding() {
    assert!(equal_fold("Quota", "QUOTA"));
    assert!(!equal_fold("quota", "quotas"));
    let kelvin = char::from_u32(0x212A).unwrap().to_string();
    assert!(equal_fold(&kelvin, "k"));
    assert!(equal_fold("", ""));
    // Neither Turkish i is an ASCII one.
    assert!(!equal_fold("m\u{131}", "mI"));
    assert!(!equal_fold("m\u{130}", "mi"));
}
