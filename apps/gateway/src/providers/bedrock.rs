//! Bedrock Converse adapter. AWS owns endpoint resolution, signing and wire decoding.
//! Prompt-bearing and credential-bearing state deliberately does not implement Debug.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use async_trait::async_trait;
use aws_sdk_bedrockruntime::{
    Client,
    config::{Region, retry::RetryConfig, timeout::TimeoutConfig},
    error::{ProvideErrorMetadata, SdkError},
    types as aws,
};
use aws_smithy_types::{Document, Number};
use serde_json::Value;
use tokio::sync::Mutex;

use super::ProviderAdapter;
use crate::inference::{error::InferenceError, types::*};

#[path = "bedrock/auth.rs"]
pub(crate) mod auth;
#[path = "bedrock/complete.rs"]
mod complete;
use complete::*;
#[cfg(test)]
#[path = "bedrock/encode_property_tests.rs"]
mod encode_property_tests;
#[path = "bedrock/images.rs"]
mod images;
#[path = "bedrock/stream.rs"]
mod stream;
#[cfg(test)]
#[path = "bedrock/tests.rs"]
mod tests;
#[path = "bedrock/transport.rs"]
mod transport;
#[path = "bedrock/wire.rs"]
mod wire;

type Result<T> = std::result::Result<T, InferenceError>;
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const MAX_BLOCKS: usize = 256;
const MAX_TOOLS: usize = 128;

pub struct BedrockAdapter {
    http: aws_smithy_runtime_api::client::http::SharedHttpClient,
    streaming_http: aws_smithy_runtime_api::client::http::SharedHttpClient,
    clients: Mutex<BTreeMap<String, Client>>,
    timeouts: transport::Timeouts,
    /// Profile and endpoint allowlists, read from the environment at startup.
    policy: auth::Policy,
    #[cfg(test)]
    test_client: Option<Client>,
}

impl BedrockAdapter {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            http: transport::client(transport::Timeouts::default(), false)?,
            streaming_http: transport::client(transport::Timeouts::default(), true)?,
            timeouts: transport::Timeouts::default(),
            clients: Mutex::new(BTreeMap::new()),
            policy: auth::Policy::from_env(),
            #[cfg(test)]
            test_client: None,
        })
    }

    /// A real adapter whose client is pinned to a local mock HTTP endpoint
    /// with fixed, non-production test credentials. For cross-module
    /// integration tests (e.g. `inference::admission_validation_tests`)
    /// that need the real adapter without reaching AWS; `bedrock::tests`
    /// builds the same shape inline since it already has field access.
    #[cfg(test)]
    pub(crate) fn for_test(base: &str) -> Self {
        Self::for_test_with_timeouts(base, transport::Timeouts::default())
    }

    #[cfg(test)]
    fn for_test_transport(
        base: &str,
        http: aws_smithy_runtime_api::client::http::SharedHttpClient,
    ) -> Self {
        Self::for_test_clients(base, transport::Timeouts::default(), http.clone(), http)
    }

    #[cfg(test)]
    fn for_test_with_timeouts(base: &str, timeouts: transport::Timeouts) -> Self {
        Self::for_test_clients(
            base,
            timeouts,
            transport::client(timeouts, false).unwrap(),
            transport::client(timeouts, true).unwrap(),
        )
    }

    async fn client(&self, plan: &auth::Plan) -> Client {
        #[cfg(test)]
        if let Some(client) = &self.test_client {
            return client.clone();
        }
        let key = plan.key();
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&key) {
            return client.clone();
        }
        let credentials = auth::credentials(&plan.auth, &plan.region).await;
        #[expect(clippy::disallowed_methods, reason = "Bedrock uses its service config")]
        let client = Client::from_conf(self.service_config(plan, credentials));
        // Bound cache cardinality even if catalog configuration changes repeatedly. Evicting
        // keeps a reused client (and its credential cache) for the current configuration.
        if clients.len() >= 64 {
            clients.pop_first();
        }
        clients.insert(key, client.clone());
        client
    }

    /// Built directly rather than copying a shared SDK configuration: environment/profile
    /// endpoint overrides (including AWS_ENDPOINT_URL_BEDROCK_RUNTIME) cannot be inherited.
    /// Only an allowlisted connection endpoint replaces the regional endpoint.
    fn service_config(
        &self,
        plan: &auth::Plan,
        credentials: aws_sdk_bedrockruntime::config::SharedCredentialsProvider,
    ) -> aws_sdk_bedrockruntime::Config {
        let mut builder = aws_sdk_bedrockruntime::Config::builder();
        builder.set_endpoint_url(plan.endpoint.clone());
        #[expect(
            clippy::disallowed_methods,
            reason = "Bedrock installs the gateway transport"
        )]
        let config = builder
            .behavior_version(auth::sdk_behavior_version())
            .region(Region::new(plan.region.clone()))
            .credentials_provider(credentials)
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            // Sign with this host's clock only: a response's Date header must
            // not move the signing time (the SDK's skew correction is on by
            // default since the 1.15x generation, #456).
            .disable_clock_skew_correction(true)
            // Cap credential loading at 5 s independently of the SDK's
            // operation timeouts.
            .identity_cache(
                aws_sdk_bedrockruntime::config::IdentityCache::lazy()
                    .load_timeout(Duration::from_secs(5))
                    .build(),
            )
            .timeout_config(self.operation_timeouts())
            .http_client(self.http.clone())
            .build();
        config
    }
    fn operation_timeouts(&self) -> TimeoutConfig {
        TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(self.timeouts.read)
            .operation_timeout(self.timeouts.total)
            .build()
    }
}

