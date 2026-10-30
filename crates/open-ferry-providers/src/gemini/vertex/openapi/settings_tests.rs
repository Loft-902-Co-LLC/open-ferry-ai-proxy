// Ported from CLIProxyAPI vertex_openapi_settings_test.go and
// gemini_vertex_executor_openapi_settings_test.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use crate::gemini::testing::{
    Mock, OK_ANSWER, Reply, collect, key_auth, options, request, service_account_auth,
};
use bytes::Bytes;
use open_ferry_core::config::{
    OAuthModelAlias, OAuthModelSetting, VertexCompatKey, VertexCompatModel,
};

#[test]
fn model_sources_override_credentials_and_do_not_leak_to_siblings() {
    for api_key in [false, true] {
        for source in [
            "models",
            "model_aliases",
            "model-aliases",
            "vertex_model_openapi",
            "oauth-attribute",
            "global-alias",
            "global-setting",
            "api-key-config",
        ] {
            if (source == "api-key-config" && !api_key)
                || (api_key
                    && ["oauth-attribute", "global-alias", "global-setting"].contains(&source))
            {
                continue;
            }
            for enabled in [false, true] {
                for alias in [false, true] {
                    if !alias && ["oauth-attribute", "global-alias"].contains(&source) {
                        continue;
                    }
                    let name = if enabled {
                        "custom-gemini"
                    } else {
                        "partner/custom-model"
                    };
                    let alias = if alias { "public-model" } else { "" };
                    let rule = json!({"name":name,"alias":alias,"openapi":enabled});
                    let mut auth = Auth {
                        provider: "vertex".into(),
                        ..Auth::default()
                    };
                    auth.attributes
                        .insert("openapi".into(), (!enabled).to_string());
                    let mut cfg = Config::default();
                    if api_key {
                        auth.attributes.insert("api_key".into(), "dummy".into());
                        auth.attributes
                            .insert("base_url".into(), "https://relay.test".into());
                        cfg.vertex_api_key.push(VertexCompatKey {
                            api_key: "dummy".into(),
                            base_url: "https://relay.test".into(),
                            openapi: Some(!enabled),
                            ..VertexCompatKey::default()
                        });
                    } else {
                        auth.metadata.insert(
                            "service_account".into(),
                            json!({"client_email":"dummy@example.test"}),
                        );
                    }
                    match source {
                        "models" | "model_aliases" | "model-aliases" => {
                            auth.metadata
                                .insert(source.into(), json!(["unrelated", rule]));
                        }
                        "vertex_model_openapi" => {
                            auth.attributes
                                .insert(source.into(), json!([rule]).to_string());
                        }
                        "oauth-attribute" => {
                            auth.attributes
                                .insert("model_aliases".into(), json!([rule]).to_string());
                        }
                        "global-alias" => {
                            cfg.oauth_model_alias.insert(
                                "vertex".into(),
                                vec![OAuthModelAlias {
                                    name: name.into(),
                                    alias: alias.into(),
                                    openapi: Some(enabled),
                                    ..OAuthModelAlias::default()
                                }],
                            );
                        }
                        "global-setting" => {
                            cfg.oauth_settings.insert(
                                "vertex".into(),
                                vec![OAuthModelSetting {
                                    name: name.into(),
                                    alias: alias.into(),
                                    openapi: Some(enabled),
                                    ..OAuthModelSetting::default()
                                }],
                            );
                        }
                        "api-key-config" => {
                            cfg.vertex_api_key[0].models.push(VertexCompatModel {
                                name: name.into(),
                                alias: alias.into(),
                                openapi: Some(enabled),
                                ..VertexCompatModel::default()
                            });
                            auth.metadata.insert(
                                "models".into(),
                                json!([{"name":name,"alias":alias,"openapi":!enabled}]),
                            );
                        }
                        _ => unreachable!(),
                    }
                    let mut opts = options(&Format::OPENAI);
                    opts.metadata.requested_model = if alias.is_empty() {
                        name.into()
                    } else {
                        "PUBLIC-MODEL(high)".into()
                    };
                    assert_eq!(
                        should_execute_request(Some(&cfg), &auth, &format!("{name}(high)"), &opts),
                        enabled,
                        "{api_key}/{source}/{enabled}/{alias}"
                    );
                    if !alias.is_empty() {
                        opts.metadata.requested_model = "sibling-alias".into();
                        assert_eq!(
                            should_execute_request(Some(&cfg), &auth, name, &opts),
                            !enabled,
                            "sibling {source}"
                        );
                    }
                    opts.metadata.requested_model = name.into();
                    assert_eq!(
                        should_execute_request(Some(&cfg), &auth, name, &opts),
                        enabled,
                        "direct {source}"
                    );
                }
            }
        }
    }
}

