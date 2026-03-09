//! Mercury 2 and Mercury Edit API clients.
//!
//! This module provides typed, mockable clients for the Inception Labs
//! Mercury API surface:
//!
//! - [`Mercury2Client`] — chat completions with structured JSON output
//! - [`MercuryEditClient`] — code editing (apply / complete / next)
//!
//! All public items carry doc comments, errors use [`thiserror`], and every
//! HTTP-facing method is exposed through a trait so callers can substitute
//! test doubles.

use std::env;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default base URL for the Mercury 2 chat completions endpoint.
const MERCURY2_BASE_URL: &str = "https://api.inceptionlabs.ai/v1/chat/completions";

/// Default model identifier sent in every Mercury 2 request.
const MERCURY2_MODEL: &str = "mercury-2";

/// Environment variable expected to hold the Inception Labs API key.
const API_KEY_ENV: &str = "MERCURY_API_KEY";

/// Cost per 1 000 input tokens (USD) — Mercury 2 / Mercury Edit.
const COST_PER_1K_INPUT: f64 = 0.00025;

/// Cost per 1 000 cached input tokens (USD) — 10x cheaper.
const COST_PER_1K_CACHED_INPUT: f64 = 0.000025;

/// Cost per 1 000 output tokens (USD) — Mercury 2 / Mercury Edit.
const COST_PER_1K_OUTPUT: f64 = 0.00075;

/// Default model for Mercury Edit endpoints (apply / fim / next-edit).
const MERCURY_EDIT_MODEL: &str = "mercury-edit";

// ---------------------------------------------------------------------------
// Reasoning effort
// ---------------------------------------------------------------------------

/// Controls how much reasoning the model does before responding.
///
/// - `Instant` — near-zero latency, 0 reasoning tokens
/// - `Low` / `Medium` / `High` — increasing quality at the cost of latency
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
- suggested_action: one of ["lock", "refactor", "test", "monitor", "ignore"]
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
/// The client auto-wraps these fields in `<|original_code|>` and
/// `<|update_snippet|>` markup tags before sending.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPayload {
    /// The original source code before any edits.
    pub original_code: String,
    /// The modified code snippet to apply (the "update").
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
/// The client auto-wraps these fields in the required markup tags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NextEditPayload {
    /// The full file content.
    pub file_content: String,
    /// The specific code region to edit.
    #[serde(default)]
    pub code_to_edit: String,
    /// Optional cursor position within `code_to_edit`.
    #[serde(default)]
    pub cursor: String,
    /// Optional recently-viewed snippets for context.
    #[serde(default)]
    pub recent_snippets: String,
    /// Stringified history of prior edit diffs.
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

/// `response_format` field for structured JSON output.
#[derive(Debug, Serialize)]
struct ResponseFormat {
    r#type: String,
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
}

/// The top-level chat completion response.
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    usage: Option<UsageBlock>,
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
}

/// Trait abstracting Mercury Edit operations so callers can inject fakes.
pub trait MercuryEditApi: Send + Sync {
    /// Apply an instruction-driven edit to a code region.
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
}

// ---------------------------------------------------------------------------
// Mercury2Client
// ---------------------------------------------------------------------------

/// Client for the Mercury 2 chat completions API.
///
/// Supports optional structured-JSON output, exponential-backoff retries on
/// transient errors (HTTP 429 / 5xx), and per-call budget tracking.
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
    /// Optional reasoning effort level for Mercury 2.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Running total of money spent across calls on this client.
    cumulative_cost: std::sync::atomic::AtomicU64,
}