#[async_trait]
impl ProviderAdapter for BedrockAdapter {
    fn id(&self) -> &'static str {
        "bedrock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            image_input: true,
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn generation_output_limit(&self) -> u32 {
        i32::MAX as u32
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        super::capabilities::serves(self.id(), protocol)
    }
    /// Dry-runs the real, pure `encode` and discards the converted input:
    /// pre-admission validation can never drift from what `execute` would
    /// actually reject, and this never resolves credentials or touches the
    /// network (`execute` only calls `encode` before any of that).
    fn validate_chat_request(&self, request: &ChatRequest) -> Result<()> {
        encode_with(request, false).map(|_| ())
    }
    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        let plan = auth::Plan::new(target, &self.policy).ok_or(InferenceError::Configuration)?;
        let input = encode(&request)?; // Validate before any credential discovery/network work.
        let client = self.client(&plan).await;
        if request.stream {
            #[expect(
                clippy::disallowed_methods,
                reason = "Bedrock streaming installs the gateway transport"
            )]
            let operation = client
                .converse_stream()
                .model_id(&target.upstream_model)
                .set_messages(Some(input.messages))
                .set_system(input.system)
                .inference_config(input.inference)
                .set_tool_config(input.tools)
                .set_output_config(input.output)
                // Operation settings and the credential cache stay shared. Only
                // the streaming HTTP body omits the fixed total lifetime cap.
                .customize()
                .config_override(
                    aws_sdk_bedrockruntime::Config::builder()
                        .http_client(self.streaming_http.clone()),
                );
            let output = operation
                .send()
                .await
                .map_err(|e| sdk_messages_error(e, request.messages_options.is_some()))?;
            Ok(ProviderOutput::Stream(stream::decode(
                output.stream,
                request.response_limit(),
                request.messages_options.is_some(),
            )))
        } else {
            let output = client
                .converse()
                .model_id(&target.upstream_model)
                .set_messages(Some(input.messages))
                .set_system(input.system)
                .inference_config(input.inference)
                .set_tool_config(input.tools)
                .set_output_config(input.output)
                .send()
                .await
                .map_err(|e| sdk_messages_error(e, request.messages_options.is_some()))?;
            Ok(super::messages::thinking_display(
                &request,
                ProviderOutput::Complete(decode_complete_limit(
                    output,
                    request.response_limit(),
                    request.messages_options.is_some(),
                )?),
            ))
        }
    }
}

struct Input {
    messages: Vec<aws::Message>,
    system: Option<Vec<aws::SystemContentBlock>>,
    inference: aws::InferenceConfiguration,
    tools: Option<aws::ToolConfiguration>,
    output: Option<aws::OutputConfig>,
}

fn encode(request: &ChatRequest) -> Result<Input> {
    encode_with(request, true)
}

