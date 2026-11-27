//! Explicitly approved local profiles: Chat and embeddings on every profile,
//! rerank on the compatible/vLLM/SGLang profiles, System One on the compatible
//! and Ollama profiles (see `workloads`). Ollama uses compatible Chat and
//! native /api/embed with truncation disabled.
use super::{ProviderAdapter, embeddings, framing, openai, secrets::SecretResolver};
use crate::inference::{error::InferenceError, types::*};
use async_trait::async_trait;
use std::sync::Arc;
pub mod endpoints;
mod workloads;
use endpoints::ApprovedEndpoints;
use workloads::RerankWire;
type Result<T> = std::result::Result<T, InferenceError>;
#[derive(Clone, Copy)]
pub enum Profile {
    OpenAiCompatible,
    Vllm,
    Sglang,
    Ollama,
}
impl Profile {
    pub(crate) const ALL: [Self; 4] = [
        Self::OpenAiCompatible,
        Self::Vllm,
        Self::Sglang,
        Self::Ollama,
    ];
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai_compatible",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::Ollama => "ollama",
        }
    }
    /// Servers of this profile accept vLLM's `priority` request field, so a
    /// route's batch scheduling may set a priority hint
    /// (`capabilities::accepts_priority_hint`) and its batch lines carry it.
    pub(crate) fn accepts_priority_hint(self) -> bool {
        matches!(self, Self::Vllm | Self::OpenAiCompatible)
    }
}
pub struct LocalAdapter {
    profile: Profile,
    resolver: Arc<dyn SecretResolver>,
    approvals: Arc<ApprovedEndpoints>,
}
impl LocalAdapter {
    pub fn new(
        profile: Profile,
        resolver: Arc<dyn SecretResolver>,
        approvals: Arc<ApprovedEndpoints>,
    ) -> Self {
        Self {
            profile,
            resolver,
            approvals,
        }
    }
    fn connection(&self, target: &Deployment) -> Result<(&crate::egress::HttpClient, &str)> {
        if target.provider != self.id() || target.region.as_deref().is_some_and(|s| !s.is_empty()) {
            return Err(InferenceError::Configuration);
        }
        let endpoint = target
            .endpoint
            .as_deref()
            .ok_or(InferenceError::Configuration)?;
        self.approvals.validate_connection(
            endpoint,
            &target.credential_ref,
            target.region.as_deref(),
        )?;
        self.approvals.approved(endpoint)
    }
    /// The approved base with its fixed `/v1` suffix removed, keeping any
    /// reverse-proxy prefix (native, non-`/v1` server routes).
    fn prefix(base: &str) -> Result<&str> {
        base.strip_suffix("/v1")
            .ok_or(InferenceError::Configuration)
    }
    /// The OpenAI video contract on this connection (`openai_compatible`
    /// only, `capabilities`): its approved base, and its credential as a
    /// bearer token when it has one (an environment reference). The
    /// credential is resolved before anything is sent.
    fn video_wire(&self, target: &Deployment) -> Result<openai::jobs::VideoWire<'_>> {
        if !matches!(self.profile, Profile::OpenAiCompatible) {
            return Err(InferenceError::Unsupported);
        }
        let (client, base) = self.connection(target)?;
        let auth = match target.credential_ref.as_str() {
            "none" => None,
            reference if reference.starts_with("env:") => {
                let secret = self.resolver.resolve(reference)?;
                let mut header =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
                        .map_err(|_| InferenceError::Configuration)?;
                header.set_sensitive(true);
                Some(header)
            }
            _ => return Err(InferenceError::Configuration),
        };
        Ok(openai::jobs::VideoWire {
            client,
            base,
            auth,
            kind: crate::egress::UpstreamClient::Local,
        })
    }
    /// Rerank wire of this profile; Ollama serves no rerank API.
    fn rerank_wire(&self) -> Option<RerankWire> {
        match self.profile {
            Profile::OpenAiCompatible | Profile::Vllm => Some(RerankWire::Jina),
            Profile::Sglang => Some(RerankWire::Sglang),
            Profile::Ollama => None,
        }
    }
    /// TypeSafe System One: generic compatible servers (OpenJev and the like)
    /// and Ollama v0.35.0+. vLLM and SGLang serve no such route.
    fn serves_systemone(&self) -> bool {
        matches!(self.profile, Profile::OpenAiCompatible | Profile::Ollama)
    }
    async fn post_json(
        &self,
        target: &Deployment,
        url: String,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (client, _) = self.connection(target)?;
        let response = self
            .authenticated_post(client, url, target)?
            .json(payload)
            .send()
            .await
            .map_err(framing::send_error(crate::egress::UpstreamClient::Local))?;
        framing::status(response.status())?;
        framing::body(response).await
    }
    /// A POST to `url` with the connection's credential. The credential is
    /// resolved before the request is built, so a reference that cannot be
    /// resolved fails with nothing sent (`inference::dispatch`).
    fn authenticated_post(
        &self,
        client: &crate::egress::HttpClient,
        url: impl reqwest::IntoUrl,
        target: &Deployment,
    ) -> Result<reqwest::RequestBuilder> {
        if target.credential_ref == "none" {
            return Ok(client.post(url));
        }
        // Only environment references may cross an approved local connection.
        if !target.credential_ref.starts_with("env:") {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut header =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
                .map_err(|_| InferenceError::Configuration)?;
        header.set_sensitive(true);
        Ok(client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, header))
    }
    fn encode_chat(&self, target: &Deployment, request: &ChatRequest) -> Result<serde_json::Value> {
        super::messages::validate(request, super::messages::ProviderClass::Local)?;
        if !self.supports_chat_request(request) {
            return Err(InferenceError::Unsupported);
        }
        let mut value = openai::encode(&target.upstream_model, request);
        // Older compatible servers use max_tokens. Every local profile pins that dialect.
        if let Some(tokens) = value
            .as_object_mut()
            .unwrap()
            .remove("max_completion_tokens")
        {
            value["max_tokens"] = tokens;
        }
        if let Some(priority) = self.batch_priority(target) {
            value["priority"] = priority.into();
        }
        Ok(value)
    }
    /// vLLM `priority` of a batch line, only when the batch runner scoped it
    /// to this exact route (its scheduling settings opt in) and the profile
    /// is vLLM-compatible. Never sent otherwise.
    fn batch_priority(&self, target: &Deployment) -> Option<i32> {
        self.profile
            .accepts_priority_hint()
            .then(|| crate::inference::scheduling::priority_for(target.id))
            .flatten()
    }
}
pub fn adapters(
    resolver: Arc<dyn SecretResolver>,
    approvals: ApprovedEndpoints,
) -> Vec<Arc<dyn ProviderAdapter>> {
    let approvals = Arc::new(approvals);
    Profile::ALL
        .into_iter()
        .map(|profile| {
            Arc::new(LocalAdapter::new(
                profile,
                resolver.clone(),
                approvals.clone(),
            )) as Arc<dyn ProviderAdapter>
        })
        .collect()
}
#[async_trait]
impl ProviderAdapter for LocalAdapter {
    fn id(&self) -> &'static str {
        self.profile.id()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            image_input: true,
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_chat_request(&self, request: &ChatRequest) -> bool {
        // Parsed Messages strict fields must reach validate_chat_request so the
        // refusal names `strict` before admission, rather than disappearing at
        // the coarse gate as a generic unsupported capability. This is not
        // strict support: the Messages field policy still refuses the field.
        self.capabilities().supports(request)
            && (request
                .messages_options
                .as_ref()
                .is_some_and(|o| o.strict_tools)
                || !request.tools.iter().any(|t| t.strict == Some(true)))
            && !(matches!(self.profile, Profile::Ollama) && request.tool_choice.is_some())
            && !(matches!(self.profile, Profile::OpenAiCompatible)
                && matches!(
                    request.tool_choice,
                    Some(ToolChoice::Required | ToolChoice::Function(_))
                ))
    }
    fn supports_embedding_request(&self, request: &EmbeddingRequest) -> bool {
        embeddings::validate(request).is_ok()
            && !(matches!(self.profile, Profile::OpenAiCompatible | Profile::Ollama)
                && request.dimensions.is_some())
    }
    /// The shared capability table (`providers::capabilities`); the rerank
    /// wire, System One and video support below must agree with it (tested).
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        super::capabilities::serves(self.id(), protocol)
    }
    /// The route's own options (0097) decide which lengths and sizes it
    /// takes; the adapter only requires a well-formed request.
    fn supports_video_request(
        &self,
        _target: &Deployment,
        request: &crate::jobs::types::VideoRequest,
    ) -> bool {
        matches!(self.profile, Profile::OpenAiCompatible) && request.validate().is_ok()
    }
    async fn create_video(
        &self,
        target: &Deployment,
        request: crate::jobs::types::VideoRequest,
    ) -> Result<crate::jobs::types::UpstreamVideo> {
        openai::jobs::create_video(&self.video_wire(target)?, target, request).await
    }
    async fn retrieve_video(
        &self,
        target: &Deployment,
        video: &crate::jobs::types::UpstreamId,
    ) -> Result<crate::jobs::types::UpstreamVideo> {
        openai::jobs::retrieve_video(&self.video_wire(target)?, video).await
    }
    async fn delete_video(
        &self,
        target: &Deployment,
        video: &crate::jobs::types::UpstreamId,
    ) -> Result<crate::jobs::types::VideoDeletion> {
        openai::jobs::delete_video(&self.video_wire(target)?, video).await
    }
    async fn video_content(
        &self,
        target: &Deployment,
        video: &crate::jobs::types::UpstreamId,
        asset: crate::jobs::types::VideoAsset,
    ) -> Result<crate::jobs::types::ContentStream> {
        openai::jobs::video_content(&self.video_wire(target)?, video, asset).await
    }
    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        let payload = self.encode_chat(target, &request)?;
        let (client, base) = self.connection(target)?;
        let response = self
            .authenticated_post(client, format!("{base}/chat/completions"), target)?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(super::image_body::json_body(&payload, &request)?)
            .send()
            .await
            .map_err(framing::send_error(crate::egress::UpstreamClient::Local))?;
        let response = if request.messages_options.is_some() {
            framing::generation_status(response).await?
        } else {
            framing::status(response.status())?;
            response
        };
        let output = if request.stream {
            framing::check_sse(&response)?;
            ProviderOutput::Stream(openai::decode_stream_profile(
                response,
                Some(self.id()),
                request.messages_options.is_some(),
            ))
        } else {
            let value = framing::body_limit(response, request.response_limit()).await?;
            ProviderOutput::Complete(openai::decode_complete_protocol(
                &value,
                Some(self.id()),
                request.messages_options.is_some(),
            )?)
        };
        Ok(super::messages::thinking_display(&request, output))
    }
    async fn execute_embeddings(
        &self,
        target: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse> {
        embeddings::validate(&request)?;
        if !self.supports_embedding_request(&request) {
            return Err(InferenceError::Unsupported);
        }
        let (client, base) = self.connection(target)?;
        let native = matches!(self.profile, Profile::Ollama);
        let (url, payload) = if native {
            // The base was canonically approved, including its origin and /v1 suffix.
            // Strip only that fixed suffix, retaining a reverse-proxy path prefix.
            let prefix = Self::prefix(base)?;
            (
                format!("{prefix}/api/embed"),
                embeddings::encode_ollama(&target.upstream_model, &request)?,
            )
        } else {
            let mut payload = embeddings::encode(&target.upstream_model, &request);
            if let Some(priority) = self.batch_priority(target) {
                payload["priority"] = priority.into();
            }
            (format!("{base}/embeddings"), payload)
        };
        let response = self
            .authenticated_post(client, url, target)?
            .json(&payload)
            .send()
            .await
            .map_err(framing::send_error(crate::egress::UpstreamClient::Local))?;
        framing::status(response.status())?;
        let value = framing::body(response).await?;
        if native {
            embeddings::decode_ollama(&value, &request)
        } else {
            embeddings::decode(&value, &request, framing::status_error)
        }
    }
    async fn execute_rerank(
        &self,
        target: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse> {
        request.validate()?;
        let wire = self.rerank_wire().ok_or(InferenceError::Unsupported)?;
        let (_, base) = self.connection(target)?;
        // vLLM's canonical route is `/rerank` (its `/v1/rerank` is a deprecated alias).
        let url = match self.profile {
            Profile::Vllm => format!("{}/rerank", Self::prefix(base)?),
            _ => format!("{base}/rerank"),
        };
        let payload = workloads::rerank_body(&target.upstream_model, &request, wire);
        let value = self.post_json(target, url, &payload).await?;
        workloads::decode_rerank(&value, &request, wire)
    }
    async fn execute_systemone(
        &self,
        target: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse> {
        request.validate()?;
        if !self.serves_systemone() {
            return Err(InferenceError::Unsupported);
        }
        let (_, base) = self.connection(target)?;
        let url = format!("{base}/systemone");
        let payload = request.wire(&target.upstream_model);
        let value = self.post_json(target, url, &payload).await?;
        workloads::decode_systemone(&value, &request)
    }
}
#[cfg(test)]
mod cancellation_tests;
#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod reasoning_tests;
#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "integration-tests"))]
mod tool_defaults_tests;
#[cfg(test)]
mod workload_tests;
