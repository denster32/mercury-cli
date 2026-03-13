//! Mercury 2 and Mercury Edit API clients.
//!
//! This module provides typed, mockable clients for the Inception Labs
//! Mercury API surface:
//!
//! - [`Mercury2Client`] - chat completions with strict JSON schema support
//! - [`MercuryEditClient`] - code editing (apply / complete / next)
//!
//! All public items carry doc comments, errors use [`thiserror`], and every
//! HTTP-facing method is exposed through a trait so callers can substitute
//! test doubles.

use std::env;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default base URL for the Mercury 2 chat completions endpoint.
const MERCURY2_BASE_URL: &str = "https://api.inceptionlabs.ai/v1/chat/completions";

/// Default model identifier sent in every Mercury 2 request.
const MERCURY2_MODEL: &str = "mercury-2";

/// Preferred environment variable for the Inception Labs API key.
pub const INCEPTION_API_KEY_ENV: &str = "INCEPTION_API_KEY";

/// Backward-compatible fallback environment variable for the API key.
pub const MERCURY_API_KEY_ENV: &str = "MERCURY_API_KEY";

/// Strict schema name for thermal-analysis responses.
const THERMAL_ASSESSMENT_SCHEMA_NAME: &str = "thermal_assessment_v1";

/// Strict schema name for planner responses.
pub const PLANNER_RESPONSE_SCHEMA_NAME: &str = "planner-response-v1";

/// Cost per 1 000 input tokens (USD) - Mercury 2 / Mercury Edit.
const COST_PER_1K_INPUT: f64 = 0.00025;

/// Cost per 1 000 cached input tokens (USD) - 10x cheaper.
const COST_PER_1K_CACHED_INPUT: f64 = 0.000025;

/// Cost per 1 000 output tokens (USD) - Mercury 2 / Mercury Edit.
const COST_PER_1K_OUTPUT: f64 = 0.00075;

/// Default model for Mercury Edit endpoints (apply / fim / next-edit).
const MERCURY_EDIT_MODEL: &str = "mercury-edit";

// ---------------------------------------------------------------------------
// Reasoning effort
// ---------------------------------------------------------------------------

/// Controls how much reasoning the model does before responding.
///
/// - `Instant` - near-zero latency, 0 reasoning tokens
/// - `Low` / `Medium` / `High` - increasing quality at the cost of latency
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Near-instant response with no chain-of-thought.
    Instant,
    /// Minimal reasoning.
    Low,
    /// Balanced reasoning (default).
    Medium,
    /// Maximum reasoning depth.
    High,
}

/// Constitutional prompt used to request a thermal-analysis JSON response.
pub const THERMAL_ANALYSIS_PROMPT: &str = r#"You are a code analysis agent in the Mercury CLI swarm.
Analyze the provided code and return a thermal assessment as JSON.
Your assessment must include:
- complexity_score (0-1): cyclomatic complexity normalized
- dependency_score (0-1): coupling density
- risk_score (0-1): likelihood of bugs or regressions
- churn_score (0-1): rate of recent changes
- suggested_action: one of [\"lock\", \"refactor\", \"test\", \"monitor\", \"ignore\"]
- reasoning: brief explanation
Respond ONLY with valid JSON, no markdown, no preamble."#;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur when talking to Mercury APIs.
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    /// An underlying HTTP / network error from `reqwest`.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The API returned a non-success status code.
    #[error("API returned status {status}: {body}")]
    ApiStatus {
        /// HTTP status code.
        status: u16,
        /// Response body (may be truncated).
        body: String,
    },

    /// Failed to deserialize a JSON response.
    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),

    /// A structured response violated the requested schema contract.
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    /// The caller's budget cap has been reached.
    #[error("budget exceeded: ${spent:.4} of ${limit:.4} limit")]
    BudgetExceeded {
        /// Total amount spent so far (USD).
        spent: f64,
        /// Configured budget ceiling (USD).
        limit: f64,
    },

    /// Retries exhausted without a successful response.
    #[error("max retries ({0}) exceeded")]
    MaxRetries(u32),

    /// The required API key environment variable is not set.
    #[error("missing API key: set {0} environment variable")]
    MissingApiKey(String),
}

// ---------------------------------------------------------------------------
// Shared payload / response types
// ---------------------------------------------------------------------------

/// Cumulative token and cost counters returned alongside every API response.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ApiUsage {
    /// Total tokens consumed by the request (prompt + completion).
    pub tokens_used: i64,
    /// Estimated cost in USD.
    pub cost_usd: f64,
}

/// A thermal assessment parsed from a Mercury 2 structured-JSON response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThermalAssessment {
    /// Cyclomatic complexity normalised to [0, 1].
    pub complexity_score: f64,
    /// Coupling / dependency density normalised to [0, 1].
    pub dependency_score: f64,
    /// Likelihood of bugs or regressions normalised to [0, 1].
    pub risk_score: f64,
    /// Rate of recent changes normalised to [0, 1].
    pub churn_score: f64,
    /// Recommended action: one of `lock`, `refactor`, `test`, `monitor`, `ignore`.
    pub suggested_action: String,
    /// Free-text explanation of the assessment.
    pub reasoning: String,
}

/// Domain payload for the Mercury Edit Apply endpoint (`/v1/apply/completions`).
///
/// The client wraps these fields in `<|original_code|>` and
/// `<|update_snippet|>` markup tags before sending. `update_snippet` must be
/// concrete replacement code or patch content, not a natural-language
/// instruction comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPayload {
    /// The original source code before any edits.
    pub original_code: String,
    /// Concrete code snippet or diff snippet to apply.
    pub update_snippet: String,
    /// Maximum tokens for the response.
    #[serde(default = "default_edit_max_tokens")]
    pub max_tokens: u32,
}

fn default_edit_max_tokens() -> u32 {
    8192
}

/// Domain payload for the Mercury Edit FIM endpoint (`/v1/fim/completions`).
///
/// Fill-In-the-Middle: provide the code before and after the cursor and
/// the model predicts what goes in between.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletePayload {
    /// Code before the cursor / insertion point.
    pub prompt: String,
    /// Code after the cursor / insertion point.
    #[serde(default)]
    pub suffix: String,
    /// Maximum tokens for the completion.
    #[serde(default = "default_fim_max_tokens")]
    pub max_tokens: u32,
}

fn default_fim_max_tokens() -> u32 {
    256
}

/// Domain payload for the Mercury Edit Next-Edit endpoint (`/v1/edit/completions`).
///
/// The client always emits the documented wrapper sections. To preserve source
/// compatibility for existing callers, file path remains out-of-band; callers
/// that have a path should use [`MercuryEditClient::next_edit_with_path`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextEditPayload {
    /// The full current file content.
    pub file_content: String,
    /// The specific code region to edit.
    #[serde(default)]
    pub code_to_edit: String,
    /// Optional cursor position within `code_to_edit`.
    #[serde(default)]
    pub cursor: String,
    /// Recently viewed snippets for context. Empty means no extra context.
    #[serde(default)]
    pub recent_snippets: String,
    /// Chronological unified diff history for prior edits.
    #[serde(default)]
    pub edit_history: String,
}

// ---------------------------------------------------------------------------
// Internal request / response shapes (OpenAI-compatible)
// ---------------------------------------------------------------------------

/// A single message in a chat completion request.
#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// `response_format` field for strict JSON-schema output.
#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: String,
    json_schema: JsonSchemaFormat,
}

/// Strict JSON-schema descriptor embedded under `response_format`.
#[derive(Debug, Serialize)]
struct JsonSchemaFormat {
    name: String,
    strict: bool,
    schema: Value,
}