/// The Converse input; with `decode` false (validation) images keep no bytes.
fn encode_with(request: &ChatRequest, decode: bool) -> Result<Input> {
    super::messages::validate(request, super::messages::ProviderClass::Bedrock)?;
    check_request(request)?;
    super::image_body::check_limits(request, &images::LIMITS)?;
    let mut size = 0usize;
    let (names, tools) = tool_specs(request, &mut size)?;
    let choice = tool_choice(request, &names, tools.is_empty())?;
    let tools = tool_config(tools, choice)?;
    let output = output_config(request, &mut size)?;
    let mut turns = Turns {
        messages: Vec::new(),
        system: Vec::new(),
        pending: BTreeSet::new(),
        seen_ids: BTreeSet::new(),
        size,
        decode,
    };
    for message in &request.messages {
        turns.push(message)?;
    }
    // Text, tool arguments, tool specifications and the output schema.
    super::image_body::check_request_size(request, turns.size, &images::LIMITS)?;
    let mut input = turns.finish(request, tools)?;
    input.output = output;
    Ok(input)
}

/// A Messages `output_config.format` as Converse `outputConfig.textFormat`.
/// Converse takes the schema as a JSON string and an optional name and
/// description; Messages has neither, so none is sent. Whether the model can
/// constrain its output is Bedrock's decision: an unsupported model fails with
/// a validation error. Effort and task budgets are not mapped here (see
/// `providers::messages::validate`), so nothing else is sent.
/// The schema string counts toward the request-size bound in `size`, as tool
/// schemas do (`tool_specs`).
fn output_config(request: &ChatRequest, size: &mut usize) -> Result<Option<aws::OutputConfig>> {
    let Some(format) = request
        .messages_options
        .as_ref()
        .and_then(|o| o.output_config.as_ref())
        .and_then(|c| c.get("format"))
    else {
        return Ok(None);
    };
    let refuse = || InferenceError::InvalidField("output_config.format");
    // The Messages grammar admits only `{"type":"json_schema","schema":{...}}`.
    if format["type"] != "json_schema" || !format["schema"].is_object() {
        return Err(refuse());
    }
    let schema = serde_json::to_string(&format["schema"]).map_err(|_| refuse())?;
    *size = size.saturating_add(schema.len());
    if *size > OUTPUT_LIMIT {
        return Err(invalid());
    }
    let definition = aws::JsonSchemaDefinition::builder()
        .schema(schema)
        .build()
        .map_err(|_| refuse())?;
    let format = aws::OutputFormat::builder()
        .r#type(aws::OutputFormatType::JsonSchema)
        .structure(aws::OutputFormatStructure::JsonSchema(definition))
        .build()
        .map_err(|_| refuse())?;
    Ok(Some(
        aws::OutputConfig::builder().text_format(format).build(),
    ))
}

fn invalid() -> InferenceError {
    InferenceError::InvalidRequest
}

/// Request-wide bounds checked before any tool or message is converted.
fn check_request(request: &ChatRequest) -> Result<()> {
    if request.messages.is_empty()
        || request.messages.len() > 1024
        || request.tools.len() > MAX_TOOLS
        || request
            .temperature
            .is_some_and(|t| !t.is_finite() || !(0.0..=1.0).contains(&t))
        || request
            .max_output_tokens
            .is_some_and(|n| n == 0 || n > i32::MAX as u32)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Converts the tool definitions in order, returning their names and specs and adding
/// their encoded size to `size`.
fn tool_specs(
    request: &ChatRequest,
    size: &mut usize,
) -> Result<(BTreeSet<String>, Vec<aws::Tool>)> {
    let mut names = BTreeSet::new();
    let mut tools = Vec::new();
    for tool in request.tools.iter() {
        if !identifier(&tool.name)
            || !names.insert(tool.name.clone())
            || !tool.parameters.is_object()
            || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(invalid());
        }
        *size += tool.name.len()
            + tool.description.as_ref().map_or(0, String::len)
            + serde_json::to_vec(&tool.parameters)
                .map_err(|_| invalid())?
                .len();
        if *size > OUTPUT_LIMIT {
            return Err(invalid());
        }
        tools.push(aws::Tool::ToolSpec(
            aws::ToolSpecification::builder()
                .name(&tool.name)
                .set_description(tool.description.clone())
                .input_schema(aws::ToolInputSchema::Json(document(&tool.parameters)?))
                // Strict support is model-specific and evaluated by Bedrock: an
                // unsupported model refuses the request rather than weakening
                // the schema. `false` is Converse's default and is not sent.
                .set_strict((tool.strict == Some(true)).then_some(true))
                .build()
                .map_err(|_| invalid())?,
        ));
    }
    Ok((names, tools))
}