impl Mercury2Client {
    /// Create a new client, reading the API key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::MissingApiKey`] if `MERCURY_API_KEY` is not set.
    pub fn from_env() -> Result<Self, ApiError> {
        let api_key =
            env::var(API_KEY_ENV).map_err(|_| ApiError::MissingApiKey(API_KEY_ENV.to_string()))?;
        Ok(Self::new(api_key))
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
            reasoning_effort: None,
            cumulative_cost: std::sync::atomic::AtomicU64::new(0),
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

    /// Set the reasoning effort level (instant / low / medium / high).
    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Return the cumulative cost spent by this client instance (USD).
    pub fn cumulative_cost(&self) -> f64 {
        f64::from_bits(
            self.cumulative_cost
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    // -- internal helpers ---------------------------------------------------

    /// Accumulate cost and enforce the budget ceiling.
    fn track_cost(&self, usage: &ApiUsage) -> Result<(), ApiError> {
        loop {
            let old_bits = self
                .cumulative_cost
                .load(std::sync::atomic::Ordering::Relaxed);
            let old = f64::from_bits(old_bits);
            let new = old + usage.cost_usd;
            let new_bits = new.to_bits();
            if self
                .cumulative_cost
                .compare_exchange_weak(
                    old_bits,
                    new_bits,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                if let Some(limit) = self.budget_limit {
                    if new > limit {
                        return Err(ApiError::BudgetExceeded { spent: new, limit });
                    }
                }
                return Ok(());
            }
        }
    }

    /// Estimate USD cost from a [`UsageBlock`], accounting for cached input discount.
    fn estimate_cost(usage: &UsageBlock) -> f64 {
        let uncached_input = (usage.prompt_tokens - usage.cached_input_tokens).max(0);
        let cached_cost = (usage.cached_input_tokens as f64 / 1000.0) * COST_PER_1K_CACHED_INPUT;
        let input_cost = (uncached_input as f64 / 1000.0) * COST_PER_1K_INPUT;
        let output_cost = (usage.completion_tokens as f64 / 1000.0) * COST_PER_1K_OUTPUT;
        cached_cost + input_cost + output_cost
    }

    /// Determine whether an HTTP status warrants a retry.
    fn is_retryable(status: u16) -> bool {
        status == 429 || (500..600).contains(&status)
    }

    /// Execute the chat completion request with retry logic.
    async fn do_chat(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        json_mode: bool,
    ) -> Result<(String, ApiUsage), ApiError> {
        let body = ChatRequest {
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
            response_format: if json_mode {
                Some(ResponseFormat {
                    r#type: "json_object".into(),
                })
            } else {
                None
            },
            reasoning_effort: self.reasoning_effort,
        };

        let mut attempts: u32 = 0;

        loop {
            attempts += 1;

            let response = self
                .http
                .post(&self.base_url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await?;

            let status = response.status().as_u16();

            if !response.status().is_success() {
                let response_body = response.text().await.unwrap_or_default();

                if Self::is_retryable(status) && attempts <= self.retry_limit {
                    let delay = self.backoff_base_ms * 2u64.saturating_pow(attempts - 1);
                    warn!(
                        status,
                        attempt = attempts,
                        delay_ms = delay,
                        "retryable API error, backing off"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }

                if attempts > self.retry_limit && Self::is_retryable(status) {
                    return Err(ApiError::MaxRetries(self.retry_limit));
                }

                return Err(ApiError::ApiStatus {
                    status,
                    body: response_body,
                });
            }

            let chat_resp: ChatResponse = response.json().await?;

            let content = chat_resp
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .unwrap_or_default();

            let api_usage = match chat_resp.usage {
                Some(u) => {
                    let cost = Self::estimate_cost(&u);
                    ApiUsage {
                        tokens_used: u.prompt_tokens + u.completion_tokens,
                        cost_usd: cost,
                    }
                }
                None => ApiUsage::default(),
            };

            self.track_cost(&api_usage)?;

            debug!(
                tokens = api_usage.tokens_used,
                cost_usd = api_usage.cost_usd,
                "Mercury 2 call completed"
            );

            return Ok((content, api_usage));
        }
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
        self.do_chat(system, user, max_tokens, false).await
    }

    async fn chat_json(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<(ThermalAssessment, ApiUsage), ApiError> {
        let (raw, usage) = self.do_chat(system, user, max_tokens, true).await?;
        let assessment: ThermalAssessment = serde_json::from_str(&raw)?;
        Ok((assessment, usage))
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
}

impl MercuryEditClient {
    /// Create a new edit client, reading the API key from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::MissingApiKey`] if `MERCURY_API_KEY` is not set.
    pub fn from_env(base_url: &str) -> Result<Self, ApiError> {
        let api_key =
            env::var(API_KEY_ENV).map_err(|_| ApiError::MissingApiKey(API_KEY_ENV.to_string()))?;
        Ok(Self::new(api_key, base_url.to_string()))
    }

    /// Create a new edit client with an explicit API key and base URL.
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            base_url,
            retry_limit: 3,
            backoff_base_ms: 500,
        }
    }

    /// Set retry parameters.
    pub fn with_retries(mut self, limit: u32, backoff_base_ms: u64) -> Self {
        self.retry_limit = limit;
        self.backoff_base_ms = backoff_base_ms;
        self
    }

    /// Post a JSON body to the given path with retry logic for transient errors.
    /// Returns the raw response body bytes on success.
    async fn post_with_retry_raw(
        &self,
        path: &str,
        payload: &(impl Serialize + Sync),
    ) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}{}", self.base_url, path);
        let mut attempts: u32 = 0;

        loop {
            attempts += 1;

            let response = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(payload)
                .send()
                .await?;

            let status = response.status().as_u16();

            if !response.status().is_success() {
                let body = response.text().await.unwrap_or_default();

                if Mercury2Client::is_retryable(status) && attempts <= self.retry_limit {
                    let delay = self.backoff_base_ms * 2u64.saturating_pow(attempts - 1);
                    warn!(
                        status,
                        attempt = attempts,
                        delay_ms = delay,
                        "retryable edit API error, backing off"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }

                if attempts > self.retry_limit && Mercury2Client::is_retryable(status) {
                    return Err(ApiError::MaxRetries(self.retry_limit));
                }

                return Err(ApiError::ApiStatus { status, body });
            }

            return Ok(response.bytes().await?.to_vec());
        }
    }

    /// Extract content and usage from a chat completion response.
    fn parse_chat_response(raw: &[u8]) -> Result<(String, ApiUsage), ApiError> {
        let resp: ChatResponse = serde_json::from_slice(raw)?;
        let content = resp
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        let usage = match resp.usage {
            Some(u) => {
                let cost = Mercury2Client::estimate_cost(&u);
                ApiUsage {
                    tokens_used: u.prompt_tokens + u.completion_tokens,
                    cost_usd: cost,
                }
            }
            None => ApiUsage::default(),
        };
        Ok((content, usage))
    }

    /// Extract text and usage from a FIM completion response.
    fn parse_fim_response(raw: &[u8]) -> Result<(String, ApiUsage), ApiError> {
        let resp: FimResponse = serde_json::from_slice(raw)?;
        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.text)
            .unwrap_or_default();
        let usage = match resp.usage {
            Some(u) => {
                let cost = Mercury2Client::estimate_cost(&u);
                ApiUsage {
                    tokens_used: u.prompt_tokens + u.completion_tokens,
                    cost_usd: cost,
                }
            }
            None => ApiUsage::default(),
        };
        Ok((text, usage))
    }
}

impl std::fmt::Debug for MercuryEditClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MercuryEditClient")
            .field("base_url", &self.base_url)
            .field("retry_limit", &self.retry_limit)
            .field("backoff_base_ms", &self.backoff_base_ms)
            .finish_non_exhaustive()
    }
}

impl MercuryEditApi for MercuryEditClient {
    /// Apply edit via `/v1/apply/completions`.
    ///
    /// Wraps original code and update snippet in `<|original_code|>` and
    /// `<|update_snippet|>` markup tags as required by the API.
    async fn apply(&self, payload: &EditPayload) -> Result<(String, ApiUsage), ApiError> {
        let content = format!(
            "<|original_code|>\n{}\n<|/original_code|>\n\n<|update_snippet|>\n{}\n<|/update_snippet|>",
            payload.original_code, payload.update_snippet
        );
        let request = ChatRequest {
            model: MERCURY_EDIT_MODEL.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content,
            }],
            max_tokens: payload.max_tokens,
            response_format: None,
            reasoning_effort: None,
        };
        let raw = self
            .post_with_retry_raw("/apply/completions", &request)
            .await?;
        let (text, usage) = Self::parse_chat_response(&raw)?;
        debug!(
            tokens = usage.tokens_used,
            cost_usd = usage.cost_usd,
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
        let raw = self
            .post_with_retry_raw("/fim/completions", &request)
            .await?;
        let (text, usage) = Self::parse_fim_response(&raw)?;
        debug!(
            tokens = usage.tokens_used,
            cost_usd = usage.cost_usd,
            "Mercury Edit FIM completed"
        );
        Ok((text, usage))
    }