/// OpenAI-compatible tool definition for Mercury chat requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDefinition,
}

/// Function metadata attached to a [`ToolDefinition`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolFunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

/// OpenAI-compatible `tool_choice` request parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String),
    Function(ToolChoiceFunction),
}

/// Select a specific function tool by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolChoiceFunction {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolChoiceFunctionName,
}

/// Function name wrapper for [`ToolChoiceFunction`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolChoiceFunctionName {
    pub name: String,
}

/// The top-level chat completion request body.

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
}

/// Token usage counters embedded in a chat completion response.
#[derive(Debug, Deserialize)]
struct UsageBlock {
    prompt_tokens: i64,
    completion_tokens: i64,
    #[allow(dead_code)]
    total_tokens: i64,
    /// Internal reasoning tokens (counted against max_tokens but not billed
    /// at completion rate).
    #[serde(default)]
    #[allow(dead_code)]
    reasoning_tokens: i64,
    /// Tokens served from Inception's input cache (10x cheaper).
    #[serde(default)]
    cached_input_tokens: i64,
}

/// A single choice inside a chat completion response.
#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

/// The message body of a chat choice.
#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_vec")]
    tool_calls: Vec<ToolCall>,
}

/// OpenAI-compatible tool call emitted by the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

/// Function call metadata emitted in a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl ToolCallFunction {
    /// Parse JSON arguments emitted for this tool call.
    pub fn parse_arguments(&self) -> Result<Value, ApiError> {
        Ok(serde_json::from_str(&self.arguments)?)
    }
}

/// The top-level chat completion response.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    usage: Option<UsageBlock>,
}

fn deserialize_null_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Fill-In-the-Middle request body for `/v1/fim/completions`.
#[derive(Debug, Serialize)]
struct FimRequest {
    model: String,
    prompt: String,
    suffix: String,
    max_tokens: u32,
    temperature: f64,
}

/// A single choice in a FIM completion response.
#[derive(Debug, Deserialize)]
struct FimChoice {
    text: String,
}

/// Top-level FIM completion response.
#[derive(Debug, Deserialize)]
struct FimResponse {
    choices: Vec<FimChoice>,
    usage: Option<UsageBlock>,
}

// ---------------------------------------------------------------------------
// Shared transport helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TransportState {
    cumulative_cost: AtomicU64,
    cumulative_tokens: AtomicI64,
    next_allowed_request_at: Mutex<Instant>,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            cumulative_cost: AtomicU64::new(0),
            cumulative_tokens: AtomicI64::new(0),
            next_allowed_request_at: Mutex::new(Instant::now()),
        }
    }
}

impl TransportState {
    async fn throttle(&self, min_request_interval_ms: u64) {
        if min_request_interval_ms == 0 {
            return;
        }

        let wait_for = {
            let mut next_allowed = self.next_allowed_request_at.lock().await;
            let now = Instant::now();
            let scheduled = if *next_allowed > now {
                *next_allowed
            } else {
                now
            };
            *next_allowed = scheduled + Duration::from_millis(min_request_interval_ms);
            scheduled.saturating_duration_since(now)
        };

        if wait_for.as_millis() > 0 {
            tokio::time::sleep(wait_for).await;
        }
    }

    fn record_usage(&self, usage: &ApiUsage, budget_limit: Option<f64>) -> Result<(), ApiError> {
        self.cumulative_tokens
            .fetch_add(usage.tokens_used, Ordering::AcqRel);

        loop {
            let old_bits = self.cumulative_cost.load(Ordering::Relaxed);
            let old = f64::from_bits(old_bits);
            let new = old + usage.cost_usd;
            let new_bits = new.to_bits();
            if self
                .cumulative_cost
                .compare_exchange_weak(old_bits, new_bits, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                if let Some(limit) = budget_limit {
                    if new > limit {
                        return Err(ApiError::BudgetExceeded { spent: new, limit });
                    }
                }
                return Ok(());
            }
        }
    }

    fn cumulative_cost(&self) -> f64 {
        f64::from_bits(self.cumulative_cost.load(Ordering::Relaxed))
    }

    fn cumulative_tokens(&self) -> i64 {
        self.cumulative_tokens.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy)]
struct TransportConfig {
    retry_limit: u32,
    backoff_base_ms: u64,
    budget_limit: Option<f64>,
    min_request_interval_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct TransportRuntime<'a> {
    config: TransportConfig,
    state: &'a TransportState,
}

fn api_key_env_hint() -> String {
    format!("{INCEPTION_API_KEY_ENV} (preferred) or {MERCURY_API_KEY_ENV}")
}

pub fn resolve_api_key(configured_env: &str) -> Result<String, ApiError> {
    let mut env_names = Vec::new();
    let configured_env = configured_env.trim();
    if !configured_env.is_empty() {
        env_names.push(configured_env);
    }

    for env_name in [INCEPTION_API_KEY_ENV, MERCURY_API_KEY_ENV] {
        if !env_names.contains(&env_name) {
            env_names.push(env_name);
        }
    }

    for env_name in env_names {
        if let Ok(value) = env::var(env_name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }

    Err(ApiError::MissingApiKey(api_key_env_hint()))
}

fn strict_json_schema_response_format(name: &str, schema: Value) -> ResponseFormat {
    ResponseFormat {
        kind: "json_schema".to_string(),
        json_schema: JsonSchemaFormat {
            name: name.to_string(),
            strict: true,
            schema,
        },
    }
}

fn thermal_assessment_json_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "complexity_score": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "dependency_score": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "risk_score": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "churn_score": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
            "suggested_action": {
                "type": "string",
                "enum": ["lock", "refactor", "test", "monitor", "ignore"]
            },
            "reasoning": { "type": "string", "minLength": 1 }
        },
        "required": [
            "complexity_score",
            "dependency_score",
            "risk_score",
            "churn_score",
            "suggested_action",
            "reasoning"
        ],
        "additionalProperties": false
    })
}

/// Strict JSON schema for the planner response contract expected by Mercury CLI.
pub fn planner_response_json_schema_v1() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Mercury CLI Planner Response v1",
        "type": "object",
        "properties": {
            "schema_version": {
                "type": "string",
                "const": PLANNER_RESPONSE_SCHEMA_NAME
            },
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "file_path": { "type": "string", "minLength": 1 },
                        "instruction": { "type": "string", "minLength": 1 },
                        "priority": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                        "estimated_tokens": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["file_path", "instruction", "priority", "estimated_tokens"],
                    "additionalProperties": false
                }
            },
            "assessments": {
                "type": "array",
                "items": thermal_assessment_json_schema()
            }
        },
        "required": ["schema_version", "steps", "assessments"],
        "additionalProperties": false
    })
}

fn wrap_tag(tag: &str, content: &str) -> String {
    format!("<|{tag}|>\n{content}\n<|/{tag}|>")
}

fn format_apply_prompt(payload: &EditPayload) -> String {
    [
        wrap_tag("original_code", &payload.original_code),
        wrap_tag("update_snippet", &payload.update_snippet),
    ]
    .join("\n\n")
}

fn format_next_edit_prompt(current_file_path: &str, payload: &NextEditPayload) -> String {
    let code_to_edit_section = [
        payload.code_to_edit.as_str(),
        &wrap_tag("cursor", &payload.cursor),
    ]
    .join("\n");
    let current_file_section = [
        format!("current_file_path: {current_file_path}"),
        payload.file_content.clone(),
        wrap_tag("code_to_edit", &code_to_edit_section),
    ]
    .join("\n");

    [
        wrap_tag("recently_viewed_code_snippets", &payload.recent_snippets),
        wrap_tag("current_file_content", &current_file_section),
        wrap_tag("edit_diff_history", &payload.edit_history),
    ]
    .join("\n\n")
}