#[test]
fn prefixes_exact_suffixes_shadowing_and_oauth_isolation() {
    let mut auth = Auth {
        provider: "vertex".into(),
        prefix: "tenant".into(),
        ..Auth::default()
    };
    auth.metadata.insert(
        "models".into(),
        json!([
            {"name":"gemini-2.5-pro","openapi":true},
            {"name":"gemini-2.5-pro","alias":"public","openapi":false},
            {"name":"gemini-2.5-pro(high)","alias":"public(high)","openapi":true}
        ]),
    );
    let mut opts = options(&Format::OPENAI);
    for (requested, upstream, expected) in [
        ("TENANT/public(high)", "gemini-2.5-pro(high)", true),
        ("tenant/public(low)", "gemini-2.5-pro(low)", false),
        ("tenant/public", "gemini-2.5-pro", false),
    ] {
        opts.metadata.requested_model = requested.into();
        assert_eq!(
            should_execute_request(None, &auth, upstream, &opts),
            expected
        );
    }
    opts.metadata.requested_model.clear();
    opts.original_request = Bytes::from_static(br#"{"model":"tenant/public"}"#);
    assert!(!should_execute_request(
        None,
        &auth,
        "gemini-2.5-pro",
        &opts
    ));
    opts.metadata.requested_model = "tenant/public(high)".into();
    assert!(should_execute_request(
        None,
        &auth,
        "gemini-2.5-pro(high)",
        &opts
    ));
    // A per-account alias with no boolean shadows a global override.
    auth.metadata.clear();
    auth.attributes.insert(
        "model_aliases".into(),
        r#"[{"name":"partner/model","alias":"public"}]"#.into(),
    );
    let mut cfg = Config::default();
    cfg.oauth_model_alias.insert(
        "vertex".into(),
        vec![OAuthModelAlias {
            name: "partner/model".into(),
            alias: "public".into(),
            openapi: Some(false),
            ..OAuthModelAlias::default()
        }],
    );
    opts.metadata.requested_model = "tenant/public".into();
    assert!(should_execute_request(
        Some(&cfg),
        &auth,
        "partner/model",
        &opts
    ));
    cfg.oauth_settings.insert(
        "vertex".into(),
        vec![OAuthModelSetting {
            name: "gemini-2.5-pro".into(),
            openapi: Some(true),
            ..OAuthModelSetting::default()
        }],
    );
    auth.attributes.insert("api_key".into(), "dummy".into());
    assert!(!should_execute_request(
        Some(&cfg),
        &auth,
        "gemini-2.5-pro",
        &opts
    ));
    auth.prefix = "partner".into();
    opts.metadata.requested_model = "partner/short".into();
    assert!(should_execute_request(None, &auth, "partner/model", &opts));
    auth.metadata.insert(
        "model_aliases".into(),
        json!([{"name":"partner/model","alias":"public","openapi":true}]),
    );
    auth.metadata.insert(
        "models".into(),
        json!([{"name":"gemini-2.5-pro","alias":"public","openapi":false}]),
    );
    opts.metadata.requested_model = "public".into();
    assert!(should_execute_request(None, &auth, "partner/model", &opts));
    for enabled in [false, true] {
        auth.attributes
            .insert("openapi".into(), enabled.to_string());
        auth.metadata.insert(
            "models".into(),
            json!([{"name":"partner/imagen-4(high)","openapi":true}]),
        );
        assert!(!should_execute_request(
            Some(&cfg),
            &auth,
            "partner/imagen-4(high)",
            &opts
        ));
    }
}

#[test]
fn config_resolution_checks_index_then_prefix_and_proxy() {
    let mut cfg = Config::default();
    for (prefix, proxy) in [("one", "http://proxy-one"), ("two", "http://proxy-two")] {
        cfg.vertex_api_key.push(VertexCompatKey {
            api_key: "dummy".into(),
            base_url: "https://relay.test".into(),
            prefix: prefix.into(),
            proxy_url: proxy.into(),
            ..VertexCompatKey::default()
        });
    }
    let mut auth = (*key_auth("vertex", "dummy", "https://relay.test")).clone();
    auth.prefix = "two".into();
    auth.proxy_url = "http://proxy-two".into();
    assert_eq!(
        settings::resolve_config(Some(&cfg), &auth).unwrap().prefix,
        "two"
    );
    auth.attributes.insert("config_index".into(), "0".into());
    assert_eq!(
        settings::resolve_config(Some(&cfg), &auth).unwrap().prefix,
        "one"
    );
    cfg.vertex_api_key[0].api_key = "different".into();
    assert_eq!(
        settings::resolve_config(Some(&cfg), &auth).unwrap().prefix,
        "two"
    );
    auth.attributes
        .insert("base_url".into(), "https://stale.test".into());
    assert_eq!(
        settings::resolve_config(Some(&cfg), &auth).unwrap().prefix,
        "two"
    );
}

#[test]
fn location_rewrites_hosts_resources_and_keeps_full_urls_without_overrides() {
    for base in [
        "",
        "https://us-central1-aiplatform.googleapis.com",
        "https://aiplatform.us.rep.googleapis.com",
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/p/locations/eu/endpoints/openapi",
    ] {
        for (location, host) in [
            ("us", "aiplatform.us.rep.googleapis.com"),
            ("eu", "aiplatform.eu.rep.googleapis.com"),
            ("GLOBAL", "aiplatform.googleapis.com"),
            ("us-central1", "us-central1-aiplatform.googleapis.com"),
        ] {
            assert_eq!(
                base_url(base, "p", location),
                format!("https://{host}/v1/projects/p/locations/{location}/endpoints/openapi"),
                "{base}/{location}"
            );
        }
    }
    assert_eq!(
        base_url("https://relay.test", "p", "eu"),
        "https://relay.test/v1/projects/p/locations/eu/endpoints/openapi"
    );
    for tail in ["", "/publishers/google/models", "/endpoints/openapi"] {
        let base = format!(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1{tail}"
        );
        assert_eq!(
            base_url(&base, "ignored", "global"),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global/endpoints/openapi"
        );
        assert_eq!(
            base_url(&base, "ignored", "europe-west4"),
            "https://europe-west4-aiplatform.googleapis.com/v1/projects/p/locations/europe-west4/endpoints/openapi"
        );
        assert_eq!(
            base_url(&base, "ignored", ""),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/endpoints/openapi"
        );
    }
    assert_eq!(
        base_url(
            "https://us-central1-aiplatform.googleapis.com",
            "p",
            "global"
        ),
        "https://aiplatform.googleapis.com/v1/projects/p/locations/global/endpoints/openapi"
    );
    assert_eq!(
        base_url(
            "https://relay.test/v1/projects/p/locations/old",
            "p",
            "global"
        ),
        "https://relay.test/v1/projects/p/locations/global/endpoints/openapi"
    );
    let mut cfg = Config::default();
    cfg.vertex_api_key.push(VertexCompatKey {
        api_key: "dummy".into(),
        openapi_location: " global ".into(),
        ..VertexCompatKey::default()
    });
    let mut auth = (*key_auth("vertex", "dummy", "")).clone();
    assert_eq!(explicit_location(Some(&cfg), &auth), "global");
    auth.metadata
        .insert("openapi-location".into(), " europe-west4 ".into());
    assert_eq!(explicit_location(Some(&cfg), &auth), "europe-west4");
    auth.attributes
        .insert("openapi_location".into(), " us-east5 ".into());
    assert_eq!(explicit_location(Some(&cfg), &auth), "us-east5");
}

#[tokio::test]
async fn every_request_method_obeys_automatic_credential_and_model_choices() {
    for service_account in [false, true] {
        for method in ["execute", "stream", "count"] {
            for (model, credential, per_model, enabled) in [
                ("partner/custom", None, None, true),
                ("custom", None, None, false),
                ("custom", Some(true), None, true),
                ("partner/custom", Some(false), None, false),
                ("custom", Some(false), Some(true), true),
                ("partner/custom", Some(true), Some(false), false),
                ("imagen-4", Some(true), Some(true), false),
            ] {
                let mock = Mock::answering(move |seen| {
                    if seen.path.ends_with(":countTokens") { return Reply::json(r#"{"totalTokens":3}"#); }
                    let openapi = seen.path.ends_with("/chat/completions");
                    if method == "stream" {
                        if openapi { Reply::sse("data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n") }
                        else { Reply::sse(&format!("data: {OK_ANSWER}\n\ndata: [DONE]\n\n")) }
                    } else if openapi { Reply::json(super::tests::CHAT) }
                    else if seen.path.ends_with(":predict") { Reply::json(r#"{"predictions":[]}"#) }
                    else { Reply::json(OK_ANSWER) }
                }).await;
                let token = Mock::start(Reply::json(
                    r#"{"access_token":"dummy-sa-token","expires_in":3600}"#,
                ))
                .await;
                let mut cfg = Config::default();
                cfg.proxy_url = "direct".into();
                let mut auth = if service_account {
                    service_account_auth(&token.url, "us-central1")
                } else {
                    (*key_auth("vertex", "dummy", &mock.url)).clone()
                };
                if service_account {
                    if let Some(value) = credential {
                        auth.metadata.insert("openapi".into(), value.into());
                    }
                    if let Some(value) = per_model {
                        auth.metadata.insert(
                            "models".into(),
                            json!(["unrelated",{"name":model,"alias":"public","openapi":value}]),
                        );
                    }
                } else {
                    cfg.vertex_api_key.push(VertexCompatKey {
                        api_key: "dummy".into(),
                        base_url: mock.url.clone(),
                        openapi: credential,
                        models: vec![VertexCompatModel {
                            name: model.into(),
                            alias: "public".into(),
                            openapi: per_model,
                            ..VertexCompatModel::default()
                        }],
                        ..VertexCompatKey::default()
                    });
                }
                let executor = VertexExecutor::new("direct")
                    .with_config(Arc::new(cfg))
                    .with_service_account_base_url(&mock.url);
                let mut opts = options(&Format::OPENAI);
                opts.original_request = Bytes::from_static(
                    br#"{"model":"public","messages":[{"role":"user","content":"hello"}]}"#,
                );
                let req = request(model, super::tests::PAYLOAD);
                match method {
                    "execute" => {
                        executor.execute(Arc::new(auth), req, opts).await.unwrap();
                    }
                    "stream" => {
                        opts.stream = true;
                        let (_, error) = collect(
                            executor
                                .execute_stream(Arc::new(auth), req, opts)
                                .await
                                .unwrap(),
                        )
                        .await;
                        assert!(error.is_none(), "{error:?}");
                    }
                    "count" => {
                        executor
                            .count_tokens(Arc::new(auth), req, opts)
                            .await
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
                if enabled && method == "count" {
                    assert_eq!(mock.hits(), 0);
                    assert_eq!(token.hits(), 0);
                    continue;
                }
                assert_eq!(mock.hits(), 1, "{service_account}/{method}/{model}");
                let seen = mock.last();
                assert_eq!(
                    seen.path.ends_with("/endpoints/openapi/chat/completions"),
                    enabled,
                    "{service_account}/{method}/{model}: {}",
                    seen.path
                );
                if enabled {
                    assert_eq!(
                        seen.json()["model"],
                        if model.contains('/') {
                            model.to_string()
                        } else {
                            format!("google/{model}")
                        }
                    );
                } else {
                    assert!(
                        seen.path
                            .contains(&format!("/publishers/google/models/{model}:"))
                    );
                }
                if service_account {
                    assert_eq!(seen.header("authorization"), Some("Bearer dummy-sa-token"));
                } else if enabled {
                    assert_eq!(seen.header("authorization"), Some("Bearer dummy"));
                } else {
                    assert_eq!(seen.header("x-goog-api-key"), Some("dummy"));
                }
            }
        }
    }
}

#[tokio::test]
async fn full_resource_location_is_preserved_or_rewritten_on_execute_and_stream() {
    for sa in [false, true] {
        for explicit in [false, true] {
            for stream in [false, true] {
                let mock=Mock::start(if stream { Reply::sse("data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n") } else { Reply::json(super::tests::CHAT) }).await;
                let token = Mock::start(Reply::json(
                    r#"{"access_token":"dummy-sa-token","expires_in":3600}"#,
                ))
                .await;
                let base = format!(
                    "{}/v1/projects/p/locations/global/publishers/google/models",
                    mock.url
                );
                let mut auth = if sa {
                    service_account_auth(&token.url, "us-central1")
                } else {
                    (*key_auth("vertex", "dummy", &base)).clone()
                };
                let mut config = Config::default();
                config.proxy_url = "direct".into();
                if explicit && !sa {
                    config.vertex_api_key.push(VertexCompatKey {
                        api_key: "dummy".into(),
                        base_url: base.clone(),
                        openapi_location: "europe-west4".into(),
                        ..VertexCompatKey::default()
                    });
                }
                if sa {
                    auth.attributes.insert("base_url".into(), base);
                }
                if explicit && sa {
                    auth.attributes
                        .insert("openapi_location".into(), "europe-west4".into());
                }
                let executor = VertexExecutor::new("direct").with_config(Arc::new(config));
                let req = request("partner/model", super::tests::PAYLOAD);
                let mut opts = options(&Format::OPENAI);
                opts.stream = stream;
                if stream {
                    let (_, error) = collect(
                        executor
                            .execute_stream(Arc::new(auth), req, opts)
                            .await
                            .unwrap(),
                    )
                    .await;
                    assert!(error.is_none());
                } else {
                    executor.execute(Arc::new(auth), req, opts).await.unwrap();
                }
                let location = if explicit { "europe-west4" } else { "global" };
                assert_eq!(
                    mock.last().path,
                    format!(
                        "/v1/projects/p/locations/{location}/endpoints/openapi/chat/completions"
                    )
                );
            }
        }
    }
}

#[test]
fn bool_shorthand_falls_through_to_the_next_source() {
    for (raw, model, automatic) in [(" t ", "custom", false), (" F ", "partner/model", true)] {
        for attribute in [false, true] {
            let mut auth = Auth::default();
            if attribute {
                auth.attributes.insert("openapi".into(), raw.into());
            } else {
                auth.metadata.insert("openapi".into(), json!(raw));
            }
            assert_eq!(explicit_setting(&auth), None);
            assert_eq!(should_execute(&auth, model), automatic, "{raw}/{attribute}");
            auth.attributes.insert("api_key".into(), "dummy".into());
            let mut cfg = Config::default();
            cfg.vertex_api_key.push(VertexCompatKey {
                api_key: "dummy".into(),
                openapi: Some(automatic),
                ..Default::default()
            });
            assert_eq!(
                should_execute_request(Some(&cfg), &auth, model, &options(&Format::OPENAI)),
                automatic
            );
            if attribute {
                auth.metadata.insert("openapi".into(), json!(automatic));
                cfg.vertex_api_key[0].openapi = Some(!automatic);
                assert_eq!(
                    should_execute_request(Some(&cfg), &auth, model, &options(&Format::OPENAI)),
                    automatic
                );
            }
        }
    }
    for (raw, expected) in [
        (" TRUE ", true),
        ("1", true),
        (" False ", false),
        ("0", false),
    ] {
        let mut auth = Auth::default();
        auth.metadata.insert("openapi".into(), json!(raw));
        assert_eq!(explicit_setting(&auth), Some(expected));
    }
}

#[test]
fn single_object_metadata_rules_route_like_one_element_arrays() {
    for source in ["models", "model_aliases", "model-aliases"] {
        for enabled in [false, true] {
            let model = if enabled { "custom" } else { "partner/model" };
            let mut auth = Auth::default();
            auth.attributes
                .insert("openapi".into(), (!enabled).to_string());
            let rule = json!({"name": model, "alias": "public", "openapi": enabled});
            let mut opts = options(&Format::OPENAI);
            opts.metadata.requested_model = "public".into();
            for value in [rule.clone(), json!([rule])] {
                auth.metadata.insert(source.into(), value);
                assert_eq!(
                    should_execute_request(None, &auth, model, &opts),
                    enabled,
                    "{source}/{enabled}"
                );
            }
        }
    }
}

#[test]
fn location_rewrite_keeps_host_only_openapi_paths_single_slashed() {
    for suffix in ["/endpoints/openapi", "/publishers/meta/models/custom"] {
        for (location, host) in [
            ("europe-west4", "europe-west4-aiplatform.googleapis.com"),
            ("global", "aiplatform.googleapis.com"),
            ("us", "aiplatform.us.rep.googleapis.com"),
            ("eu", "aiplatform.eu.rep.googleapis.com"),
        ] {
            assert_eq!(
                base_url(
                    &format!("https://us-central1-aiplatform.googleapis.com{suffix}"),
                    "p",
                    location
                ),
                format!("https://{host}/endpoints/openapi")
            );
        }
    }
}