    /// Next edit suggestion via `/v1/edit/completions`.
    ///
    /// Wraps fields in the required markup tags:
    /// `<|recently_viewed_code_snippets|>`, `<|current_file_content|>`,
    /// `<|code_to_edit|>`, `<|cursor|>`, `<|edit_diff_history|>`.
    async fn next_edit(&self, payload: &NextEditPayload) -> Result<(String, ApiUsage), ApiError> {
        let mut parts = Vec::new();
        if !payload.recent_snippets.is_empty() {
            parts.push(format!(
                "<|recently_viewed_code_snippets|>\n{}\n<|/recently_viewed_code_snippets|>",
                payload.recent_snippets
            ));
        }
        parts.push(format!(
            "<|current_file_content|>\n{}\n<|/current_file_content|>",
            payload.file_content
        ));
        if !payload.code_to_edit.is_empty() {
            parts.push(format!(
                "<|code_to_edit|>\n{}\n<|/code_to_edit|>",
                payload.code_to_edit
            ));
        }
        if !payload.cursor.is_empty() {
            parts.push(format!("<|cursor|>\n{}\n<|/cursor|>", payload.cursor));
        }
        if !payload.edit_history.is_empty() {
            parts.push(format!(
                "<|edit_diff_history|>\n{}\n<|/edit_diff_history|>",
                payload.edit_history
            ));
        }
        let content = parts.join("\n");
        let request = ChatRequest {
            model: MERCURY_EDIT_MODEL.to_string(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content,
            }],
            max_tokens: 4096,
            response_format: None,
            reasoning_effort: None,
        };
        let raw = self
            .post_with_retry_raw("/edit/completions", &request)
            .await?;
        let (text, usage) = Self::parse_chat_response(&raw)?;
        debug!(
            tokens = usage.tokens_used,
            cost_usd = usage.cost_usd,
            "Mercury Edit next-edit completed"
        );
        Ok((text, usage))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let err = ApiError::MissingApiKey("MERCURY_API_KEY".into());
        assert!(format!("{err}").contains("MERCURY_API_KEY"));
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