fn is_retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn retry_delay_ms(config: TransportConfig, attempts: u32) -> u64 {
    config
        .backoff_base_ms
        .saturating_mul(2u64.saturating_pow(attempts.saturating_sub(1)))
}

fn estimate_cost(usage: &UsageBlock) -> f64 {
    let uncached_input = (usage.prompt_tokens - usage.cached_input_tokens).max(0);
    let cached_cost = (usage.cached_input_tokens as f64 / 1000.0) * COST_PER_1K_CACHED_INPUT;
    let input_cost = (uncached_input as f64 / 1000.0) * COST_PER_1K_INPUT;
    let output_cost = (usage.completion_tokens as f64 / 1000.0) * COST_PER_1K_OUTPUT;
    cached_cost + input_cost + output_cost
}

async fn post_json_with_retry_raw(
    http: &Client,
    api_key: &str,
    url: &str,
    payload: &(impl Serialize + Sync),
    runtime: TransportRuntime<'_>,
    log_label: &str,
) -> Result<Vec<u8>, ApiError> {
    let mut attempts: u32 = 0;
    let TransportRuntime { config, state } = runtime;

    loop {
        attempts += 1;
        state.throttle(config.min_request_interval_ms).await;

        let response = match http
            .post(url)
            .bearer_auth(api_key)
            .json(payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if attempts <= config.retry_limit {
                    let delay = retry_delay_ms(config, attempts);
                    warn!(
                        attempt = attempts,
                        delay_ms = delay,
                        endpoint = log_label,
                        error = %error,
                        "transport error, retrying request"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                return Err(ApiError::Http(error));
            }
        };
        let status = response.status().as_u16();

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();

            if is_retryable(status) && attempts <= config.retry_limit {
                let delay = retry_delay_ms(config, attempts);
                warn!(
                    status,
                    attempt = attempts,
                    delay_ms = delay,
                    endpoint = log_label,
                    "retryable API error, backing off"
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }

            if attempts > config.retry_limit && is_retryable(status) {
                return Err(ApiError::MaxRetries(config.retry_limit));
            }

            return Err(ApiError::ApiStatus { status, body });
        }

        let raw = match response.bytes().await {
            Ok(raw) => raw.to_vec(),
            Err(error) => {
                if attempts <= config.retry_limit {
                    let delay = retry_delay_ms(config, attempts);
                    warn!(
                        attempt = attempts,
                        delay_ms = delay,
                        endpoint = log_label,
                        error = %error,
                        "failed to read response body, retrying request"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                return Err(ApiError::Http(error));
            }
        };

        return Ok(raw);
    }
}

async fn post_json_with_retry_parsed<T, P>(
    http: &Client,
    api_key: &str,
    url: &str,
    payload: &(impl Serialize + Sync),
    runtime: TransportRuntime<'_>,
    log_label: &str,
    mut parse: P,
) -> Result<T, ApiError>
where
    P: FnMut(&[u8]) -> Result<T, ApiError>,
{
    let mut attempts: u32 = 0;
    let config = runtime.config;

    loop {
        attempts += 1;
        let raw = post_json_with_retry_raw(http, api_key, url, payload, runtime, log_label).await?;

        match parse(&raw) {
            Ok(parsed) => return Ok(parsed),
            Err(ApiError::JsonParse(error)) if attempts <= config.retry_limit => {
                let delay = retry_delay_ms(config, attempts);
                warn!(
                    attempt = attempts,
                    delay_ms = delay,
                    endpoint = log_label,
                    response_bytes = raw.len(),
                    error = %error,
                    "JSON parse error, retrying request"
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn parse_chat_response_with_tools(
    raw: &[u8],
) -> Result<(String, Vec<ToolCall>, ApiUsage), ApiError> {
    let resp: ChatResponse = serde_json::from_slice(raw)?;
    let (content, tool_calls) = resp
        .choices
        .into_iter()
        .next()
        .map(|c| (c.message.content.unwrap_or_default(), c.message.tool_calls))
        .unwrap_or_default();
    let usage = match resp.usage {
        Some(u) => ApiUsage {
            tokens_used: u.prompt_tokens + u.completion_tokens,
            cost_usd: estimate_cost(&u),
        },
        None => ApiUsage::default(),
    };
    Ok((content, tool_calls, usage))
}

fn parse_chat_response(raw: &[u8]) -> Result<(String, ApiUsage), ApiError> {
    let (content, _tool_calls, usage) = parse_chat_response_with_tools(raw)?;
    Ok((content, usage))
}

fn parse_fim_response(raw: &[u8]) -> Result<(String, ApiUsage), ApiError> {
    let resp: FimResponse = serde_json::from_slice(raw)?;
    let text = resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.text)
        .unwrap_or_default();
    let usage = match resp.usage {
        Some(u) => ApiUsage {
            tokens_used: u.prompt_tokens + u.completion_tokens,
            cost_usd: estimate_cost(&u),
        },
        None => ApiUsage::default(),
    };
    Ok((text, usage))
}

// ---------------------------------------------------------------------------
// Traits (for mockability)
// ---------------------------------------------------------------------------

/// Trait abstracting Mercury 2 chat completions so callers can inject fakes.
pub trait Mercury2Api: Send + Sync {
    /// Send a chat completion request and return the assistant reply together
    /// with usage information.
    fn chat(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> impl std::future::Future<Output = Result<(String, ApiUsage), ApiError>> + Send;

    /// Convenience wrapper that requests structured JSON output and
    /// deserializes the reply into a [`ThermalAssessment`].
    fn chat_json(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> impl std::future::Future<Output = Result<(ThermalAssessment, ApiUsage), ApiError>> + Send;

    /// Request a strict JSON-schema response and return the parsed value.
    fn chat_json_schema_value(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        schema_name: &str,
        schema: Value,
    ) -> impl std::future::Future<Output = Result<(Value, ApiUsage), ApiError>> + Send;

    /// Send a chat completion request with OpenAI-compatible tool definitions.
    ///
    /// The default implementation falls back to plain chat so existing mocks
    /// can ignore tool support until they need to exercise the tool loop.
    fn chat_with_tools(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
    ) -> impl std::future::Future<Output = Result<(String, Vec<ToolCall>, ApiUsage), ApiError>> + Send
    {
        async move {
            let _ = (tools, tool_choice);
            let (content, usage) = self.chat(system, user, max_tokens).await?;
            Ok((content, Vec::new(), usage))
        }
    }
}

/// Trait abstracting Mercury Edit operations so callers can inject fakes.
pub trait MercuryEditApi: Send + Sync {
    /// Apply a concrete replacement snippet or patch to a code region.
    fn apply(
        &self,
        payload: &EditPayload,
    ) -> impl std::future::Future<Output = Result<(String, ApiUsage), ApiError>> + Send;

    /// Request an autocompletion at the given cursor position.
    fn complete(
        &self,
        payload: &CompletePayload,
    ) -> impl std::future::Future<Output = Result<(String, ApiUsage), ApiError>> + Send;

    /// Suggest the next logical edit based on prior history.
    fn next_edit(
        &self,
        payload: &NextEditPayload,
    ) -> impl std::future::Future<Output = Result<(String, ApiUsage), ApiError>> + Send;

    /// Suggest the next logical edit while supplying the current file path.
    fn next_edit_with_path(
        &self,
        current_file_path: &str,
        payload: &NextEditPayload,
    ) -> impl std::future::Future<Output = Result<(String, ApiUsage), ApiError>> + Send;
}

// ---------------------------------------------------------------------------
// Mercury2Client
// ---------------------------------------------------------------------------

/// Client for the Mercury 2 chat completions API.
///
/// Supports strict JSON-schema output, exponential-backoff retries on
/// transient errors (HTTP 429 / 5xx), and shared cost/token accounting.
///
/// `Debug` is implemented manually because [`reqwest::Client`] does not
/// derive it.
pub struct Mercury2Client {
    http: Client,
    api_key: String,
    base_url: String,
    model: String,
    /// Maximum number of retry attempts for transient errors.
    pub retry_limit: u32,
    /// Base delay in milliseconds for exponential backoff.
    pub backoff_base_ms: u64,
    /// Optional per-session budget ceiling (USD). `None` means unlimited.
    pub budget_limit: Option<f64>,
    /// Minimum delay between outbound requests from this client.
    pub min_request_interval_ms: u64,
    /// Optional reasoning effort level for Mercury 2.
    pub reasoning_effort: Option<ReasoningEffort>,
    transport_state: TransportState,
}

impl Mercury2Client {
    /// Create a new client, reading the API key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::MissingApiKey`] if neither `INCEPTION_API_KEY`
    /// nor `MERCURY_API_KEY` is set.
    pub fn from_env() -> Result<Self, ApiError> {
        Ok(Self::new(resolve_api_key("")?))
    }

    /// Create a new client with an explicit API key.
    pub fn new(api_key: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            base_url: MERCURY2_BASE_URL.to_string(),
            model: MERCURY2_MODEL.to_string(),
            retry_limit: 3,
            backoff_base_ms: 500,
            budget_limit: None,
            min_request_interval_ms: 0,
            reasoning_effort: None,
            transport_state: TransportState::default(),
        }
    }

    /// Override the base URL (useful for integration tests against a local stub).
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    /// Override the model identifier.
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Set retry parameters.
    pub fn with_retries(mut self, limit: u32, backoff_base_ms: u64) -> Self {
        self.retry_limit = limit;
        self.backoff_base_ms = backoff_base_ms;
        self
    }

    /// Set a hard budget ceiling in USD.
    pub fn with_budget(mut self, limit_usd: f64) -> Self {
        self.budget_limit = Some(limit_usd);
        self
    }

    /// Enforce a minimum delay between outbound requests from this client.
    pub fn with_request_spacing(mut self, min_request_interval_ms: u64) -> Self {
        self.min_request_interval_ms = min_request_interval_ms;
        self
    }

    /// Set the reasoning effort level (instant / low / medium / high).
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Return the cumulative cost spent by this client instance (USD).
    pub fn cumulative_cost(&self) -> f64 {
        self.transport_state.cumulative_cost()
    }

    /// Return the cumulative tokens consumed by this client instance.
    pub fn cumulative_tokens(&self) -> i64 {
        self.transport_state.cumulative_tokens()
    }

    /// Send a chat completion request with a strict JSON schema and deserialize it.
    pub async fn chat_with_json_schema<T: DeserializeOwned>(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        schema_name: &str,
        schema: Value,
    ) -> Result<(T, ApiUsage), ApiError> {
        let mut attempts: u32 = 0;

        loop {
            attempts += 1;
            let (raw, usage) = self
                .do_chat(
                    system,
                    user,
                    max_tokens,
                    Some(strict_json_schema_response_format(
                        schema_name,
                        schema.clone(),
                    )),
                )
                .await?;

            match serde_json::from_str(&raw) {
                Ok(parsed) => return Ok((parsed, usage)),
                Err(error) if attempts <= self.retry_limit => {
                    let delay = retry_delay_ms(self.transport_config(), attempts);
                    warn!(
                        attempt = attempts,
                        delay_ms = delay,
                        endpoint = "chat/completions",
                        schema = schema_name,
                        response_chars = raw.chars().count(),
                        error = %error,
                        "structured content JSON parse error, retrying request"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                Err(error) => return Err(ApiError::JsonParse(error)),
            }
        }
    }

    /// Estimate USD cost from a [`UsageBlock`], accounting for cached input discount.
    #[cfg(test)]
    fn estimate_cost(usage: &UsageBlock) -> f64 {
        estimate_cost(usage)
    }

    /// Determine whether an HTTP status warrants a retry.
    #[cfg(test)]
    fn is_retryable(status: u16) -> bool {
        is_retryable(status)
    }

    fn transport_config(&self) -> TransportConfig {
        TransportConfig {
            retry_limit: self.retry_limit,
            backoff_base_ms: self.backoff_base_ms,
            budget_limit: self.budget_limit,
            min_request_interval_ms: self.min_request_interval_ms,
        }
    }

    fn track_usage(&self, usage: &ApiUsage) -> Result<(), ApiError> {
        self.transport_state
            .record_usage(usage, self.transport_config().budget_limit)
    }

    fn build_chat_request(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        response_format: Option<ResponseFormat>,
        tools: Option<Vec<ToolDefinition>>,
        tool_choice: Option<ToolChoice>,
    ) -> ChatRequest {
        ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: system.to_string(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: user.to_string(),
                },
            ],
            max_tokens,
            response_format,
            reasoning_effort: self.reasoning_effort,
            tools,
            tool_choice,
        }
    }

    async fn do_chat(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        response_format: Option<ResponseFormat>,
    ) -> Result<(String, ApiUsage), ApiError> {
        let body = self.build_chat_request(system, user, max_tokens, response_format, None, None);

        let (content, api_usage) = post_json_with_retry_parsed(
            &self.http,
            &self.api_key,
            &self.base_url,
            &body,
            TransportRuntime {
                config: self.transport_config(),
                state: &self.transport_state,
            },
            "chat/completions",
            parse_chat_response,
        )
        .await?;
        self.track_usage(&api_usage)?;

        debug!(
            tokens = api_usage.tokens_used,
            cost_usd = api_usage.cost_usd,
            total_tokens = self.cumulative_tokens(),
            total_cost_usd = self.cumulative_cost(),
            "Mercury 2 call completed"
        );
        Ok((content, api_usage))
    }

    /// Send a chat request with OpenAI-compatible tool definitions and return
    /// both assistant text and tool calls for a future repair loop.
    pub async fn chat_with_tools(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
    ) -> Result<(String, Vec<ToolCall>, ApiUsage), ApiError> {
        let body =
            self.build_chat_request(system, user, max_tokens, None, Some(tools), tool_choice);

        let (content, tool_calls, api_usage) = post_json_with_retry_parsed(
            &self.http,
            &self.api_key,
            &self.base_url,
            &body,
            TransportRuntime {
                config: self.transport_config(),
                state: &self.transport_state,
            },
            "chat/completions",
            parse_chat_response_with_tools,
        )
        .await?;
        self.track_usage(&api_usage)?;

        debug!(
            tokens = api_usage.tokens_used,
            cost_usd = api_usage.cost_usd,
            total_tokens = self.cumulative_tokens(),
            total_cost_usd = self.cumulative_cost(),
            tool_calls = tool_calls.len(),
            "Mercury 2 tool call completed"
        );

        Ok((content, tool_calls, api_usage))
    }
}

impl std::fmt::Debug for Mercury2Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mercury2Client")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("retry_limit", &self.retry_limit)
            .field("backoff_base_ms", &self.backoff_base_ms)
            .field("budget_limit", &self.budget_limit)
            .field("min_request_interval_ms", &self.min_request_interval_ms)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish_non_exhaustive()
    }
}

impl Mercury2Api for Mercury2Client {
    async fn chat(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<(String, ApiUsage), ApiError> {
        self.do_chat(system, user, max_tokens, None).await
    }

    async fn chat_json(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<(ThermalAssessment, ApiUsage), ApiError> {
        self.chat_with_json_schema(
            system,
            user,
            max_tokens,
            THERMAL_ASSESSMENT_SCHEMA_NAME,
            thermal_assessment_json_schema(),
        )
        .await
    }

    async fn chat_json_schema_value(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        schema_name: &str,
        schema: Value,
    ) -> Result<(Value, ApiUsage), ApiError> {
        self.chat_with_json_schema(system, user, max_tokens, schema_name, schema)
            .await
    }

    async fn chat_with_tools(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
    ) -> Result<(String, Vec<ToolCall>, ApiUsage), ApiError> {
        Mercury2Client::chat_with_tools(self, system, user, max_tokens, tools, tool_choice).await
    }
}

// ---------------------------------------------------------------------------
// MercuryEditClient
// ---------------------------------------------------------------------------

/// Client for the Mercury Edit API (apply / complete / next).
///
/// `Debug` is implemented manually because [`reqwest::Client`] does not
/// derive it.
pub struct MercuryEditClient {
    http: Client,
    api_key: String,
    base_url: String,
    /// Maximum number of retry attempts for transient errors.
    pub retry_limit: u32,
    /// Base delay in milliseconds for exponential backoff.
    pub backoff_base_ms: u64,
    /// Optional per-session budget ceiling (USD). `None` means unlimited.
    pub budget_limit: Option<f64>,
    /// Minimum delay between outbound requests from this client.
    pub min_request_interval_ms: u64,
    transport_state: TransportState,
}

impl MercuryEditClient {
    /// Create a new edit client, reading the API key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::MissingApiKey`] if neither `INCEPTION_API_KEY`
    /// nor `MERCURY_API_KEY` is set.
    pub fn from_env(base_url: &str) -> Result<Self, ApiError> {
        Ok(Self::new(resolve_api_key("")?, base_url.to_string()))
    }

    /// Create a new edit client with an explicit API key and base URL.
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            base_url,
            retry_limit: 3,
            backoff_base_ms: 500,
            budget_limit: None,
            min_request_interval_ms: 0,
            transport_state: TransportState::default(),
        }
    }

    /// Set retry parameters.
    pub fn with_retries(mut self, limit: u32, backoff_base_ms: u64) -> Self {
        self.retry_limit = limit;
        self.backoff_base_ms = backoff_base_ms;
        self
    }

    /// Set a hard budget ceiling in USD.
    pub fn with_budget(mut self, limit_usd: f64) -> Self {
        self.budget_limit = Some(limit_usd);
        self
    }

    /// Enforce a minimum delay between outbound requests from this client.
    pub fn with_request_spacing(mut self, min_request_interval_ms: u64) -> Self {
        self.min_request_interval_ms = min_request_interval_ms;
        self
    }

    /// Return the cumulative cost spent by this client instance (USD).
    pub fn cumulative_cost(&self) -> f64 {
        self.transport_state.cumulative_cost()
    }

    /// Return the cumulative tokens consumed by this client instance.
    pub fn cumulative_tokens(&self) -> i64 {
        self.transport_state.cumulative_tokens()
    }

    fn transport_config(&self) -> TransportConfig {
        TransportConfig {
            retry_limit: self.retry_limit,
            backoff_base_ms: self.backoff_base_ms,
            budget_limit: self.budget_limit,
            min_request_interval_ms: self.min_request_interval_ms,
        }
    }

    fn track_usage(&self, usage: &ApiUsage) -> Result<(), ApiError> {
        self.transport_state
            .record_usage(usage, self.transport_config().budget_limit)
    }

    /// Post a JSON body to the given path with shared retry and throttle logic.
    async fn post_with_retry_parsed<T, P>(
        &self,
        path: &str,
        payload: &(impl Serialize + Sync),
        parse: P,
    ) -> Result<T, ApiError>
    where
        P: FnMut(&[u8]) -> Result<T, ApiError>,
    {
        let url = format!("{}{}", self.base_url, path);
        post_json_with_retry_parsed(
            &self.http,
            &self.api_key,
            &url,
            payload,
            TransportRuntime {
                config: self.transport_config(),
                state: &self.transport_state,
            },
            path,
            parse,
        )
        .await
    }

    fn build_apply_request(payload: &EditPayload) -> ChatRequest {
        ChatRequest {
            model: MERCURY_EDIT_MODEL.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: format_apply_prompt(payload),
            }],
            max_tokens: payload.max_tokens,
            response_format: None,
            reasoning_effort: None,
            tools: None,
            tool_choice: None,
        }
    }

    fn build_next_edit_request(current_file_path: &str, payload: &NextEditPayload) -> ChatRequest {
        ChatRequest {
            model: MERCURY_EDIT_MODEL.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: format_next_edit_prompt(current_file_path, payload),
            }],
            max_tokens: 4096,
            response_format: None,
            reasoning_effort: None,
            tools: None,
            tool_choice: None,
        }
    }

    /// Suggest the next logical edit while explicitly supplying the current file path.
    pub async fn next_edit_with_path(
        &self,
        current_file_path: &str,
        payload: &NextEditPayload,
    ) -> Result<(String, ApiUsage), ApiError> {
        let request = Self::build_next_edit_request(current_file_path, payload);
        let (text, usage) = self
            .post_with_retry_parsed("/edit/completions", &request, parse_chat_response)
            .await?;
        self.track_usage(&usage)?;
        debug!(
            tokens = usage.tokens_used,
            cost_usd = usage.cost_usd,
            total_tokens = self.cumulative_tokens(),
            total_cost_usd = self.cumulative_cost(),
            "Mercury Edit next-edit completed"
        );
        Ok((text, usage))
    }
}

impl std::fmt::Debug for MercuryEditClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MercuryEditClient")
            .field("base_url", &self.base_url)
            .field("retry_limit", &self.retry_limit)
            .field("backoff_base_ms", &self.backoff_base_ms)
            .field("budget_limit", &self.budget_limit)
            .field("min_request_interval_ms", &self.min_request_interval_ms)
            .finish_non_exhaustive()
    }
}

