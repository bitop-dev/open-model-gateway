//! OpenAI's wire protocol lives here; no upstream identifiers or error bodies escape.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use serde_json::{Map, Value, json};

use super::error_envelope::empty;
use super::{ProviderAdapter, secrets::SecretResolver};
use crate::inference::{error::InferenceError, types::*};
use crate::jobs::types::{
    BatchEndpoint, BatchRequest, ByteStream, ContentStream, NativeResult, UpstreamBatch,
    UpstreamId, UpstreamVideo, VideoAsset, VideoRequest,
};

const BASE: &str = "https://api.openai.com/v1";
#[cfg(test)]
const BODY_LIMIT: usize = 4 * 1024 * 1024;
const SSE_LIMIT: usize = 1024 * 1024;
const MAX_TOOL_CALLS: usize = 128;

type Result<T> = std::result::Result<T, InferenceError>;

pub struct OpenAiAdapter {
    pub(super) client: crate::egress::HttpClient,
    pub(super) resolver: Arc<dyn SecretResolver>,
    // Only the private test constructor can replace the production origin.
    pub(super) base: String,
}

impl OpenAiAdapter {
    pub fn new(resolver: Arc<dyn SecretResolver>) -> anyhow::Result<Self> {
        // Every resolved address must pass the egress policy (#33).
        let client = crate::egress::client_builder(crate::egress::resolver(
            crate::egress::Purpose::Provider,
        ))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(crate::egress::UpstreamClient::Openai.pool_idle_timeout())
        .count_connections(crate::egress::UpstreamClient::Openai)
        .build()
        .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?;
        Ok(Self {
            client,
            resolver,
            base: BASE.into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(resolver: Arc<dyn SecretResolver>, base: String) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let mut adapter = Self::new(resolver).unwrap();
        adapter.base = base;
        adapter
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiAdapter {
    fn id(&self) -> &'static str {
        "openai"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
            image_input: true,
        }
    }

    /// The shared capability table (`providers::capabilities`). Videos are
    /// not served: OpenAI shut down the Sora 2 models and the Videos API on
    /// 2026-09-24, so new video jobs are refused before admission. The video
    /// wire code below stays for jobs created before the shutdown.
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        super::capabilities::serves(self.id(), protocol)
    }

    fn validate_chat_request(&self, request: &ChatRequest) -> Result<()> {
        super::messages::validate(request, super::messages::ProviderClass::OpenAi)
    }

    async fn connect_realtime(
        &self,
        target: &Deployment,
        setup: &crate::inference::realtime::RealtimeSetup,
    ) -> Result<crate::inference::realtime::RealtimeUpstream> {
        realtime::connect(self, target, setup).await
    }

    fn supports_video_request(&self, target: &Deployment, request: &VideoRequest) -> bool {
        jobs::supports_video(target, request)
    }

    async fn create_video(
        &self,
        target: &Deployment,
        request: VideoRequest,
    ) -> Result<UpstreamVideo> {
        jobs::create_video(self, target, request).await
    }

    async fn retrieve_video(
        &self,
        target: &Deployment,
        video: &UpstreamId,
    ) -> Result<UpstreamVideo> {
        jobs::retrieve_video(self, target, video).await
    }

    async fn delete_video(&self, target: &Deployment, video: &UpstreamId) -> Result<()> {
        jobs::delete_video(self, target, video).await
    }

    async fn video_content(
        &self,
        target: &Deployment,
        video: &UpstreamId,
        asset: VideoAsset,
    ) -> Result<ContentStream> {
        jobs::video_content(self, target, video, asset).await
    }

    fn native_batch(&self, target: &Deployment, _endpoint: BatchEndpoint) -> bool {
        jobs::native_batch(target)
    }

    fn encode_native_line(
        &self,
        target: &Deployment,
        endpoint: BatchEndpoint,
        custom_id: &str,
        request: &BatchRequest,
    ) -> Result<Vec<u8>> {
        jobs::encode_native_line(target, endpoint, custom_id, request)
    }

    async fn submit_native_batch(
        &self,
        target: &Deployment,
        endpoint: BatchEndpoint,
        records: ByteStream,
    ) -> Result<UpstreamBatch> {
        jobs::submit_native_batch(self, target, endpoint, records).await
    }

    async fn native_batch_results(
        &self,
        target: &Deployment,
        batch: &UpstreamBatch,
    ) -> Result<ByteStream> {
        jobs::native_batch_results(self, target, batch).await
    }

    fn decode_native_result(
        &self,
        _target: &Deployment,
        endpoint: BatchEndpoint,
        line: &[u8],
    ) -> Result<NativeResult> {
        jobs::decode_native_result(endpoint, line)
    }

    async fn delete_native_batch(&self, target: &Deployment, batch: &UpstreamBatch) -> Result<()> {
        jobs::delete_native_batch(self, target, batch).await
    }

    async fn retrieve_batch(
        &self,
        target: &Deployment,
        batch: &UpstreamId,
    ) -> Result<UpstreamBatch> {
        jobs::batch_action(self, target, batch, false).await
    }

    async fn cancel_batch(&self, target: &Deployment, batch: &UpstreamId) -> Result<UpstreamBatch> {
        jobs::batch_action(self, target, batch, true).await
    }

    fn supports_transcription_request(&self, _: &Deployment, _: &TranscriptionRequest) -> bool {
        true
    }

    fn supports_speech_request(&self, _: &Deployment, _: &SpeechRequest) -> bool {
        true
    }

    async fn execute_audio_transcription(
        &self,
        target: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse> {
        audio::transcribe(self, target, request).await
    }

    async fn execute_audio_speech(
        &self,
        target: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse> {
        audio::speak(self, target, request).await
    }

    fn supports_image_request(&self, target: &Deployment, request: &ImageRequest) -> bool {
        images::supports(target, request)
    }

    async fn execute_images(
        &self,
        target: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse> {
        images::execute(self, target, request).await
    }

    async fn execute_protocol(
        &self,
        target: &Deployment,
        request: ChatRequest,
        protocol: ApiProtocol,
    ) -> Result<ProviderOutput> {
        match protocol {
            ApiProtocol::ChatCompletions | ApiProtocol::Messages => {
                self.execute(target, request).await
            }
            ApiProtocol::Responses => super::openai_responses::execute(self, target, request).await,
            _ => Err(InferenceError::Unsupported),
        }
    }

    async fn execute_embeddings(
        &self,
        target: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse> {
        super::embeddings::validate(&request)?;
        if target.provider != self.id()
            || target.credential_ref == "none"
            || target
                .endpoint
                .as_deref()
                .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
        {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut auth = header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
            .map_err(|_| InferenceError::Configuration)?;
        auth.set_sensitive(true);
        let response = self
            .client
            .post(format!("{}/embeddings", self.base))
            .header(header::AUTHORIZATION, auth)
            .json(&super::embeddings::encode(&target.upstream_model, &request))
            .send()
            .await
            .map_err(transport_error)?;
        check_status(response.status())?;
        super::embeddings::decode(
            &super::framing::body(response).await?,
            &request,
            super::framing::status_error,
        )
    }

    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        self.validate_chat_request(&request)?;
        // Validate before resolving credentials, even when using the test transport.
        if target.provider != self.id()
            || target.credential_ref == "none"
            || target
                .endpoint
                .as_deref()
                .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
        {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
                .map_err(|_| InferenceError::Configuration)?;
        authorization.set_sensitive(true);
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base))
            .header(header::AUTHORIZATION, authorization)
            .header(header::CONTENT_TYPE, "application/json")
            .body(super::image_body::json_body(
                &encode_chat(&target.upstream_model, &request),
                &request,
            )?)
            .send()
            .await
            .map_err(transport_error)?;
        let response = if request.messages_options.is_some() {
            super::framing::generation_status(response).await?
        } else {
            check_status(response.status())?;
            response
        };
        if request.stream {
            let is_sse = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
                });
            if !is_sse {
                return Err(InferenceError::InvalidUpstream);
            }
            Ok(ProviderOutput::Stream(decode_stream(
                response,
                request.messages_options.is_some(),
            )))
        } else {
            if response
                .content_length()
                .is_some_and(|n| n > request.response_limit() as u64)
            {
                return Err(InferenceError::InvalidUpstream);
            }
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(transport_error)?;
                if chunk.len() > request.response_limit().saturating_sub(body.len()) {
                    return Err(InferenceError::InvalidUpstream);
                }
                body.extend_from_slice(&chunk);
            }
            let value = super::framing::json(&body)?;
            Ok(ProviderOutput::Complete(decode_complete_protocol(
                &value,
                None,
                request.messages_options.is_some(),
            )?))
        }
    }
}

fn transport_error(error: reqwest::Error) -> InferenceError {
    crate::egress::UpstreamClient::Openai.lost_before_response(&error);
    if error.is_timeout() {
        InferenceError::Timeout
    } else {
        InferenceError::UpstreamUnavailable
    }
}

fn check_status(status: StatusCode) -> Result<()> {
    super::framing::status(status)
}

pub(super) use chat_controls::{encode_chat, json_schema_format};

pub(super) fn encode(model: &str, request: &ChatRequest) -> Value {
    let mut images = 0;
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let mut value = json!({"role": message.role, "content": super::image_body::chat_content(message.content.as_ref(), &mut images)});
            if message.role == Role::Tool && message.tool_result_is_error {
                value["content"] = json!(format!(
                    "[tool_error]\n{}",
                    message.content.as_ref().and_then(Content::text).unwrap_or("")
                ));
            }
            if let Some(id) = &message.tool_call_id {
                value["tool_call_id"] = json!(id);
            }
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .map(|tool| {
                            json!({
                                "id": tool.id, "type": "function",
                                "function": { "name": tool.name, "arguments": tool.arguments }
                            })
                        })
                        .collect(),
                );
            }
            value
        })
        .collect();
    let mut value = json!({"model": model, "messages": messages, "stream": request.stream});
    if request.stream {
        value["stream_options"] = json!({"include_usage": true});
    }
    if let Some(temperature) = request.temperature {
        value["temperature"] = json!(temperature);
    }
    if let Some(options) = &request.messages_options {
        if let Some(p) = options.top_p {
            value["top_p"] = json!(p);
        }
        if let Some(stops) = &options.stop_sequences {
            value["stop"] = json!(stops);
        }
        // user_id is a client label, never an authority/provider identity.
    }
    if let Some(tokens) = request.max_output_tokens {
        value["max_completion_tokens"] = json!(tokens);
    }
    if !request.tools.is_empty() {
        value["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    let mut function = json!({"name": tool.name, "parameters": tool.parameters});
                    if let Some(description) = &tool.description {
                        function["description"] = json!(description);
                    }
                    if let Some(strict) = tool.strict {
                        function["strict"] = json!(strict);
                    }
                    json!({"type": "function", "function": function})
                })
                .collect(),
        );
    }
    if let Some(choice) = &request.tool_choice {
        value["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function(name) => json!({"type": "function", "function": {"name": name}}),
        };
    }
    value
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().ok_or(InferenceError::InvalidUpstream)
}

