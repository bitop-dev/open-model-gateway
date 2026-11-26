//! OpenRouter wire protocol at the fixed origin `https://openrouter.ai/api/v1`.
//!
//! - The key is a server-held `env:` reference; inference credentials are
//!   never forwarded. Optional `HTTP-Referer`/`X-Title` come from server config.
//! - Redirects, ambient proxies and implicit retries are disabled.
//! - Non-success bodies are dropped except bounded structured context-error
//!   classification for Messages validation responses. Error bodies can carry
//!   account `user_id` and provider diagnostics: never log or forward them.
//!   HTTP status meanings and embedded error codes outrank diagnostic text.
//! - Every request sets `provider.data_collection` (default `"deny"`). Free
//!   (`:free`) endpoints may train on prompts and are unavailable under deny
//!   (upstream 404 → `upstream_rejected`).
//! - `usage.cost` is captured from the JSON number's source text as exact
//!   micro-USD (rounded up) for `provider_cost_microusd` evidence only; it is
//!   never the gateway charge.
//! - Chat: OpenRouter-only fields (`native_finish_reason`, reasoning traces)
//!   are normalized away. The `reasoning` text reaches only Messages clients
//!   (as unsigned thinking); `reasoning_details` is never returned. Reasoning
//!   tokens stay in `completion_tokens`. The final streaming accounting
//!   frame (repeated finish choice with an empty delta plus `usage`) is treated
//!   as usage, not as a second terminal event.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{self, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

use super::error_envelope::empty;
use super::{
    ProviderAdapter, embeddings, metering, openai,
    secrets::SecretResolver,
    text_workloads::{rerank_results, rerank_usage, systemone_usage},
};
use crate::inference::{error::InferenceError, evidence, types::*};

pub const BASE: &str = "https://openrouter.ai/api/v1";
const BODY_LIMIT: usize = 4 * 1024 * 1024;

