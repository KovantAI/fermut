//! Anthropic Messages API client over `ureq`.
//!
//! Single sync call per `complete()` — no streaming, no async. The mock
//! variant satisfies `LlmClient` without touching the network, gated by
//! `FERMUT_LLM_MOCK=1` so unit tests run deterministically.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{DEFAULT_MAX_TOKENS, DEFAULT_MODEL};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Wall-clock cap on a single Messages API call. ureq has no default; without
/// this a hung TLS handshake or a stuck stream blocks the worker thread
/// forever, freezing `fermut suggest --parallel N` for every other mutant
/// behind it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total attempts (initial + retries). Retries cover transient transport
/// errors and Anthropic 5xx / 429. 4xx is treated as terminal — retrying a
/// 400/401/403 just wastes API budget.
const MAX_ATTEMPTS: u32 = 3;

/// Per-call request envelope. Subset of the Messages API we actually use.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
}

impl LlmRequest {
    pub fn new(system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            system: system.into(),
            user: user.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

pub trait LlmClient: Send + Sync {
    /// Synchronously call the model. Returns the assistant text content.
    fn complete(&self, req: &LlmRequest) -> Result<String>;
}

/// Selects a client based on environment:
///
/// - `FERMUT_LLM_MOCK` set to a truthy value (`1`, `true`, `yes`, `on`) →
///   `MockClient` (deterministic, offline). `0`, `false`, `no`, `off`, or an
///   empty value is treated as unset so a CI script can flip the flag off
///   without unsetting it.
/// - else → `AnthropicClient` using `ANTHROPIC_API_KEY` (or
///   `FERMUT_ANTHROPIC_API_KEY` if scoped).
pub fn client_from_env() -> Result<Box<dyn LlmClient>> {
    if env_flag_enabled("FERMUT_LLM_MOCK") {
        return Ok(Box::new(MockClient));
    }
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("FERMUT_ANTHROPIC_API_KEY"))
        .map_err(|_| {
            anyhow!(
                "ANTHROPIC_API_KEY is not set. Export your key, or run with \
                 FERMUT_LLM_MOCK=1 for a canned response."
            )
        })?;
    Ok(Box::new(AnthropicClient::new(key)))
}

/// Parses a boolean-like env var. Truthy: `1`, `true`, `yes`, `on` (case
/// insensitive, trimmed). Anything else — including `0`, `false`, empty, or
/// unset — is treated as disabled. Matches the convention used by most CI
/// systems so a script that exports `FOO=0` to turn a flag off behaves the
/// way users expect.
fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

pub struct AnthropicClient {
    api_key: String,
    agent: ureq::Agent,
}

impl AnthropicClient {
    pub fn new(api_key: String) -> Self {
        // Single Agent reused across calls so the underlying connection
        // pool, TLS context, and timeout config don't get rebuilt per
        // request. Timeouts apply to every request issued through this
        // agent; ureq's default is unbounded.
        //
        // http_status_as_error(false): keep 4xx/5xx as `Ok(response)` so we
        // can read the body into the error message and drive our own retry
        // policy. With the default (true) a non-2xx becomes
        // `Error::StatusCode(code)` with no body attached.
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build();
        let agent: ureq::Agent = config.into();
        Self { api_key, agent }
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<WireMessage<'a>>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct WireResponse {
    content: Vec<WireBlock>,
}

#[derive(Deserialize)]
struct WireBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl LlmClient for AnthropicClient {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        let body = WireRequest {
            model: &req.model,
            max_tokens: req.max_tokens,
            system: &req.system,
            messages: vec![WireMessage {
                role: "user",
                content: &req.user,
            }],
        };
        let payload = serde_json::to_value(&body).context("serialize request")?;

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            // `send_json` sets `content-type: application/json` itself. The
            // agent is built with `http_status_as_error(false)`, so a non-2xx
            // arrives as `Ok(response)` (body intact) rather than an error.
            let resp = self
                .agent
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .send_json(&payload);