fn array(value: &Value) -> Result<&Vec<Value>> {
    value.as_array().ok_or(InferenceError::InvalidUpstream)
}

fn string(value: &Value) -> Result<&str> {
    value.as_str().ok_or(InferenceError::InvalidUpstream)
}

fn nonempty_string(value: &Value) -> Result<String> {
    let value = string(value)?;
    if value.is_empty() {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(value.to_owned())
}

fn optional_string(value: &Value) -> Result<Option<String>> {
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(string(value)?.to_owned()))
    }
}

// Content-bearing objects must not silently lose newly introduced modalities.
fn known_fields(value: &Value, allowed: &[&str]) -> Result<()> {
    if object(value)?
        .iter()
        .any(|(key, value)| !allowed.contains(&key.as_str()) && !empty(value))
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}

use local_fields::{
    LOCAL_REASONING_FIELDS, LOCAL_STOP_FIELDS, known_profile_fields, local_reasoning,
};

fn finish(value: &Value) -> Result<FinishReason> {
    match string(value)? {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" => Ok(FinishReason::ToolCalls),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(InferenceError::InvalidUpstream),
    }
}

pub(super) fn usage(value: &Value) -> Result<Usage> {
    super::metering::inclusive(
        value,
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
        "cache_write_tokens",
    )
}
pub(super) fn local_usage(value: &Value, profile: &str) -> Result<Usage> {
    let mut usage = super::metering::inclusive(
        value,
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
        if profile == "vllm" {
            "created_cache_tokens"
        } else {
            "cache_write_tokens"
        },
    )?;
    // SGLang reports the breakdown as top-level `usage.reasoning_tokens`.
    if usage.reasoning_tokens.is_none() {
        usage.reasoning_tokens =
            super::metering::top_level_reasoning_tokens(value, usage.output_tokens);
    }
    Ok(usage)
}