type Result<T> = std::result::Result<T, InferenceError>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataCollection {
    #[default]
    Deny,
    Allow,
}
impl DataCollection {
    /// `GATEWAY_OPENROUTER_DATA_COLLECTION` (`deny` default, or `allow`).
    /// `serve` validates it at startup through `OpenRouterConfig::from_env`.
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("GATEWAY_OPENROUTER_DATA_COLLECTION")
            .ok()
            .filter(|v| !v.is_empty())
            .as_deref()
        {
            None | Some("deny") => Ok(Self::Deny),
            Some("allow") => Ok(Self::Allow),
            Some(_) => anyhow::bail!("GATEWAY_OPENROUTER_DATA_COLLECTION must be deny or allow"),
        }
    }
    /// The operator override from `GATEWAY_OPENROUTER_DATA_COLLECTION`, when set.
    /// It locks Admin › Settings › Data & privacy. Invalid values fail `serve`
    /// startup; elsewhere they fall back to the deny override (fail closed).
    pub fn env_override() -> Option<Self> {
        static OVERRIDE: std::sync::OnceLock<Option<DataCollection>> = std::sync::OnceLock::new();
        *OVERRIDE.get_or_init(|| {
            std::env::var_os("GATEWAY_OPENROUTER_DATA_COLLECTION")
                .filter(|v| !v.is_empty())
                .map(|_| Self::from_env().unwrap_or_default())
        })
    }
    /// The installation setting (0010 `installation_settings`), refreshed by the
    /// maintenance loop and on save. Deny until first loaded.
    pub fn installation_default() -> Self {
        if INSTALLATION_ALLOWS.load(std::sync::atomic::Ordering::SeqCst) {
            Self::Allow
        } else {
            Self::Deny
        }
    }
    pub fn set_installation_default(value: Self) {
        INSTALLATION_ALLOWS.store(value == Self::Allow, std::sync::atomic::Ordering::SeqCst);
    }
    /// The effective policy (no secrets): the environment override, else the
    /// installation setting.
    pub fn configured() -> Self {
        Self::env_override().unwrap_or_else(Self::installation_default)
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deny" => Some(Self::Deny),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

static INSTALLATION_ALLOWS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Server configuration; never derived from inference requests.
#[derive(Clone, Default)]
pub struct OpenRouterConfig {
    http_referer: Option<HeaderValue>,
    title: Option<HeaderValue>,
    data_collection: DataCollection,
    /// No environment override: follow the live installation setting.
    follow_installation: bool,
}
impl OpenRouterConfig {
    pub fn new(
        http_referer: Option<&str>,
        title: Option<&str>,
        data_collection: DataCollection,
    ) -> anyhow::Result<Self> {
        let referer = http_referer
            .map(|v| {
                anyhow::ensure!(
                    (v.starts_with("https://") || v.starts_with("http://"))
                        && v.len() <= 512
                        && v.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                    "GATEWAY_OPENROUTER_HTTP_REFERER must be an absolute http(s) URL"
                );
                Ok(HeaderValue::from_str(v)?)
            })
            .transpose()?;
        let title = title
            .map(|v| {
                anyhow::ensure!(
                    !v.trim().is_empty()
                        && v.trim() == v
                        && v.len() <= 128
                        && v.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                    "GATEWAY_OPENROUTER_TITLE must be 1-128 printable ASCII characters"
                );
                Ok(HeaderValue::from_str(v)?)
            })
            .transpose()?;
        Ok(Self {
            http_referer: referer,
            title,
            data_collection,
            follow_installation: false,
        })
    }
    /// `GATEWAY_OPENROUTER_HTTP_REFERER`, `GATEWAY_OPENROUTER_TITLE`,
    /// `GATEWAY_OPENROUTER_DATA_COLLECTION` (`deny` default, or `allow`).
    pub fn from_env() -> anyhow::Result<Self> {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let data_collection = DataCollection::from_env()?;
        let mut config = Self::new(
            get("GATEWAY_OPENROUTER_HTTP_REFERER").as_deref(),
            get("GATEWAY_OPENROUTER_TITLE").as_deref(),
            data_collection,
        )?;
        // Unset: Admin › Settings › Data & privacy decides, per request.
        config.follow_installation = get("GATEWAY_OPENROUTER_DATA_COLLECTION").is_none();
        Ok(config)
    }
}

pub struct OpenRouterAdapter {
    client: crate::egress::HttpClient,
    resolver: Arc<dyn SecretResolver>,
    // Only the private test constructor can replace the production origin.
    base: String,
    config: OpenRouterConfig,
}

impl OpenRouterAdapter {
    pub fn new(
        resolver: Arc<dyn SecretResolver>,
        config: OpenRouterConfig,
    ) -> anyhow::Result<Self> {
        // Every resolved address must pass the egress policy (#33).
        let client = crate::egress::client_builder(crate::egress::resolver(
            crate::egress::Purpose::Provider,
        ))
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(crate::egress::UpstreamClient::Openrouter.pool_idle_timeout())
        .count_connections(crate::egress::UpstreamClient::Openrouter)
        .build()
        .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?;
        Ok(Self {
            client,
            resolver,
            base: BASE.into(),
            config,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        resolver: Arc<dyn SecretResolver>,
        config: OpenRouterConfig,
        base: String,
    ) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let mut adapter = Self::new(resolver, config).unwrap();
        adapter.client = crate::egress::HttpClient::from_test_client(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .build()
                .unwrap(),
        );
        adapter.base = base;
        adapter
    }

    fn provider_preferences(&self) -> Value {
        let policy = if self.config.follow_installation {
            DataCollection::configured()
        } else {
            self.config.data_collection
        };
        json!({"data_collection": policy.as_str()})
    }

    /// Validate the connection before resolving credentials, then POST.
    async fn post(
        &self,
        target: &Deployment,
        path: &str,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let body = serde_json::to_vec(body).map_err(|_| InferenceError::InvalidRequest)?;
        self.post_inner(target, path, body, false).await
    }
    async fn post_inner(
        &self,
        target: &Deployment,
        path: &str,
        body: Vec<u8>,
        messages: bool,
    ) -> Result<reqwest::Response> {
        if target.provider != self.id()
            || !target.credential_ref.starts_with("env:")
            || target
                .endpoint
                .as_deref()
                .is_some_and(|v| v != BASE && v != "https://openrouter.ai/api/v1/")
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
            || target.upstream_model.trim().is_empty()
        {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
            .map_err(|_| InferenceError::Configuration)?;
        auth.set_sensitive(true);
        let mut builder = self
            .client
            .post(format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, auth)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(referer) = &self.config.http_referer {
            builder = builder.header("HTTP-Referer", referer.clone());
        }
        if let Some(title) = &self.config.title {
            builder = builder.header("X-Title", title.clone());
        }
        let response = builder.send().await.map_err(transport)?;
        if !response.status().is_success() {
            let code = response.status().as_u16();
            // Keep this provider's status meanings (notably account credit,
            // moderation and timeout) before considering an error body. Only
            // validation envelopes may refine a refusal to context overflow.
            if messages && matches!(code, 400 | 413 | 422) {
                return super::framing::generation_status(response).await;
            }
            // Other error bodies are never parsed, logged or forwarded.
            return Err(status_error(code));
        }
        Ok(response)
    }
}

fn transport(error: reqwest::Error) -> InferenceError {
    crate::egress::UpstreamClient::Openrouter.lost_before_response(&error);
    if error.is_timeout() {
        InferenceError::Timeout
    } else {
        InferenceError::UpstreamUnavailable
    }
}

/// Status (or embedded `error.code`) to a sanitized error kind.
fn status_error(status: u16) -> InferenceError {
    match status {
        // Bad key, or the gateway's OpenRouter account lacks credit/limit:
        // operator action, not a client error.
        401 | 402 => InferenceError::Configuration,
        // Moderation/guardrail rejection of this request.
        403 => InferenceError::UpstreamRejected,
        408 | 524 => InferenceError::Timeout,
        429 => InferenceError::Busy,
        // Redirects are never followed.
        300..=399 => InferenceError::Configuration,
        400..=499 => InferenceError::UpstreamRejected,
        500..=599 => InferenceError::UpstreamUnavailable,
        _ => InferenceError::InvalidUpstream,
    }
}

/// The response path's class of HTTP `code` for the error body `value`:
/// `status_error`, and for a Messages client a validation status refined by
/// the body (`generation_error`), as `post_inner` applies it.
fn error_status(value: &Value, messages: bool, code: u16) -> InferenceError {
    if messages && matches!(code, 400 | 413 | 422) {
        super::framing::generation_error(code, Some(value), super::framing::classified_error)
    } else {
        status_error(code)
    }
}

/// A 200 body carrying `error` (`error_envelope`), for an endpoint whose
/// answer is in `output` and whose usage is in `usage_members`, classified
/// with `as_http`. `cost` is the body's exact `usage.cost`: a positive or
/// unreadable one is usage too, so the error is `InvalidUpstream`. On an
/// error, the endpoint's token or duration usage (`usage`, parsed without
/// the cost) and the exact cost are kept as independent evidence: one being
/// malformed does not drop the other. The body's `model` is not recorded.
fn embedded_error(
    value: &Value,
    output: &[&str],
    usage_members: &[&str],
    cost: &Result<Option<i64>>,
    as_http: impl FnOnce(u16) -> InferenceError,
    usage: impl FnOnce() -> Option<Result<Usage>>,
) -> Result<()> {
    let Err(class) = super::error_envelope::check(value, output, usage_members, as_http) else {
        return Ok(());
    };
    let class = if matches!(cost, Ok(None | Some(0))) {
        class
    } else {
        InferenceError::InvalidUpstream
    };
    let cost = cost.as_ref().ok().copied().flatten();
    evidence::preserve(Err(class), || match usage() {
        Some(Ok(mut observed)) => {
            observed.provider_cost_microusd = cost;
            Some(Ok(observed))
        }
        _ => cost.map(|cost| {
            Ok(Usage {
                provider_cost_microusd: Some(cost),
                ..Usage::default()
            })
        }),
    })
}

fn stream_embedded_error(value: &Value, messages: bool) -> Result<()> {
    if empty(&value["error"]) {
        return Ok(());
    }
    Err(super::stream_error::classify(&value["error"], |code| {
        error_status(value, messages, code)
    }))
}

async fn read(response: reqwest::Response) -> Result<Vec<u8>> {
    read_limit(response, BODY_LIMIT).await
}
async fn read_limit(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport)?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(InferenceError::InvalidUpstream);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct CostProbe<'a> {
    #[serde(borrow, default)]
    usage: Option<UsageCost<'a>>,
}
#[derive(Deserialize)]
struct UsageCost<'a> {
    #[serde(borrow, default)]
    cost: Option<&'a RawValue>,
}
/// `usage.cost` from the original JSON text (exact; no f64 round trip).
fn provider_cost(bytes: &[u8]) -> Result<Option<i64>> {
    let probe: CostProbe =
        serde_json::from_slice(bytes).map_err(|_| InferenceError::InvalidUpstream)?;
    probe
        .usage
        .and_then(|u| u.cost)
        .map(|raw| metering::usd_text_to_microusd_ceil(raw.get()))
        .transpose()
}

/// A JSON object body. The caller checks its `error` (`embedded_error`)
/// before reading its exact cost (`provider_cost`), so a malformed cost next
/// to an error is evidence of processing rather than a parse failure.
fn object(bytes: &[u8]) -> Result<Value> {
    let value: Value = super::framing::json(bytes)?;
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(value)
}

/// Embedding models with a fixed output width that rejects other `dimensions`.
fn fixed_dimensions(model: &str) -> Option<u32> {
    let base = model.strip_suffix(":free").unwrap_or(model);
    (base == "nvidia/nemotron-3-embed-1b").then_some(2048)
}

#[async_trait]
impl ProviderAdapter for OpenRouterAdapter {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            image_input: true,
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }

    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        super::capabilities::serves(self.id(), protocol)
    }

    fn validate_chat_request(&self, request: &ChatRequest) -> Result<()> {
        super::messages::validate(request, super::messages::ProviderClass::OpenAi)
    }

    fn supports_transcription_request(
        &self,
        _target: &Deployment,
        request: &TranscriptionRequest,
    ) -> bool {
        audio::supports_transcription(request)
    }

    fn supports_speech_request(&self, _target: &Deployment, request: &SpeechRequest) -> bool {
        audio::supports_speech(request)
    }

    async fn execute_audio_transcription(
        &self,
        target: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse> {
        self.transcribe(target, request).await
    }

    async fn execute_audio_speech(
        &self,
        target: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse> {
        self.speak(target, request).await
    }

    fn supports_image_request(&self, _target: &Deployment, request: &ImageRequest) -> bool {
        images::supports(request)
    }

    async fn execute_images(
        &self,
        target: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse> {
        self.generate_images(target, request).await
    }

    fn supports_embedding_target(&self, target: &Deployment, request: &EmbeddingRequest) -> bool {
        // OpenRouter's catalog advertises no `dimensions` parameter for its
        // embedding models; only a model's fixed native width is accepted.
        match fixed_dimensions(&target.upstream_model) {
            Some(width) => request.dimensions.is_none_or(|d| d == width),
            None => request.dimensions.is_none(),
        }
    }

    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        super::messages::validate(&request, super::messages::ProviderClass::OpenAi)?;
        if !self.supports_chat_request(&request) {
            return Err(InferenceError::Unsupported);
        }
        let mut body = openai::encode(&target.upstream_model, &request);
        body["provider"] = self.provider_preferences();
        body["usage"] = json!({"include": true});
        messages_controls(&mut body, &request);
        require_parameters(&mut body, &request);
        let body = super::image_body::json_body(&body, &request)?;
        let response = self
            .post_inner(
                target,
                "/chat/completions",
                body,
                request.messages_options.is_some(),
            )
            .await?;
        let output = if request.stream {
            super::framing::check_sse(&response)?;
            ProviderOutput::Stream(decode_chat_stream(
                response,
                request.messages_options.is_some(),
            ))
        } else {
            ProviderOutput::Complete(decode_chat(
                &read_limit(response, request.response_limit()).await?,
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
        if !self.supports_embedding_target(target, &request) {
            return Err(InferenceError::Unsupported);
        }
        let mut body = embeddings::encode(&target.upstream_model, &request);
        body["provider"] = self.provider_preferences();
        let response = self.post(target, "/embeddings", &body).await?;
        let bytes = read(response).await?;
        let value: Value = super::framing::json(&bytes)?;
        if !value.is_object() {
            return Err(InferenceError::InvalidUpstream);
        }
        let cost = provider_cost(&bytes);
        // Independent evidence: bad token metering must not erase a valid
        // reported cost, nor may bad cost erase valid token observations.
        // Output zero is semantic for embeddings; invalid input stays unknown.
        let mut observed = metering::embedding(&value["usage"]).unwrap_or(Usage {
            output_tokens: Some(0),
            ..Usage::default()
        });
        observed.meters = Some(metering::text_workload_meters(Some(0)));
        observed.provider_cost_microusd = cost.as_ref().ok().copied().flatten();
        embedded_error(&value, &["data"], &["usage"], &cost, status_error, || {
            value["usage"].is_object().then_some(Ok(observed))
        })?;
        evidence::preserve(
            cost.and_then(|_| {
                let mut decoded = embeddings::decode(&value, &request, status_error)?;
                decoded.usage = observed;
                if let Some(width) = fixed_dimensions(&target.upstream_model)
                    && decoded.embeddings.iter().any(|v| v.len() != width as usize)
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                Ok(decoded)
            }),
            || Some(Ok(observed)),
        )
    }

    async fn execute_rerank(
        &self,
        target: &Deployment,
        request: RerankRequest,
    ) -> Result<RerankResponse> {
        request.validate()?;
        let mut body = json!({
            "model": target.upstream_model,
            "query": request.query,
            "documents": request.documents,
            "provider": self.provider_preferences(),
        });
        if request.top_n.is_some() {
            body["top_n"] = json!(request.result_limit());
        }
        let response = self.post(target, "/rerank", &body).await?;
        let bytes = read(response).await?;
        let value = object(&bytes)?;
        let cost = provider_cost(&bytes);
        embedded_error(
            &value,
            &["results"],
            &["usage"],
            &cost,
            status_error,
            || {
                value["usage"]
                    .is_object()
                    .then(|| rerank_usage(&value["usage"], None))
            },
        )?;
        let usage = rerank_usage(&value["usage"], cost?)?;
        evidence::preserve(
            rerank_results(&value, &request).map(|results| RerankResponse { results, usage }),
            || Some(Ok(usage)),
        )
    }

    async fn execute_systemone(
        &self,
        target: &Deployment,
        request: SystemoneRequest,
    ) -> Result<SystemoneResponse> {
        request.validate()?;
        let mut body = request.wire(&target.upstream_model);
        body["provider"] = self.provider_preferences();
        let response = self.post(target, "/systemone", &body).await?;
        let bytes = read(response).await?;
        let value = object(&bytes)?;
        let cost = provider_cost(&bytes);
        embedded_error(
            &value,
            &["answers"],
            &["usage"],
            &cost,
            status_error,
            || {
                value["usage"]
                    .is_object()
                    .then(|| systemone_usage(&value["usage"], None))
            },
        )?;
        let usage = systemone_usage(&value["usage"], cost?)?;
        evidence::preserve(
            parse_answers(&value["answers"], &request)
                .map(|answers| SystemoneResponse { answers, usage }),
            || Some(Ok(usage)),
        )
    }
}

mod audio;
mod chat;
use chat::{decode_chat, decode_chat_stream, messages_controls, require_parameters};
mod images;
#[cfg(test)]
mod tests;