fn tool_choice(
    request: &ChatRequest,
    names: &BTreeSet<String>,
    no_tools: bool,
) -> Result<Option<aws::ToolChoice>> {
    Ok(match &request.tool_choice {
        None => None,
        Some(_) if no_tools => return Err(invalid()),
        Some(ToolChoice::None) => return Err(InferenceError::Unsupported),
        Some(ToolChoice::Auto) => Some(aws::ToolChoice::Auto(
            aws::AutoToolChoice::builder().build(),
        )),
        Some(ToolChoice::Required) => {
            Some(aws::ToolChoice::Any(aws::AnyToolChoice::builder().build()))
        }
        Some(ToolChoice::Function(name)) => {
            if !names.contains(name) {
                return Err(invalid());
            }
            Some(aws::ToolChoice::Tool(
                aws::SpecificToolChoice::builder()
                    .name(name)
                    .build()
                    .map_err(|_| invalid())?,
            ))
        }
    })
}

fn tool_config(
    tools: Vec<aws::Tool>,
    choice: Option<aws::ToolChoice>,
) -> Result<Option<aws::ToolConfiguration>> {
    Ok(if tools.is_empty() {
        None
    } else {
        Some(
            aws::ToolConfiguration::builder()
                .set_tools(Some(tools))
                .set_tool_choice(choice)
                .build()
                .map_err(|_| invalid())?,
        )
    })
}

/// The conversation as converted so far: Bedrock messages, system blocks, tool calls
/// still awaiting a result, every tool call id seen and the running encoded size.
struct Turns {
    messages: Vec<aws::Message>,
    system: Vec<aws::SystemContentBlock>,
    pending: BTreeSet<String>,
    seen_ids: BTreeSet<String>,
    size: usize,
    /// Images carry their bytes (`false` only for validation).
    decode: bool,
}

impl Turns {
    fn push(&mut self, message: &Message) -> Result<()> {
        self.size = self
            .size
            .checked_add(message.content.as_ref().map_or(0, Content::text_len))
            .ok_or_else(invalid)?;
        if self.size > OUTPUT_LIMIT {
            return Err(invalid());
        }
        if message.role == Role::Developer {
            return Err(InferenceError::Unsupported);
        }
        if message.role == Role::System {
            return self.push_system(message);
        }
        if message.role != Role::Tool && !self.pending.is_empty() {
            return Err(invalid());
        }
        if message.role != Role::Tool && message.tool_call_id.is_some() {
            return Err(invalid());
        }
        if message.role != Role::Assistant && !message.tool_calls.is_empty() {
            return Err(invalid());
        }
        let role = if message.role == Role::Assistant {
            aws::ConversationRole::Assistant
        } else {
            aws::ConversationRole::User
        };
        let content = if message.role == Role::Tool {
            self.tool_result(message)?
        } else {
            self.text_and_calls(message)?
        };
        if content.is_empty() {
            return Err(invalid());
        }
        self.append(message, role, content)
    }

    fn push_system(&mut self, message: &Message) -> Result<()> {
        if !self.messages.is_empty()
            || !message.tool_calls.is_empty()
            || message.tool_call_id.is_some()
        {
            return Err(invalid());
        }
        let text = message
            .content
            .as_ref()
            .map(Content::require_text)
            .transpose()?
            .filter(|s| !s.is_empty())
            .ok_or_else(invalid)?;
        self.system
            .push(aws::SystemContentBlock::Text(text.to_owned()));
        Ok(())
    }

    fn tool_result(&mut self, message: &Message) -> Result<Vec<aws::ContentBlock>> {
        let mut content = Vec::new();
        let id = message.tool_call_id.as_ref().ok_or_else(invalid)?;
        if !self.pending.remove(id) {
            return Err(invalid());
        }
        // Canonical tool result content is a string, preserved verbatim (JSON
        // strings included; re-emitting would alter them), or text and images.
        let blocks = match message.content.as_ref().ok_or_else(invalid)? {
            Content::Parts(parts) => images::tool_result_blocks(parts, self.decode)?,
            text => vec![aws::ToolResultContentBlock::Text(
                text.require_text()?.to_owned(),
            )],
        };
        content.push(aws::ContentBlock::ToolResult(
            aws::ToolResultBlock::builder()
                .tool_use_id(id)
                .set_content(Some(blocks))
                // Bedrock evaluates status support for the selected model or profile.
                .set_status(
                    message
                        .tool_result_is_error
                        .then_some(aws::ToolResultStatus::Error),
                )
                .build()
                .map_err(|_| invalid())?,
        ));
        Ok(content)
    }