#[cfg(test)]
pub(super) fn decode_complete(value: &Value) -> Result<ChatResponse> {
    decode_complete_profile(value, None)
}
/// Structural failure still fails, but a valid usage object is kept as evidence.
#[cfg(test)]
pub(super) fn decode_complete_profile(
    value: &Value,
    profile: Option<&str>,
) -> Result<ChatResponse> {
    decode_complete_protocol(value, profile, false)
}
pub(super) fn decode_complete_protocol(
    value: &Value,
    profile: Option<&str>,
    messages: bool,
) -> Result<ChatResponse> {
    crate::inference::evidence::preserve(decode_shape(value, profile, messages), || {
        value["usage"].is_object().then(|| {
            match profile {
                Some(profile) => local_usage(&value["usage"], profile),
                None => usage(&value["usage"]),
            }
            .map(|u| super::metering::with_model(u, &value["model"]))
        })
    })
}
/// Whether an OpenAI Chat wire `refusal` is accepted: only for Messages
/// requests on the cloud wire (OpenAI and OpenRouter), where strict
/// structured output lets a model decline. Messages has a refusal stop reason
/// for it; Chat Completions and Responses clients keep refusing it as lossy,
/// and local profiles never send structured output.
fn accepts_refusal(profile: Option<&str>, messages: bool) -> bool {
    messages && profile.is_none()
}
/// A nonempty string `refusal`. Null and `""` are no refusal; anything else
/// is not a refusal this wire defines.
fn refusal(value: &Value) -> Result<Option<&str>> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok((!text.is_empty()).then_some(text.as_str())),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
const MESSAGE_FIELDS: &[&str] = &["role", "content", "tool_calls"];
const REFUSAL_MESSAGE_FIELDS: &[&str] = &["role", "content", "tool_calls", "refusal"];

