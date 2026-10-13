// Ported from CLIProxyAPI internal/runtime/executor/gemini_vertex_executor.go
// (native Interactions routing, URLs, Execute and ExecuteStream) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Vertex's opt-in native Interactions endpoint. Regional service accounts
//! retain the Gemini bridge; native service-account calls use global.

use super::*;
use crate::gemini::interactions::{
    apply_api_revision, prepare_request, translate_answer, translate_stream,
};

pub(super) fn should_execute_native(auth: &Auth, options: &Options) -> bool {
    if options.source_format != Format::INTERACTIONS {
        return false;
    }
    let enabled = auth
        .attribute("interactions")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true") || value.trim() == "1")
        || ["interactions", "native_interactions"]
            .iter()
            .any(|key| auth.metadata.get(*key).and_then(Value::as_bool) == Some(true));
    if !enabled {
        return false;
    }
    // Only service accounts have a regional endpoint. Missing locations
    // use the existing regional default too.
    if auth.attribute("api_key").unwrap_or_default().is_empty()
        && auth
            .metadata_str("access_token")
            .unwrap_or_default()
            .is_empty()
    {
        return auth
            .metadata_str("location")
            .is_some_and(|location| location.trim() == "global");
    }
    true
}

impl VertexExecutor {
    fn interactions_url(&self, auth: &Auth, target: &Target<'_>, stream: bool) -> String {
        let custom = auth.attribute("base_url").unwrap_or_default().trim();
        let base = if !custom.is_empty() {
            custom
        } else if matches!(target, Target::ServiceAccount { .. }) {
            self.service_account_base_url
                .as_deref()
                .unwrap_or(GLOBAL_BASE_URL)
        } else {
            GLOBAL_BASE_URL
        }
        .trim_end_matches('/');
        let project = match target {
            Target::ApiKey { .. } => auth.metadata_str("project_id").unwrap_or_default().trim(),
            Target::ServiceAccount { project, .. } => project.trim(),
        };
        let path = if project.is_empty() {
            "interactions".to_owned()
        } else {
            format!("projects/{project}/locations/global/interactions")
        };
        format!(
            "{base}/v1beta1/{path}{}",
            if stream { "?alt=sse" } else { "" }
        )
    }

    async fn send_interactions(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
        stream: bool,
    ) -> Result<(reqwest::Response, crate::redact::Secrets, Value), ExecError> {
        let target = target(auth)?;
        let body = prepare_request(
            self.config.as_deref(),
            self.models(),
            PROVIDER,
            request,
            options,
            stream,
        )?;
        let mut headers = self.headers(auth, options, &target).await?;
        apply_api_revision(&mut headers, &options.headers);
        let kind = if stream {
            AttemptKind::Stream
        } else {
            AttemptKind::Execute
        };
        let (response, secrets) = post(
            &self.clients,
            &self.interactions_url(auth, &target, stream),
            headers,
            &body,
            NAME,
            Attempt::new(
                options,
                kind,
                PROVIDER,
                base_model(&request.model),
                &Format::INTERACTIONS,
                auth,
            ),
        )
        .await?;
        Ok((response, secrets, body))
    }

    pub(super) async fn execute_interactions(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let (response, secrets, body) = self
            .send_interactions(auth, request, options, false)
            .await?;
        let (headers, data) = read_answer(response, &secrets).await?;
        translate_answer(request, options, &body, headers, data)
    }

    pub(super) async fn execute_interactions_stream(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<StreamResponse, ExecError> {
        let (response, secrets, body) =
            self.send_interactions(auth, request, options, true).await?;
        Ok(translate_stream(
            response, request, options, &body, secrets, NAME,
        ))
    }
}