    fn text_and_calls(&mut self, message: &Message) -> Result<Vec<aws::ContentBlock>> {
        let mut content = Vec::new();
        if let Some(Content::Parts(parts)) = &message.content {
            content.extend(images::blocks(parts, self.decode)?);
        } else if let Some(text) = message
            .content
            .as_ref()
            .map(Content::require_text)
            .transpose()?
            && !text.is_empty()
        {
            content.push(aws::ContentBlock::Text(text.to_owned()));
        }
        if message.tool_calls.len() > MAX_TOOLS {
            return Err(invalid());
        }
        for call in &message.tool_calls {
            content.push(self.tool_use(call)?);
        }
        Ok(content)
    }

    fn tool_use(&mut self, call: &ToolCall) -> Result<aws::ContentBlock> {
        if !identifier(&call.id)
            || !identifier(&call.name)
            || !self.seen_ids.insert(call.id.clone())
        {
            return Err(invalid());
        }
        let args =
            crate::inference::json_weight::parse_document(&call.arguments).ok_or_else(invalid)?;
        if !args.is_object() {
            return Err(invalid());
        }
        self.size = self
            .size
            .checked_add(call.arguments.len())
            .ok_or_else(invalid)?;
        self.pending.insert(call.id.clone());
        Ok(aws::ContentBlock::ToolUse(
            aws::ToolUseBlock::builder()
                .tool_use_id(&call.id)
                .name(&call.name)
                .input(document(&args)?)
                .build()
                .map_err(|_| invalid())?,
        ))
    }

    fn append(
        &mut self,
        message: &Message,
        role: aws::ConversationRole,
        content: Vec<aws::ContentBlock>,
    ) -> Result<()> {
        // Tool results from one assistant turn must be sent together as one user message.
        // Do not otherwise collapse user/assistant boundaries or alter turn ordering.
        if message.role == Role::Tool
            && self.messages.last().is_some_and(|m| {
                m.role == aws::ConversationRole::User
                    && m.content
                        .iter()
                        .all(|b| matches!(b, aws::ContentBlock::ToolResult(_)))
            })
        {
            self.messages.last_mut().unwrap().content.extend(content);
        } else {
            self.messages.push(
                aws::Message::builder()
                    .role(role)
                    .set_content(Some(content))
                    .build()
                    .map_err(|_| invalid())?,
            );
        }
        Ok(())
    }

    fn finish(self, request: &ChatRequest, tools: Option<aws::ToolConfiguration>) -> Result<Input> {
        let Self {
            messages,
            system,
            pending,
            size,
            ..
        } = self;
        if !pending.is_empty()
            || messages.is_empty()
            || messages[0].role != aws::ConversationRole::User
            || messages
                .last()
                .is_some_and(|m| m.role != aws::ConversationRole::User)
            || size > OUTPUT_LIMIT
        {
            return Err(invalid());
        }
        Ok(Input {
            messages,
            system: (!system.is_empty()).then_some(system),
            tools,
            inference: inference_config(request),
            output: None,
        })
    }
}