impl MercuryEditApi for MercuryEditClient {
    /// Apply edit via `/v1/apply/completions`.
    ///
    /// Wraps original code and update snippet in `<|original_code|>` and
    /// `<|update_snippet|>` markup tags as required by the API.
    async fn apply(&self, payload: &EditPayload) -> Result<(String, ApiUsage), ApiError> {
        let request = Self::build_apply_request(payload);
        let (text, usage) = self
            .post_with_retry_parsed("/apply/completions", &request, parse_chat_response)
            .await?;
        self.track_usage(&usage)?;
        debug!(
            tokens = usage.tokens_used,
            cost_usd = usage.cost_usd,
            total_tokens = self.cumulative_tokens(),
            total_cost_usd = self.cumulative_cost(),
            "Mercury Edit apply completed"
        );
        Ok((text, usage))
    }

    /// FIM autocomplete via `/v1/fim/completions`.
    ///
    /// Uses the Fill-In-the-Middle format with `prompt` and `suffix`.
    async fn complete(&self, payload: &CompletePayload) -> Result<(String, ApiUsage), ApiError> {
        let request = FimRequest {
            model: MERCURY_EDIT_MODEL.to_string(),
            prompt: payload.prompt.clone(),
            suffix: payload.suffix.clone(),
            max_tokens: payload.max_tokens,
            temperature: 0.0,
        };
        let (text, usage) = self
            .post_with_retry_parsed("/fim/completions", &request, parse_fim_response)
            .await?;
        self.track_usage(&usage)?;
        debug!(
            tokens = usage.tokens_used,
            cost_usd = usage.cost_usd,
            total_tokens = self.cumulative_tokens(),
            total_cost_usd = self.cumulative_cost(),
            "Mercury Edit FIM completed"
        );
        Ok((text, usage))
    }