            match resp {
                Ok(mut r) => {
                    let code = r.status().as_u16();
                    if (200..300).contains(&code) {
                        return parse_response(r);
                    }
                    let body = r.body_mut().read_to_string().unwrap_or_default();
                    let err = anyhow!("Anthropic API returned {code}: {body}");
                    if is_retryable_status(code) && attempt < MAX_ATTEMPTS {
                        warn!(
                            attempt,
                            code, "Anthropic API returned retryable status; backing off"
                        );
                        backoff(attempt);
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
                Err(e) => {
                    // Transport-level errors (connect refused, TLS reset,
                    // read timeout) are always worth retrying — the
                    // server never saw a complete request, so we're not
                    // double-spending API budget.
                    let err = anyhow!("Anthropic API transport error: {e}");
                    if attempt < MAX_ATTEMPTS {
                        warn!(attempt, error = %e, "Anthropic API transport error; backing off");
                        backoff(attempt);
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("Anthropic API: no attempts made")))
    }
}

fn parse_response(mut resp: ureq::http::Response<ureq::Body>) -> Result<String> {
    let parsed: WireResponse = resp
        .body_mut()
        .read_json()
        .context("parse Anthropic Messages response")?;
    let text = parsed
        .content
        .into_iter()
        .filter(|b| b.kind == "text")
        .map(|b| b.text)
        .collect::<Vec<_>>()
        .join("");
    if text.trim().is_empty() {
        return Err(anyhow!(
            "Anthropic returned no text content (possibly only tool_use blocks)"
        ));
    }
    Ok(text)
}

/// 5xx and 429 are transient; the server is overloaded or briefly broken.
/// 4xx other than 429 means the request itself is bad (auth, schema,
/// rate-limit exhaustion that won't reset on this timescale) — retrying
/// is wasted API spend.
fn is_retryable_status(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}

/// Exponential backoff: 1s, 2s, 4s. Bounded by `MAX_ATTEMPTS` so total
/// wait stays sub-10s. Sync sleep is fine — `complete()` is sync, called
/// from rayon workers that already block on the response.
fn backoff(attempt: u32) {
    let secs = 1u64 << (attempt - 1).min(3);
    std::thread::sleep(Duration::from_secs(secs));
}

/// Offline client used by `FERMUT_LLM_MOCK=1`. Returns a deterministic
/// canned response that round-trips the user prompt so tests can assert
/// the prompt body without hitting the network.
#[derive(Default)]
pub struct MockClient;

impl LlmClient for MockClient {
    fn complete(&self, req: &LlmRequest) -> Result<String> {
        Ok(format!(
            "# mock LLM response\n# model={model}\n# user prompt (first 200 chars):\n# {snippet}\n\n```python\ndef test_mock_generated():\n    # placeholder generated by FERMUT_LLM_MOCK\n    assert True\n```\n",
            model = req.model,
            snippet = req.user.chars().take(200).collect::<String>()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_canned_payload_with_model_echoed() {
        let c = MockClient;
        let r = LlmRequest::new("sys", "USER_TOKEN_XYZ").with_model("claude-haiku-test");
        let out = c.complete(&r).unwrap();
        assert!(out.contains("model=claude-haiku-test"));
        assert!(out.contains("USER_TOKEN_XYZ"));
        assert!(out.contains("```python"));
    }

    #[test]
    fn client_from_env_uses_mock_when_flag_set() {
        let prev_mock = std::env::var("FERMUT_LLM_MOCK").ok();
        // SAFETY: tests in the same module touching env can race; serialize
        // by running this isolated. Restore after.
        std::env::set_var("FERMUT_LLM_MOCK", "1");
        let c = client_from_env().unwrap();
        let r = LlmRequest::new("s", "u");
        let out = c.complete(&r).unwrap();
        assert!(out.contains("mock LLM response"));
        match prev_mock {
            Some(v) => std::env::set_var("FERMUT_LLM_MOCK", v),
            None => std::env::remove_var("FERMUT_LLM_MOCK"),
        }
    }

    #[test]
    fn env_flag_enabled_truthy_values() {
        let key = "FERMUT_TEST_FLAG_TRUTHY";
        for v in ["1", "true", "TRUE", "True", "yes", "YES", "on", " 1 "] {
            std::env::set_var(key, v);
            assert!(env_flag_enabled(key), "expected `{v}` to be truthy");
        }
        std::env::remove_var(key);
    }

    /// Regression: previously, *any* set value (including `0` or `false`) was
    /// treated as enabled because `std::env::var(...).is_ok()` ignores the
    /// payload. A CI script that exports `FERMUT_LLM_MOCK=0` to flip mock
    /// mode off must hit the real API, not silently keep mocking.
    #[test]
    fn env_flag_enabled_falsy_values() {
        let key = "FERMUT_TEST_FLAG_FALSY";
        for v in ["0", "false", "FALSE", "no", "off", "", " ", "garbage"] {
            std::env::set_var(key, v);
            assert!(!env_flag_enabled(key), "expected `{v}` to be falsy");
        }
        std::env::remove_var(key);
        assert!(!env_flag_enabled(key));
    }
}