        // With cached tokens: 500 cached, 500 uncached
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
            .with_budget(10.0);

        assert_eq!(client.base_url, "http://localhost:8080");
        assert_eq!(client.model, "mercury-coder-large");
        assert_eq!(client.retry_limit, 5);
        assert_eq!(client.backoff_base_ms, 1000);
        assert_eq!(client.budget_limit, Some(10.0));
        assert!((client.cumulative_cost() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_tracking_enforces_limit() {
        let client = Mercury2Client::new("test-key".into()).with_budget(0.10);

        let small = ApiUsage {
            tokens_used: 100,
            cost_usd: 0.05,
        };
        assert!(client.track_cost(&small).is_ok());
        assert!((client.cumulative_cost() - 0.05).abs() < 1e-10);

        // Second call pushes past the limit
        let over = ApiUsage {
            tokens_used: 200,
            cost_usd: 0.08,
        };
        let result = client.track_cost(&over);
        assert!(result.is_err());
        match result {
            Err(ApiError::BudgetExceeded { spent, limit }) => {
                assert!((spent - 0.13).abs() < 1e-10);
                assert!((limit - 0.10).abs() < 1e-10);
            }
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
    }

    #[test]
    fn from_env_fails_without_key() {
        // Temporarily ensure the env var is unset for this test.
        // If it was previously set we restore it afterward.
        let prev = env::var(API_KEY_ENV).ok();
        env::remove_var(API_KEY_ENV);

        let result = Mercury2Client::from_env();
        assert!(result.is_err());
        match result {
            Err(ApiError::MissingApiKey(name)) => assert_eq!(name, API_KEY_ENV),
            other => panic!("expected MissingApiKey, got {other:?}"),
        }

        // Restore if it was set.
        if let Some(val) = prev {
            env::set_var(API_KEY_ENV, val);
        }
    }

    #[test]
    fn edit_client_builder() {
        let client = MercuryEditClient::new("test-key".into(), "http://localhost:9000".into())
            .with_retries(7, 250);

        assert_eq!(client.retry_limit, 7);
        assert_eq!(client.backoff_base_ms, 250);
    }

    #[test]
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
            edit_history: "added struct Foo".into(),
        };
        let json = serde_json::to_string(&payload).expect("serialization should succeed in test");
        let back: NextEditPayload =
            serde_json::from_str(&json).expect("deserialization should succeed in test");
        assert_eq!(back.edit_history, "added struct Foo");
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
}