    /// Next edit suggestion via `/v1/edit/completions`.
    ///
    /// Emits all documented wrapper sections using an empty path when callers
    /// do not provide one.
    async fn next_edit(&self, payload: &NextEditPayload) -> Result<(String, ApiUsage), ApiError> {
        self.next_edit_with_path("", payload).await
    }

    async fn next_edit_with_path(
        &self,
        current_file_path: &str,
        payload: &NextEditPayload,
    ) -> Result<(String, ApiUsage), ApiError> {
        MercuryEditClient::next_edit_with_path(self, current_file_path, payload).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    struct StubResponse {
        status_line: &'static str,
        body: Vec<u8>,
    }

    struct ApiStubServer {
        addr: SocketAddr,
        shutdown: Arc<AtomicBool>,
        request_count: Arc<AtomicUsize>,
        worker: Option<JoinHandle<()>>,
    }

    impl ApiStubServer {
        fn start(responses: Vec<StubResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("stub listener should bind");
            listener
                .set_nonblocking(true)
                .expect("stub listener should be configurable");
            let addr = listener
                .local_addr()
                .expect("stub listener should have a local address");
            let shutdown = Arc::new(AtomicBool::new(false));
            let request_count = Arc::new(AtomicUsize::new(0));
            let queued_responses = Arc::new(StdMutex::new(VecDeque::from(responses)));

            let worker_shutdown = Arc::clone(&shutdown);
            let worker_request_count = Arc::clone(&request_count);
            let worker_responses = Arc::clone(&queued_responses);
            let worker = thread::spawn(move || loop {
                if worker_shutdown.load(Ordering::Acquire) {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                        let _ = read_http_request(&mut stream);
                        worker_request_count.fetch_add(1, Ordering::AcqRel);

                        let response = worker_responses
                            .lock()
                            .expect("stub response queue poisoned")
                            .pop_front()
                            .unwrap_or_else(|| StubResponse {
                                status_line: "500 Internal Server Error",
                                body: br#"{"error":"no stub response queued"}"#.to_vec(),
                            });

                        write_http_response(&mut stream, response)
                            .expect("stub response should be writable");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("stub accept failed: {error}"),
                }
            });

            Self {
                addr,
                shutdown,
                request_count,
                worker: Some(worker),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::Acquire)
        }
    }

    impl Drop for ApiStubServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.addr);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn write_http_response(stream: &mut TcpStream, response: StubResponse) -> std::io::Result<()> {
        let headers = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.status_line,
            response.body.len()
        );
        stream.write_all(headers.as_bytes())?;
        stream.write_all(&response.body)?;
        stream.flush()
    }

    fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, String)>> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) if buffer.is_empty() => return Ok(None),
                Ok(0) => break None,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
                        break Some(index + 4);
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(err) => return Err(err),
            }
        };

        let Some(header_end) = header_end else {
            return Ok(None);
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);

        while buffer.len() < header_end + content_length {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(err) => return Err(err),
            }
        }

        let body = if buffer.len() >= header_end {
            let available = buffer.len().saturating_sub(header_end);
            let body_len = content_length.min(available);
            String::from_utf8_lossy(&buffer[header_end..header_end + body_len]).to_string()
        } else {
            String::new()
        };

        Ok(Some((path, body)))
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn chat_completion_response(content: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "choices": [
                {
                    "message": {
                        "content": content
                    }
                }
            ],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2,
                "cached_input_tokens": 0
            }
        }))
        .expect("chat completion response should serialize")
    }

    fn with_api_envs<T>(
        inception: Option<&str>,
        mercury: Option<&str>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock poisoned");
        let prev_inception = env::var(INCEPTION_API_KEY_ENV).ok();
        let prev_mercury = env::var(MERCURY_API_KEY_ENV).ok();

        match inception {
            Some(value) => env::set_var(INCEPTION_API_KEY_ENV, value),
            None => env::remove_var(INCEPTION_API_KEY_ENV),
        }
        match mercury {
            Some(value) => env::set_var(MERCURY_API_KEY_ENV, value),
            None => env::remove_var(MERCURY_API_KEY_ENV),
        }

        let result = catch_unwind(AssertUnwindSafe(f));

        match prev_inception {
            Some(value) => env::set_var(INCEPTION_API_KEY_ENV, value),
            None => env::remove_var(INCEPTION_API_KEY_ENV),
        }
        match prev_mercury {
            Some(value) => env::set_var(MERCURY_API_KEY_ENV, value),
            None => env::remove_var(MERCURY_API_KEY_ENV),
        }

        match result {
            Ok(value) => value,
            Err(payload) => resume_unwind(payload),
        }
    }

    #[test]
    fn error_display_budget_exceeded() {
        let err = ApiError::BudgetExceeded {
            spent: 1.2345,
            limit: 1.0,
        };
        let msg = format!("{err}");
        assert!(msg.contains("1.2345"));
        assert!(msg.contains("1.0000"));
    }

    #[test]
    fn error_display_max_retries() {
        let err = ApiError::MaxRetries(5);
        assert_eq!(format!("{err}"), "max retries (5) exceeded");
    }

    #[test]
    fn error_display_missing_key() {
        let err = ApiError::MissingApiKey(api_key_env_hint());
        let msg = format!("{err}");
        assert!(msg.contains(INCEPTION_API_KEY_ENV));
        assert!(msg.contains(MERCURY_API_KEY_ENV));
    }

    #[test]
    fn error_display_api_status() {
        let err = ApiError::ApiStatus {
            status: 403,
            body: "forbidden".into(),
        };
        assert!(format!("{err}").contains("403"));
        assert!(format!("{err}").contains("forbidden"));
    }

    #[test]
    fn thermal_assessment_deserializes() {
        let json = r#"{
            "complexity_score": 0.8,
            "dependency_score": 0.3,
            "risk_score": 0.5,
            "churn_score": 0.2,
            "suggested_action": "refactor",
            "reasoning": "High complexity with moderate risk"
        }"#;
        let assessment: ThermalAssessment =
            serde_json::from_str(json).expect("deserialization should succeed in test");
        assert!((assessment.complexity_score - 0.8).abs() < f64::EPSILON);
        assert_eq!(assessment.suggested_action, "refactor");
    }

    #[test]
    fn edit_payload_serializes() {
        let payload = EditPayload {
            original_code: "fn main() {}".into(),
            update_snippet: "fn main() { println!(\"hi\"); }".into(),
            max_tokens: 4096,
        };
        let json = serde_json::to_string(&payload).expect("serialization should succeed in test");
        assert!(json.contains("original_code"));
        assert!(json.contains("update_snippet"));
        assert!(json.contains("4096"));
    }

    #[test]
    fn edit_payload_default_max_tokens() {
        let json = r#"{"original_code":"fn main() {}","update_snippet":"fn main() { }"}"#;
        let payload: EditPayload =
            serde_json::from_str(json).expect("deserialization should succeed in test");
        assert_eq!(payload.max_tokens, 8192);
    }

    #[test]
    fn api_usage_defaults_to_zero() {
        let usage = ApiUsage::default();
        assert_eq!(usage.tokens_used, 0);
        assert!((usage.cost_usd - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn is_retryable_identifies_transient_codes() {
        assert!(Mercury2Client::is_retryable(429));
        assert!(Mercury2Client::is_retryable(500));
        assert!(Mercury2Client::is_retryable(502));
        assert!(Mercury2Client::is_retryable(503));
        assert!(Mercury2Client::is_retryable(599));
        assert!(!Mercury2Client::is_retryable(200));
        assert!(!Mercury2Client::is_retryable(400));
        assert!(!Mercury2Client::is_retryable(401));
        assert!(!Mercury2Client::is_retryable(403));
        assert!(!Mercury2Client::is_retryable(404));
    }

    #[test]
    fn cost_estimation_is_correct() {
        let usage = UsageBlock {
            prompt_tokens: 1000,
            completion_tokens: 1000,
            total_tokens: 2000,
            reasoning_tokens: 0,
            cached_input_tokens: 0,
        };
        let cost = Mercury2Client::estimate_cost(&usage);
        let expected = COST_PER_1K_INPUT + COST_PER_1K_OUTPUT;
        assert!((cost - expected).abs() < 1e-10);

        let usage_cached = UsageBlock {
            prompt_tokens: 1000,
            completion_tokens: 1000,
            total_tokens: 2000,
            reasoning_tokens: 0,
            cached_input_tokens: 500,
        };
        let cost_cached = Mercury2Client::estimate_cost(&usage_cached);
        assert!(cost_cached < cost, "cached inputs should reduce cost");
    }

    #[test]
    fn client_builder_methods() {
        let client = Mercury2Client::new("test-key".into())
            .with_base_url("http://localhost:8080".into())
            .with_model("mercury-coder-large".into())
            .with_retries(5, 1000)
            .with_budget(10.0)
            .with_request_spacing(250);

        assert_eq!(client.base_url, "http://localhost:8080");
        assert_eq!(client.model, "mercury-coder-large");
        assert_eq!(client.retry_limit, 5);
        assert_eq!(client.backoff_base_ms, 1000);
        assert_eq!(client.budget_limit, Some(10.0));
        assert_eq!(client.min_request_interval_ms, 250);
        assert!((client.cumulative_cost() - 0.0).abs() < f64::EPSILON);
        assert_eq!(client.cumulative_tokens(), 0);
    }

    #[test]
    fn budget_tracking_enforces_limit() {
        let client = Mercury2Client::new("test-key".into()).with_budget(0.10);

        let small = ApiUsage {
            tokens_used: 100,
            cost_usd: 0.05,
        };
        assert!(client.track_usage(&small).is_ok());
        assert!((client.cumulative_cost() - 0.05).abs() < 1e-10);
        assert_eq!(client.cumulative_tokens(), 100);

        let over = ApiUsage {
            tokens_used: 200,
            cost_usd: 0.08,
        };
        let result = client.track_usage(&over);
        assert!(result.is_err());
        match result {
            Err(ApiError::BudgetExceeded { spent, limit }) => {
                assert!((spent - 0.13).abs() < 1e-10);
                assert!((limit - 0.10).abs() < 1e-10);
            }
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
        assert_eq!(client.cumulative_tokens(), 300);
    }

    #[test]
    fn from_env_prefers_inception_key() {
        with_api_envs(Some("preferred-key"), Some("legacy-key"), || {
            let client = Mercury2Client::from_env().expect("client should load preferred key");
            assert_eq!(client.api_key, "preferred-key");
        });
    }

    #[test]
    fn from_env_falls_back_to_legacy_key() {
        with_api_envs(None, Some("legacy-key"), || {
            let client = Mercury2Client::from_env().expect("client should load fallback key");
            assert_eq!(client.api_key, "legacy-key");
        });
    }

    #[test]
    fn edit_client_from_env_uses_shared_resolution() {
        with_api_envs(Some("preferred-key"), Some("legacy-key"), || {
            let client = MercuryEditClient::from_env("http://localhost:9000")
                .expect("edit client should load preferred key");
            assert_eq!(client.api_key, "preferred-key");
        });
    }

    #[test]
    fn from_env_fails_without_key() {
        with_api_envs(None, None, || {
            let result = Mercury2Client::from_env();
            assert!(result.is_err());
            match result {
                Err(ApiError::MissingApiKey(name)) => {
                    assert!(name.contains(INCEPTION_API_KEY_ENV));
                    assert!(name.contains(MERCURY_API_KEY_ENV));
                }
                other => panic!("expected MissingApiKey, got {other:?}"),
            }
        });
    }

    #[test]
    fn edit_client_builder() {
        let client = MercuryEditClient::new("test-key".into(), "http://localhost:9000".into())
            .with_retries(7, 250)
            .with_budget(1.5)
            .with_request_spacing(100);

        assert_eq!(client.retry_limit, 7);
        assert_eq!(client.backoff_base_ms, 250);
        assert_eq!(client.budget_limit, Some(1.5));
        assert_eq!(client.min_request_interval_ms, 100);
        assert_eq!(client.cumulative_tokens(), 0);
    }

    #[test]
    #[allow(clippy::const_is_empty)]
    fn thermal_analysis_prompt_is_non_empty() {
        assert!(!THERMAL_ANALYSIS_PROMPT.is_empty());
        assert!(THERMAL_ANALYSIS_PROMPT.contains("complexity_score"));
        assert!(THERMAL_ANALYSIS_PROMPT.contains("suggested_action"));
    }

    #[test]
    fn complete_payload_round_trips() {
        let payload = CompletePayload {
            prompt: "fn foo() {\n    ".into(),
            suffix: "\n}".into(),
            max_tokens: 128,
        };
        let json = serde_json::to_string(&payload).expect("serialization should succeed in test");
        let back: CompletePayload =
            serde_json::from_str(&json).expect("deserialization should succeed in test");
        assert_eq!(back.suffix, "\n}");
        assert_eq!(back.max_tokens, 128);
    }

    #[test]
    fn next_edit_payload_round_trips() {
        let payload = NextEditPayload {
            file_content: "struct Foo;".into(),
            code_to_edit: "struct Foo;".into(),
            cursor: String::new(),
            recent_snippets: String::new(),
            edit_history: "--- a/src/lib.rs\n+++ b/src/lib.rs".into(),
        };
        let json = serde_json::to_string(&payload).expect("serialization should succeed in test");
        let back: NextEditPayload =
            serde_json::from_str(&json).expect("deserialization should succeed in test");
        assert_eq!(back.edit_history, "--- a/src/lib.rs\n+++ b/src/lib.rs");
    }

    #[test]
    fn reasoning_effort_serializes() {
        let json = serde_json::to_string(&ReasoningEffort::Instant)
            .expect("serialization should succeed in test");
        assert_eq!(json, "\"instant\"");
        let json = serde_json::to_string(&ReasoningEffort::High)
            .expect("serialization should succeed in test");
        assert_eq!(json, "\"high\"");
    }

    #[test]
    fn strict_json_schema_request_uses_official_shape() {
        let request = ChatRequest {
            model: MERCURY2_MODEL.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "plan this".into(),
            }],
            max_tokens: 256,
            response_format: Some(strict_json_schema_response_format(
                PLANNER_RESPONSE_SCHEMA_NAME,
                planner_response_json_schema_v1(),
            )),
            reasoning_effort: Some(ReasoningEffort::Medium),
            tools: None,
            tool_choice: None,
        };

        let json = serde_json::to_value(&request).expect("serialization should succeed in test");
        assert_eq!(json["response_format"]["type"], "json_schema");
        assert_eq!(
            json["response_format"]["json_schema"]["name"],
            PLANNER_RESPONSE_SCHEMA_NAME
        );
        assert_eq!(json["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            json["response_format"]["json_schema"]["schema"]["properties"]["schema_version"]
                ["const"],
            PLANNER_RESPONSE_SCHEMA_NAME
        );
        assert_eq!(
            json["response_format"]["json_schema"]["schema"]["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
    }

    #[test]
    fn apply_request_wraps_original_and_update_snippet_verbatim() {
        let payload = EditPayload {
            original_code: "fn main() {}".into(),
            update_snippet: "fn main() { println!(\"hi\"); }".into(),
            max_tokens: 1024,
        };
        let request = MercuryEditClient::build_apply_request(&payload);
        let content = &request.messages[0].content;

        assert!(content.contains("<|original_code|>\nfn main() {}\n<|/original_code|>"));
        assert!(content
            .contains("<|update_snippet|>\nfn main() { println!(\"hi\"); }\n<|/update_snippet|>"));
        assert!(!content.contains("// Instruction:"));
    }

    #[test]
    fn next_edit_request_includes_empty_wrappers_and_nested_sections() {
        let payload = NextEditPayload {
            file_content: "fn main() {}".into(),
            code_to_edit: String::new(),
            cursor: String::new(),
            recent_snippets: String::new(),
            edit_history: String::new(),
        };
        let request = MercuryEditClient::build_next_edit_request("src/main.rs", &payload);
        let content = &request.messages[0].content;

        assert!(content
            .contains("<|recently_viewed_code_snippets|>\n\n<|/recently_viewed_code_snippets|>"));
        assert!(content.contains(
            "<|current_file_content|>\ncurrent_file_path: src/main.rs\nfn main() {}\n<|code_to_edit|>\n\n<|cursor|>\n\n<|/cursor|>\n<|/code_to_edit|>\n<|/current_file_content|>"
        ));
        assert!(content.contains("<|edit_diff_history|>\n\n<|/edit_diff_history|>"));
        assert!(!content.contains("<|current_file_path|>"));
    }

    #[test]
    fn next_edit_request_preserves_chronological_unidiff_history() {
        let history = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@\n fn a() {}\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -2,2 +2,3 @@\n fn b() {}";
        let payload = NextEditPayload {
            file_content: "mod api;".into(),
            code_to_edit: "mod api;".into(),
            cursor: "1:1".into(),
            recent_snippets: "src/lib.rs\nfn a() {}".into(),
            edit_history: history.into(),
        };

        let request = MercuryEditClient::build_next_edit_request("src/main.rs", &payload);
        let content = &request.messages[0].content;
        assert!(content.contains(history));
        assert!(content.contains(
            "<|code_to_edit|>\nmod api;\n<|cursor|>\n1:1\n<|/cursor|>\n<|/code_to_edit|>"
        ));
    }

    #[test]
    fn tool_call_request_serializes_tools_and_named_tool_choice() {
        let request = ChatRequest {
            model: MERCURY2_MODEL.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "fix failing tests".into(),
            }],
            max_tokens: 256,
            response_format: None,
            reasoning_effort: Some(ReasoningEffort::Low),
            tools: Some(vec![ToolDefinition {
                kind: "function".into(),
                function: ToolFunctionDefinition {
                    name: "run_tests".into(),
                    description: Some("Run targeted tests".into()),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "command": {"type": "string"}
                        },
                        "required": ["command"],
                        "additionalProperties": false
                    }),
                },
            }]),
            tool_choice: Some(ToolChoice::Function(ToolChoiceFunction {
                kind: "function".into(),
                function: ToolChoiceFunctionName {
                    name: "run_tests".into(),
                },
            })),
        };

        let body = serde_json::to_value(&request).expect("serialization should succeed in test");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "run_tests");
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["function"]["name"], "run_tests");
    }

    #[test]
    fn parse_chat_response_with_tools_extracts_tool_calls_and_usage() {
        let raw = br#"{
            "choices": [
                {
                    "message": {
                        "content": "",
                        "tool_calls": [
                            {
                                "id": "call_123",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": "{\"path\":\"src/lib.rs\"}"
                                }
                            }
                        ]
                    }
                }
            ],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "total_tokens": 14,
                "cached_input_tokens": 0
            }
        }"#;

        let (content, tool_calls, usage) =
            parse_chat_response_with_tools(raw).expect("parse should succeed in test");
        assert_eq!(content, "");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_123");
        assert_eq!(tool_calls[0].function.name, "read_file");
        assert_eq!(
            tool_calls[0]
                .function
                .parse_arguments()
                .expect("valid json")["path"],
            "src/lib.rs"
        );
        assert_eq!(usage.tokens_used, 14);
    }

    #[test]
    fn parse_chat_response_with_tools_tolerates_null_tool_calls() {
        let raw = br#"{
            "choices": [
                {
                    "message": {
                        "content": "OK",
                        "tool_calls": null
                    }
                }
            ],
            "usage": {
                "prompt_tokens": 6,
                "completion_tokens": 1,
                "total_tokens": 7,
                "cached_input_tokens": 0
            }
        }"#;

        let (content, tool_calls, usage) =
            parse_chat_response_with_tools(raw).expect("parse should succeed in test");
        assert_eq!(content, "OK");
        assert!(tool_calls.is_empty());
        assert_eq!(usage.tokens_used, 7);
    }

    #[test]
    fn planner_schema_requires_versioned_contract() {
        let schema = planner_response_json_schema_v1();
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(schema["title"], "Mercury CLI Planner Response v1");
        assert_eq!(
            schema["properties"]["schema_version"]["const"],
            PLANNER_RESPONSE_SCHEMA_NAME
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["steps"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            schema["properties"]["steps"]["items"]["properties"]["priority"]["minimum"],
            0.0
        );
        assert_eq!(
            schema["properties"]["assessments"]["items"]["properties"]["reasoning"]["minLength"],
            1
        );
    }

    #[tokio::test]
    async fn chat_retries_after_malformed_outer_json_response() {
        let server = ApiStubServer::start(vec![
            StubResponse {
                status_line: "200 OK",
                body: br#"{"choices":"#.to_vec(),
            },
            StubResponse {
                status_line: "200 OK",
                body: chat_completion_response("ok"),
            },
        ]);

        let client = Mercury2Client::new("test-key".into())
            .with_base_url(server.url())
            .with_retries(2, 1);

        let (content, usage) = client
            .chat("system", "user", 64)
            .await
            .expect("client should retry malformed outer JSON");

        assert_eq!(content, "ok");
        assert_eq!(usage.tokens_used, 2);
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn chat_with_json_schema_retries_after_malformed_structured_content() {
        let schema = json!({
            "type": "object",
            "properties": {
                "ok": { "type": "boolean" }
            },
            "required": ["ok"],
            "additionalProperties": false
        });
        let server = ApiStubServer::start(vec![
            StubResponse {
                status_line: "200 OK",
                body: chat_completion_response("{\"ok\":"),
            },
            StubResponse {
                status_line: "200 OK",
                body: chat_completion_response(
                    &serde_json::to_string(&json!({ "ok": true }))
                        .expect("structured response should serialize"),
                ),
            },
        ]);

        let client = Mercury2Client::new("test-key".into())
            .with_base_url(server.url())
            .with_retries(2, 1);

        let (value, usage): (Value, ApiUsage) = client
            .chat_with_json_schema("system", "user", 64, "test_schema", schema)
            .await
            .expect("client should retry malformed structured content");

        assert_eq!(value["ok"], true);
        assert_eq!(usage.tokens_used, 2);
        assert_eq!(server.request_count(), 2);
    }
}