fn decode_shape(value: &Value, profile: Option<&str>, messages: bool) -> Result<ChatResponse> {
    object(value)?;
    // A 200 error envelope (`error_envelope`); the caller's `preserve`
    // keeps a valid usage object next to it as evidence.
    super::error_envelope::check(value, &["choices"], &["usage"], |code| {
        super::stream_error::openai_chat_status(value, messages, code)
    })?;
    let choices = array(&value["choices"])?;
    if choices.len() != 1 {
        return Err(InferenceError::InvalidUpstream);
    }
    let choice = &choices[0];
    if choice["index"].as_u64() != Some(0) {
        return Err(InferenceError::InvalidUpstream);
    }
    known_profile_fields(
        choice,
        &["index", "message", "finish_reason", "logprobs"],
        profile,
        LOCAL_STOP_FIELDS,
    )?;
    let message = &choice["message"];
    let accepts_refusal = accepts_refusal(profile, messages);
    known_profile_fields(
        message,
        if accepts_refusal {
            REFUSAL_MESSAGE_FIELDS
        } else {
            MESSAGE_FIELDS
        },
        profile,
        LOCAL_REASONING_FIELDS,
    )?;
    if message["role"].as_str() != Some("assistant") {
        return Err(InferenceError::InvalidUpstream);
    }
    let content = optional_string(&message["content"])?;
    let mut tool_calls = Vec::new();
    if !message["tool_calls"].is_null() {
        let calls = array(&message["tool_calls"])?;
        if calls.len() > MAX_TOOL_CALLS {
            return Err(InferenceError::InvalidUpstream);
        }
        for tool in calls {
            known_fields(tool, &["id", "type", "function"])?;
            if tool["type"].as_str() != Some("function") {
                return Err(InferenceError::InvalidUpstream);
            }
            let function = &tool["function"];
            known_fields(function, &["name", "arguments"])?;
            let id = nonempty_string(&tool["id"])?;
            if tool_calls.iter().any(|t: &ToolCall| t.id == id) {
                return Err(InferenceError::InvalidUpstream);
            }
            tool_calls.push(ToolCall {
                id,
                name: nonempty_string(&function["name"])?,
                arguments: string(&function["arguments"])?.into(),
            });
        }
    }
    // Only Messages clients receive a local trace (as unsigned thinking).
    let reasoning = match profile {
        Some(_) if messages => local_reasoning(message)?,
        _ => None,
    };
    let mut finish_reason = finish(&choice["finish_reason"])?;
    // A refusal is the whole reply. Its text is the provider's explanation,
    // not output: it is not returned as content and never logged; the body
    // that held it was already read within the response bound.
    if accepts_refusal && refusal(&message["refusal"])?.is_some() {
        if content.as_deref().is_some_and(|c| !c.is_empty()) || !tool_calls.is_empty() {
            return Err(InferenceError::InvalidUpstream);
        }
        finish_reason = FinishReason::Refusal;
    }
    Ok(ChatResponse {
        messages_content: None,
        reasoning,
        content: content.filter(|c| finish_reason != FinishReason::Refusal || !c.is_empty()),
        tool_calls,
        finish_reason,
        usage: super::metering::with_model(
            match profile {
                Some(profile) => local_usage(&value["usage"], profile)?,
                None => usage(&value["usage"])?,
            },
            &value["model"],
        ),
    })
}