fn inference_config(request: &ChatRequest) -> aws::InferenceConfiguration {
    aws::InferenceConfiguration::builder()
        .set_temperature(request.temperature.map(|n| n as f32))
        .set_top_p(
            request
                .messages_options
                .as_ref()
                .and_then(|o| o.top_p)
                .map(|p| p as f32),
        )
        .set_stop_sequences(
            request
                .messages_options
                .as_ref()
                .and_then(|o| o.stop_sequences.clone()),
        )
        .set_max_tokens(request.max_output_tokens.map(|n| n as i32))
        .build()
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn document(value: &Value) -> Result<Document> {
    Ok(match value {
        Value::Null => Document::Null,
        Value::Bool(v) => Document::Bool(*v),
        Value::String(v) => Document::String(v.clone()),
        Value::Array(v) => Document::Array(v.iter().map(document).collect::<Result<_>>()?),
        Value::Object(v) => Document::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), document(v)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Number(v) => Document::Number(if let Some(n) = v.as_u64() {
            Number::PosInt(n)
        } else if let Some(n) = v.as_i64() {
            Number::NegInt(n)
        } else {
            Number::Float(v.as_f64().ok_or(InferenceError::InvalidRequest)?)
        }),
    })
}
fn json_document(value: &Document) -> Result<Value> {
    Ok(match value {
        Document::Null => Value::Null,
        Document::Bool(v) => Value::Bool(*v),
        Document::String(v) => Value::String(v.clone()),
        Document::Array(v) => Value::Array(v.iter().map(json_document).collect::<Result<_>>()?),
        Document::Object(v) => Value::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), json_document(v)?)))
                .collect::<Result<_>>()?,
        ),
        Document::Number(Number::PosInt(v)) => Value::from(*v),
        Document::Number(Number::NegInt(v)) => Value::from(*v),
        Document::Number(Number::Float(v)) => {
            Value::Number(serde_json::Number::from_f64(*v).ok_or(InferenceError::InvalidUpstream)?)
        }
    })
}
fn finish(reason: &aws::StopReason) -> Result<FinishReason> {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "max_tokens" => Ok(FinishReason::Length),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "guardrail_intervened" | "content_filtered" => Ok(FinishReason::ContentFilter),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
fn usage(value: &aws::TokenUsage) -> Result<Usage> {
    if value.total_tokens < 0
        || value.cache_read_input_tokens.is_some_and(|n| n < 0)
        || value.cache_write_input_tokens.is_some_and(|n| n < 0)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut wire =
        serde_json::json!({"inputTokens":value.input_tokens,"outputTokens":value.output_tokens});
    if let Some(n) = value.cache_read_input_tokens {
        wire["cacheReadInputTokens"] = n.into();
    }
    if let Some(n) = value.cache_write_input_tokens {
        wire["cacheWriteInputTokens"] = n.into();
    }
    if let Some(details) = &value.cache_details {
        wire["cacheDetails"] = Value::Array(
            details
                .iter()
                .map(|d| serde_json::json!({"ttl":d.ttl.as_str(),"inputTokens":d.input_tokens}))
                .collect(),
        );
    }
    super::metering::bedrock(&wire)
}
fn sdk_messages_error<E: ProvideErrorMetadata>(
    error: SdkError<E>,
    messages: bool,
) -> InferenceError {
    if messages && let Some(service) = error.as_service_error() {
        if matches!(
            service.code(),
            Some("ValidationException" | "validationException")
        ) && service.message().is_some_and(|m| {
            // Native validation envelope only; a quoted client phrase in an
            // unrelated diagnostic is not proof of context overflow.
            let message = m.to_ascii_lowercase();
            message.starts_with("input is too long for requested model")
                || message == "too many input tokens"
                || message.starts_with("too many input tokens:")
                || message == "prompt is too long"
                || message.starts_with("prompt is too long:")
        }) {
            return InferenceError::ContextLength;
        }
        if matches!(
            service.code(),
            Some("ServiceUnavailableException" | "ModelNotReadyException")
        ) {
            return InferenceError::Overloaded;
        }
    }
    sdk_error(error)
}
fn sdk_error<E: ProvideErrorMetadata>(error: SdkError<E>) -> InferenceError {
    if matches!(error, SdkError::TimeoutError(_)) {
        return InferenceError::Timeout;
    }
    if let Some(service) = error.as_service_error() {
        return service_error(service.code());
    }
    if matches!(error, SdkError::ResponseError(_)) {
        InferenceError::InvalidUpstream
    } else {
        InferenceError::UpstreamUnavailable
    }
}
fn service_error(code: Option<&str>) -> InferenceError {
    match code {
        Some(
            "AccessDeniedException"
            | "UnrecognizedClientException"
            | "InvalidSignatureException"
            | "ExpiredTokenException",
        ) => InferenceError::Configuration,
        Some("ThrottlingException" | "throttlingException" | "ServiceQuotaExceededException") => {
            InferenceError::Busy
        }
        Some("ModelTimeoutException" | "modelTimeoutException") => InferenceError::Timeout,
        Some("ValidationException" | "validationException" | "ResourceNotFoundException") => {
            InferenceError::UpstreamRejected
        }
        _ => InferenceError::UpstreamUnavailable,
    }
}
