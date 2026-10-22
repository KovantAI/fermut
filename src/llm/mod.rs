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

pub use client::LlmClient;

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};

use cache::LlmCache;
use client::{client_from_env, LlmRequest};

/// The client, built on the first cache miss and shared by every caller in a
/// command (`--parallel` workers included). Lazy so a fully cached run needs
/// no API key. A pre-set cell is the test seam for a canned client.
pub(crate) type ClientCell = OnceLock<std::result::Result<Arc<dyn LlmClient>, String>>;

/// `req`'s response from the cache under `key`, else from the model (then
/// cached and persisted at once, so an interrupted run keeps what it paid
/// for). The bool says whether it was a cache hit.
pub(crate) fn complete_cached(
    req: &LlmRequest,
    key: String,
    opts: &LlmCallOpts,
    client_cell: &ClientCell,
    cache: &Mutex<LlmCache>,
) -> (Result<String>, bool) {
    if !opts.no_cache {
        if let Some(hit) = crate::sync::lock_recover(cache).lookup(&key) {
            return (Ok(hit.to_string()), true);
        }
    }
    let client = client_cell.get_or_init(|| {
        client_from_env()
            .map(Arc::<dyn LlmClient>::from)
            .map_err(|e| e.to_string())
    });
    let client = match client {
        Ok(c) => c,
        Err(e) => return (Err(anyhow!(e.clone())), false),
    };
    match client.complete(req) {
        Ok(text) => {
            if !opts.no_cache {
                // Hold the lock across save so concurrent workers serialize
                // disk writes and never race on the shared tmp-sibling path.
                let mut guard = crate::sync::lock_recover(cache);
                guard.insert(key, text.clone());
                if let Err(e) = guard.save(&opts.cache_path) {
                    eprintln!(
                        "  warning: persisting llm cache to {} failed: {e}",
                        opts.cache_path.display()
                    );
                }
            }
            (Ok(text), false)
        }
        Err(e) => (Err(e), false),
    }
}

/// One command's LLM state: call options, the lazily built client, and the
/// response cache (loaded once, unless `no_cache`).
pub(crate) struct LlmSession {
    pub opts: LlmCallOpts,
    pub client_cell: ClientCell,
    pub cache: Mutex<LlmCache>,
}

impl LlmSession {
    pub(crate) fn new(opts: LlmCallOpts) -> Self {
        let cache = if opts.no_cache {
            LlmCache::default()
        } else {
            LlmCache::load(&opts.cache_path)
        };
        Self {
            opts,
            client_cell: OnceLock::new(),
            cache: Mutex::new(cache),
        }
    }
}

/// `--triage` for `autofix` / `suggest`: ask the LLM equivalence judge about
/// each survivor before generating a test for it. Advisory only; the
/// mutation score never changes. See [`crate::equiv::llm`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum TriageMode {
    /// No judge call (default).
    #[default]
    Off,
    /// Skip survivors judged equivalent; feed the judge's explanation of a
    /// behavioural difference into test generation.
    Skip,
    /// Judge every survivor but still generate (and, in autofix, verify):
    /// a mutant judged equivalent that autofix kills is a measured false
    /// positive.
    Audit,
}

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