/// Incremental byte framing: UTF-8 is decoded only after a complete line, so a
/// codepoint, CRLF pair, or event may span any number of network chunks. The
/// entire frame (including comments and ignored fields) has a hard byte bound.
#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    skip_lf: bool,
    first_line: bool,
}

impl SseDecoder {
    pub(super) fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub(super) fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        self.event_bytes += 1;
        if self.event_bytes > SSE_LIMIT {
            return Err(InferenceError::InvalidUpstream);
        }
        if byte != b'\n' && byte != b'\r' {
            if self.line.len() == SSE_LIMIT {
                return Err(InferenceError::InvalidUpstream);
            }
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        let line = std::str::from_utf8(&self.line).map_err(|_| InferenceError::InvalidUpstream)?;
        let line = if self.first_line {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        self.first_line = false;
        if line.is_empty() {
            self.line.clear();
            self.event_bytes = 0;
            if self.data.is_empty() {
                return Ok(None);
            }
            self.data.pop(); // SSE joins consecutive data fields with a newline.
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            if field == "data" {
                if self.data.len() + value.len() + 1 > SSE_LIMIT {
                    return Err(InferenceError::InvalidUpstream);
                }
                self.data.extend_from_slice(value.as_bytes());
                self.data.push(b'\n');
            }
        }
        self.line.clear();
        Ok(None)
    }
}

fn decode_stream(response: reqwest::Response, messages: bool) -> EventStream {
    decode_stream_profile(response, None, messages)
}
pub(super) fn decode_stream_profile(
    response: reqwest::Response,
    profile: Option<&'static str>,
    messages: bool,
) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let mut chunks = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut state = StreamState::new(profile, messages);
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(transport_error)?;
            for &byte in chunk.iter() {
                if let Some(data) = decoder.push(byte)? {
                    for event in state.decode(&data)? {
                        let done = matches!(event, ChatEvent::Done);
                        yield event;
                        if done { return; }
                    }
                }
            }
        }
        // A valid finish chunk is insufficient; only [DONE] proves completion.
        Err(super::framing::TRANSPORT)?;
    })
}

mod audio;
mod chat_controls;
#[path = "openai/chat_stream.rs"]
mod chat_stream;
pub(super) use chat_stream::StreamState;
mod images;
mod jobs;
mod local_fields;
pub(crate) mod realtime;
#[cfg(test)]
mod tests;
