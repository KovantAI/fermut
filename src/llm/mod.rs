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
// TODO: not yet wired into the pipeline (no CLI flag constructs it); kept
// pending a wire-up-or-delete decision. Remove the allow once reachable.
#[allow(dead_code)]
pub mod prompt_equiv;
pub mod redact;

pub use client::LlmClient;

/// Default Anthropic model used when neither config nor `--model` overrides.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Loop-invariant LLM call configuration shared by `fermut explain --llm` and
/// `fermut suggest`: which model, whether to bypass the cache, and where the
/// cache file lives. Built once per command (model and cache path already
/// resolved) and passed by reference to each per-mutant call, replacing the
/// repeated `(model, no_cache, cache_path)` argument trio.
#[derive(Clone, Debug)]
pub struct LlmCallOpts {
    pub model: String,
    pub no_cache: bool,
    pub cache_path: std::path::PathBuf,
}

/// Default max tokens for completions. Tests are small; this is plenty.
pub const DEFAULT_MAX_TOKENS: u32 = 2048;
