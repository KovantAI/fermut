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

/// Explicit cap on the response body we buffer. ureq 3 defaults to 10MB and
/// returns an `Err` past it — which the error-body read swallows to an empty
/// string, and the success read surfaces as a parse failure. We set our own so
/// the ceiling is intentional and survives a ureq default change. Anthropic
/// Messages responses are far smaller, but a `max_tokens`-maxed completion plus
/// usage metadata can run to a few MB — 32MB leaves generous headroom.
const RESPONSE_BODY_LIMIT: u64 = 32 * 1024 * 1024;

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
        // request. Timeouts apply to every request issued through this agent.
        let agent: ureq::Agent = agent_config().into();
        Self { api_key, agent }
    }
}

/// Build the shared agent config. Extracted (and pure) so the timeout policy
/// is asserted in a unit test instead of only exercised over the network.
///
/// Timeouts are set **per phase**, not as a single `timeout_global`. A global
/// cap is one clock for connect + send + receive, so a slow connect eats into
/// the body-read budget; bounding each phase independently gives the response
/// stream its full [`HTTP_TIMEOUT`] regardless of connect time, while still
/// guaranteeing no phase can hang a worker forever (ureq's default is
/// unbounded). `http_status_as_error(false)` keeps 4xx/5xx as `Ok(response)`
/// so we read the body into the error message and drive our own retry policy;
/// the default (true) turns a non-2xx into `Error::StatusCode` with no body.
fn agent_config() -> ureq::config::Config {
    ureq::Agent::config_builder()
        .timeout_resolve(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_send_request(Some(HTTP_TIMEOUT))
        .timeout_send_body(Some(HTTP_TIMEOUT))
        .timeout_recv_response(Some(HTTP_TIMEOUT))
        .timeout_recv_body(Some(HTTP_TIMEOUT))
        .http_status_as_error(false)
        .build()
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
                    let body =
                        read_capped_string(r.body_mut(), RESPONSE_BODY_LIMIT).unwrap_or_default();
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
                    // A completion is NOT idempotent: retrying after the
                    // request was fully sent re-runs the generation and
                    // double-bills tokens. So only retry transport errors that
                    // prove the request never reached the server intact
                    // (connect/DNS/TLS/send-phase). A receive-phase or global
                    // timeout may mean Anthropic already processed the call —
                    // surface it instead of gambling on a duplicate.
                    let err = anyhow!("Anthropic API transport error: {e}");
                    if transport_retryable(&e) && attempt < MAX_ATTEMPTS {
                        warn!(attempt, error = %e, "Anthropic API transport error (pre-send); backing off");
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
    let parsed: WireResponse = read_capped_json(resp.body_mut(), RESPONSE_BODY_LIMIT)
        .context("parse Anthropic Messages response")?;
    extract_text(parsed)
}

/// Read a JSON body with an explicit byte cap. Centralizes the
/// `with_config().limit(..)` incantation so the two read sites can't drift, and
/// takes `limit` as a parameter so the cap itself is unit-testable with a small
/// value. A body larger than `limit` errors rather than parsing a truncation.
fn read_capped_json<T: serde::de::DeserializeOwned>(
    body: &mut ureq::Body,
    limit: u64,
) -> Result<T, ureq::Error> {
    body.with_config().limit(limit).read_json()
}

/// String counterpart of [`read_capped_json`] for reading error-response bodies
/// into a diagnostic message under the same explicit cap.
fn read_capped_string(body: &mut ureq::Body, limit: u64) -> Result<String, ureq::Error> {
    body.with_config().limit(limit).read_to_string()
}

/// Flatten the wire response into assistant text: concatenate every `text`
/// block in order, ignore non-text blocks (e.g. `tool_use`), and treat an
/// all-whitespace result as an error so a caller never acts on a blank
/// suggestion. Pure so the folding + empty guard are unit-tested without a
/// network round-trip.
fn extract_text(parsed: WireResponse) -> Result<String> {
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

/// Whether a `ureq` transport error is safe to retry for our *non-idempotent*
/// POST. Only errors that prove the request never reached the server as a
/// complete unit qualify — retrying anything that could already have been
/// received and processed risks a duplicate completion and double token spend.
///
/// Retryable (failure before the full request left the client): DNS, connect,
/// TLS, and send-phase timeouts, plus connect-phase IO errors. Not retryable:
/// global / per-call / receive-phase timeouts, and any other IO or protocol
/// error we can't prove happened pre-send.
fn transport_retryable(err: &ureq::Error) -> bool {
    use ureq::{Error, Timeout};
    match err {
        Error::HostNotFound
        | Error::ConnectionFailed
        | Error::ConnectProxyFailed(_)
        | Error::Tls(_)
        // rustls surfaces handshake failures here, not as `Tls`. The handshake
        // completes before any request byte is sent, so a transient reset is
        // safe to retry. Unconditional match: our ureq is pinned to the rustls
        // feature (guarded by tests/deps_hygiene.rs), so the variant is always
        // present.
        | Error::Rustls(_)
        | Error::TlsRequired => true,
        Error::Timeout(t) => matches!(
            t,
            Timeout::Resolve | Timeout::Connect | Timeout::SendRequest | Timeout::SendBody
        ),
        Error::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::AddrNotAvailable
        ),
        _ => false,
    }
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

    // --- Fix 4: retry status policy is exercised without a network round-trip.
    #[test]
    fn is_retryable_status_covers_429_and_5xx_only() {
        for c in [429u16, 500, 502, 503, 504, 599] {
            assert!(is_retryable_status(c), "{c} should be retryable");
        }
        for c in [200u16, 201, 400, 401, 403, 404, 409, 418, 428, 499, 600] {
            assert!(!is_retryable_status(c), "{c} should be terminal");
        }
    }

    // --- Fix 1: a completion is non-idempotent, so only pre-send transport
    // failures may be retried. These two tests pin the boundary.
    #[test]
    fn transport_retryable_true_for_presend_failures() {
        use ureq::{Error, Timeout};
        assert!(transport_retryable(&Error::HostNotFound));
        assert!(transport_retryable(&Error::ConnectionFailed));
        assert!(transport_retryable(&Error::ConnectProxyFailed(
            "proxy".into()
        )));
        assert!(transport_retryable(&Error::Tls("handshake")));
        assert!(transport_retryable(&Error::TlsRequired));
        // rustls handshake failure — pre-send, so retryable (Fix 2).
        assert!(transport_retryable(&Error::Rustls(
            rustls::Error::HandshakeNotComplete
        )));
        assert!(transport_retryable(&Error::Timeout(Timeout::Resolve)));
        assert!(transport_retryable(&Error::Timeout(Timeout::Connect)));
        assert!(transport_retryable(&Error::Timeout(Timeout::SendRequest)));
        assert!(transport_retryable(&Error::Timeout(Timeout::SendBody)));
        assert!(transport_retryable(&Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        ))));
    }

    #[test]
    fn transport_retryable_false_once_request_may_be_processed() {
        // The regression this guards: a receive-phase / global timeout means
        // Anthropic may have already run and billed the completion. Retrying
        // would double-spend, so it must be surfaced, not retried.
        use ureq::{Error, Timeout};
        assert!(!transport_retryable(&Error::Timeout(Timeout::Global)));
        assert!(!transport_retryable(&Error::Timeout(Timeout::PerCall)));
        assert!(!transport_retryable(&Error::Timeout(Timeout::RecvResponse)));
        assert!(!transport_retryable(&Error::Timeout(Timeout::RecvBody)));
        // A mid-stream reset could be post-send — don't gamble on a duplicate.
        assert!(!transport_retryable(&Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        ))));
    }

    // --- Fix 3: response flattening + the empty-content guard, unit-tested.
    fn block(kind: &str, text: &str) -> WireBlock {
        WireBlock {
            kind: kind.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn extract_text_joins_text_blocks_and_skips_non_text() {
        let resp = WireResponse {
            content: vec![
                block("text", "hello "),
                block("tool_use", "IGNORE_ME"),
                block("text", "world"),
            ],
        };
        assert_eq!(extract_text(resp).unwrap(), "hello world");
    }

    #[test]
    fn extract_text_errors_when_only_non_text_blocks() {
        let resp = WireResponse {
            content: vec![block("tool_use", "{}"), block("thinking", "...")],
        };
        assert!(extract_text(resp).is_err());
    }

    #[test]
    fn extract_text_errors_on_whitespace_only() {
        let resp = WireResponse {
            content: vec![block("text", "   \n\t")],
        };
        assert!(extract_text(resp).is_err());
    }

    // --- Fix 1: the timeout policy gives the response its own budget, with no
    // shared global cap a slow connect could eat into.
    #[test]
    fn agent_timeouts_give_recv_its_own_budget_independent_of_connect() {
        let t = agent_config().timeouts();
        assert_eq!(t.connect, Some(HTTP_CONNECT_TIMEOUT));
        assert_eq!(t.recv_response, Some(HTTP_TIMEOUT));
        assert_eq!(t.recv_body, Some(HTTP_TIMEOUT));
        // The regression this guards: reverting to a single `timeout_global`
        // sets `global = Some(..)` and leaves recv budgets `None`, so a slow
        // connect would starve the body read.
        assert_eq!(t.global, None);
        assert_ne!(t.recv_body, t.connect);
    }

    // --- Fix 3: the explicit body cap is actually applied at the read site, so
    // an oversized body errors instead of parsing a truncation.
    #[test]
    fn read_capped_json_parses_within_limit_and_errors_past_it() {
        let json = br#"{"content":[{"type":"text","text":"hi"}]}"#;

        let mut within = ureq::Body::builder().data(json.to_vec());
        let parsed: WireResponse = read_capped_json(&mut within, RESPONSE_BODY_LIMIT).unwrap();
        assert_eq!(extract_text(parsed).unwrap(), "hi");

        // A limit below the payload size must fail the read, not silently
        // truncate — the guard that keeps a dropped `.limit()` from regressing.
        let mut over = ureq::Body::builder().data(json.to_vec());
        assert!(read_capped_json::<WireResponse>(&mut over, 4).is_err());
    }

    #[test]
    fn read_capped_string_respects_limit() {
        let mut within = ureq::Body::builder().data(b"hello world".to_vec());
        assert_eq!(
            read_capped_string(&mut within, RESPONSE_BODY_LIMIT).unwrap(),
            "hello world"
        );
        let mut over = ureq::Body::builder().data(b"hello world".to_vec());
        assert!(read_capped_string(&mut over, 3).is_err());
    }
}
