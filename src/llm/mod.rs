//! LLM integration: Anthropic Messages API client + prompt builders + cache.
//!
//! Used by `fermut suggest` (generate killing tests) and `fermut explain
//! --llm` (richer survivor explanations). Pure HTTP via `ureq`; no async
//! runtime, no tokio. Mockable via `FERMUT_LLM_MOCK` for offline tests.
//!
//! ## Module shape
//!
//! - `client` — `LlmClient` trait + `AnthropicClient` impl + `MockClient`.
//! - `prompt` — pure functions building system + user messages from a
//!   `Mutant` and project context. No I/O.
//! - `cache` — persistent JSON cache keyed by (mutant.id, file sha256,
//!   prompt sha256). Same shape as the result cache.
//!
//! Auth: `ANTHROPIC_API_KEY` env var (or `FERMUT_ANTHROPIC_API_KEY` as an
//! alias for env scoping). Default model: `claude-sonnet-4-6`.

pub mod cache;
pub mod client;
pub mod prompt;
pub mod prompt_equiv;
pub mod redact;

pub use cache::LlmCache;
pub use client::{client_from_env, LlmClient, LlmRequest, MockClient};
pub use prompt::{build_explain_prompt, build_suggest_prompt, PromptContext};
pub use prompt_equiv::{build_equivalence_prompt, PROMPT_TEMPLATE_VERSION};

/// Default Anthropic model used when neither config nor `--model` overrides.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Default max tokens for completions. Tests are small; this is plenty.
pub const DEFAULT_MAX_TOKENS: u32 = 2048;
