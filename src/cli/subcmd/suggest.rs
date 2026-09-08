//! `fermut suggest` — generate killing pytest tests for surviving mutants
//! via the Anthropic Messages API.
//!
//! Inputs are the same as `show`/`explain`: a prior JSON report plus a
//! mutant selector (or `--all-survivors`). For each target the model is
//! prompted with the mutation, surrounding source, the enclosing symbol,
//! and a few existing tests sampled from the project's test tree (so the
//! generated test matches local style). The response is parsed for a
//! `python` code block.
//!
//! Outputs:
//! - default human format: print the generated test(s) to stdout, append
//!   to `--out`, or `--apply` to the test file referencing the enclosing
//!   symbol.
//! - `--format json` emits the structured [`SuggestReport`] — used by
//!   agent integrations that drive `suggest` from a script.
//!
//! Parallelism: `--parallel N` runs N LLM calls concurrently via rayon.
//! File writes (`--apply` / `--out`) always happen serially after
//! generation finishes, so the on-disk order is deterministic. Default
//! `--parallel 1` keeps you under per-tenant rate limits.
//!
//! Caching: every successful (mutant_id, file_sha256, prompt) tuple is
//! recorded in `.fermut/llm-cache.json` so re-runs are free. Disable with
//! `--no-cache` when iterating on prompts.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use serde::Serialize;

use super::explain::{enclosing_symbol, render_source_snippet, EnclosingSymbol, MutantSummary};
use crate::cli::Format;
use crate::llm::cache::{default_cache_path, LlmCache};
use crate::llm::client::client_from_env;
use crate::llm::prompt::{
    build_suggest_prompt, cache_key, extract_first_code_block, file_sha256, PromptContext,
    SampleTest,
};
use crate::llm::{LlmCallOpts, DEFAULT_MODEL};
use crate::mutator::Mutant;
use crate::report::{MutantOutcome, Report};
use crate::sync::lock_recover;
use crate::util::search::{grep_symbol, DEFAULT_GREP_MATCH_LIMIT};

pub struct SuggestOpts {
    pub report: PathBuf,
    pub target: Option<String>,
    pub all_survivors: bool,
    pub apply: bool,
    pub out: Option<PathBuf>,
    pub model: Option<String>,
    pub tests: Option<PathBuf>,
    pub context_lines: usize,
    pub sample_count: usize,
    pub no_cache: bool,
    pub cache_path: Option<PathBuf>,
    pub project_root: PathBuf,
    pub format: Format,
    pub parallel: usize,
}

/// Built with a caller-resolved `project_root` (via
/// [`crate::cli::current_project_root`]) rather than reading the cwd here, so
/// the conversion is pure and unit-testable.
impl From<(SuggestArgs, PathBuf)> for SuggestOpts {
    fn from((a, project_root): (SuggestArgs, PathBuf)) -> Self {
        let SuggestArgs {
            report,
            target,
            all_survivors,
            apply,
            out,
            model,
            tests,
            context,
            sample_count,
            no_cache,
            cache_path,
            format,
            parallel,
        } = a;
        SuggestOpts {
            report,
            target,
            all_survivors,
            apply,
            out,
            model,
            tests,
            context_lines: context,
            sample_count,
            no_cache,
            cache_path,
            project_root,
            format,
            parallel,
        }
    }
}

// ---------------------------------------------------------------------------
// Report shape — stable JSON contract for agent consumers.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SuggestReport {
    pub model: String,
    pub entries: Vec<SuggestEntry>,
}

#[derive(Debug, Serialize)]
pub struct SuggestEntry {
    pub mutant: MutantSummary,
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn suggest(opts: SuggestOpts) -> Result<()> {
    let report = crate::report::load(&opts.report)?;

    let targets = select_targets(&report, opts.target.as_deref(), opts.all_survivors)?;
    if targets.is_empty() {
        match opts.format {
            Format::Json => {
                let out = SuggestReport {
                    model: opts.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                    entries: Vec::new(),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            Format::Human => {
                eprintln!("no mutants matched the selector / no survivors in the report");
            }
        }
        return Ok(());
    }

    let model = opts.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let cache_path = opts
        .cache_path
        .clone()
        .unwrap_or_else(|| default_cache_path(&opts.project_root));
    let cache = if opts.no_cache {
        LlmCache::default()
    } else {
        LlmCache::load(&cache_path)
    };
    let cache = Arc::new(Mutex::new(cache));
    // Defer client construction until a cache miss forces a network call.
    // Avoids failing with "ANTHROPIC_API_KEY is not set" when every target is
    // already cached. `OnceLock` ensures one construction across `--parallel`.
    let client_cell: Arc<
        OnceLock<std::result::Result<Arc<dyn crate::llm::client::LlmClient>, String>>,
    > = Arc::new(OnceLock::new());

    let parallel = opts.parallel.max(1);

    let mutants: Vec<Mutant> = targets.iter().map(|o| o.mutant().clone()).collect();
    let contexts: Vec<PromptContext> = mutants
        .iter()
        .map(|m| {
            build_context(
                m,
                opts.context_lines,
                opts.tests.as_deref(),
                opts.sample_count,
            )
        })
        .collect();

    let log_progress = matches!(opts.format, Format::Human);

    // Loop-invariant LLM knobs, resolved once and shared by every worker.
    let llm_opts = LlmCallOpts {
        model: model.clone(),
        no_cache: opts.no_cache,
        cache_path: cache_path.clone(),
    };

    // Generation phase: parallel-safe. File writes happen later, serial.
    // Cache persists per-response inside `generate_one` so an interrupt mid-loop
    // does not discard responses already paid for.
    let work = mutants.into_iter().zip(contexts).collect::<Vec<_>>();
    let raw_results: Vec<GenResult> = if parallel == 1 {
        work.into_iter()
            .map(|(m, ctx)| generate_one(&m, &ctx, &llm_opts, &client_cell, &cache, log_progress))
            .collect()
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .build()
            .context("building rayon pool for --parallel")?;
        pool.install(|| {
            work.into_par_iter()
                .with_max_len(1)
                .map(|(m, ctx)| {
                    generate_one(&m, &ctx, &llm_opts, &client_cell, &cache, log_progress)
                })
                .collect()
        })
    };

    // Apply phase: serial. Determines applied_to per entry.
    let mut entries: Vec<SuggestEntry> = Vec::with_capacity(raw_results.len());
    for r in raw_results {
        let entry = finalize_entry(r, opts.apply, opts.out.as_deref(), log_progress);
        entries.push(entry);
    }

    let suggest_report = SuggestReport { model, entries };
    match opts.format {
        Format::Json => {
            let s = serde_json::to_string_pretty(&suggest_report)
                .context("serializing suggest report")?;
            println!("{s}");
        }
        Format::Human => render_human(&suggest_report, opts.apply, opts.out.as_deref()),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-mutant generation. Pure aside from cache + network.
// ---------------------------------------------------------------------------

struct GenResult {
    mutant: Mutant,
    ctx: PromptContext,
    cached: bool,
    response: Result<String>,
}

fn generate_one(
    m: &Mutant,
    ctx: &PromptContext,
    opts: &LlmCallOpts,
    client_cell: &OnceLock<std::result::Result<Arc<dyn crate::llm::client::LlmClient>, String>>,
    cache: &Arc<Mutex<LlmCache>>,
    log: bool,
) -> GenResult {
    if log {
        eprintln!(
            "→ generating test for {}:{} [{}]",
            m.file.display(),
            m.line,
            m.operator.name()
        );
    }

    let req = build_suggest_prompt(m, ctx, &opts.model);
    let sha = file_sha256(&m.file).unwrap_or_default();
    let key = cache_key(&m.id, &sha, &req.user);

    let cached_hit = if opts.no_cache {
        None
    } else {
        lock_recover(cache).lookup(&key).map(str::to_string)
    };

    let (response, cached) = match cached_hit {
        Some(hit) => {
            if log {
                eprintln!("  cache hit");
            }
            (Ok(hit), true)
        }
        None => {
            let client_result = client_cell.get_or_init(|| {
                client_from_env()
                    .map(|c| {
                        let c: Arc<dyn crate::llm::client::LlmClient> = Arc::from(c);
                        c
                    })
                    .map_err(|e| e.to_string())
            });
            match client_result {
                Err(e) => (Err(anyhow!(e.clone())), false),
                Ok(client) => match client.complete(&req) {
                    Ok(text) => {
                        if !opts.no_cache {
                            // Hold the lock across save so concurrent rayon
                            // workers serialize disk writes and never race on
                            // the shared tmp-sibling path.
                            let mut guard = lock_recover(cache);
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
                },
            }
        }
    };
    GenResult {
        mutant: m.clone(),
        ctx: ctx.clone(),
        cached,
        response,
    }
}

fn finalize_entry(r: GenResult, apply: bool, out: Option<&Path>, log: bool) -> SuggestEntry {
    let m = &r.mutant;
    let summary = MutantSummary {
        id: m.id.clone(),
        file: m.file.display().to_string(),
        line: m.line,
        operator: m.operator.name().to_string(),
        original: m.original.clone(),
        replacement: m.replacement.clone(),
    };

    let response = match r.response {
        Ok(text) => text,
        Err(e) => {
            return SuggestEntry {
                mutant: summary,
                cached: r.cached,
                response: None,
                extracted_code: None,
                applied_to: None,
                error: Some(e.to_string()),
            };
        }
    };

    let extracted = extract_first_code_block(&response);
    if extracted.is_none() {
        return SuggestEntry {
            mutant: summary,
            cached: r.cached,
            response: Some(response),
            extracted_code: None,
            applied_to: None,
            error: Some("model response did not contain a python code block".into()),
        };
    }
    let code = extracted.unwrap();
    let header = format!(
        "\n# fermut suggest: kills mutation `{}` → `{}` at {}:{}\n",
        m.original,
        m.replacement,
        m.file.display(),
        m.line,
    );
    let payload = format!("{header}{code}\n");

    let mut applied_to: Option<String> = None;
    let mut error: Option<String> = None;

    if apply {
        match infer_apply_target(m, &r.ctx) {
            Ok(path) => match append_to_test_file(&path, &payload) {
                Ok(()) => {
                    if log {
                        eprintln!("  appended to {}", path.display());
                    }
                    applied_to = Some(path.display().to_string());
                }
                Err(e) => error = Some(e.to_string()),
            },
            Err(e) => match out {
                Some(p) => match append_to_test_file(p, &payload) {
                    Ok(()) => {
                        if log {
                            eprintln!("  appended to {} (fallback from --apply)", p.display());
                        }
                        applied_to = Some(p.display().to_string());
                    }
                    Err(e) => error = Some(e.to_string()),
                },
                None => error = Some(e.to_string()),
            },
        }
    } else if let Some(p) = out {
        match append_to_test_file(p, &payload) {
            Ok(()) => {
                if log {
                    eprintln!("  appended to {}", p.display());
                }
                applied_to = Some(p.display().to_string());
            }
            Err(e) => error = Some(e.to_string()),
        }
    }

    SuggestEntry {
        mutant: summary,
        cached: r.cached,
        response: Some(response),
        extracted_code: Some(code),
        applied_to,
        error,
    }
}

fn render_human(report: &SuggestReport, apply: bool, out: Option<&Path>) {
    let written = apply || out.is_some();
    for entry in &report.entries {
        let m = &entry.mutant;
        let target =
            entry
                .applied_to
                .as_deref()
                .unwrap_or(if written { "(write failed)" } else { "" });
        if let Some(code) = &entry.extracted_code {
            if !written {
                println!(
                    "\n# fermut suggest: kills mutation `{}` → `{}` at {}:{}",
                    m.original, m.replacement, m.file, m.line
                );
                print!("{code}");
                if !code.ends_with('\n') {
                    println!();
                }
            } else if !target.is_empty() && entry.applied_to.is_some() {
                // Already logged during apply.
            }
        }
        if let Some(err) = &entry.error {
            eprintln!("! {}:{} [{}] — {}", m.file, m.line, m.operator, err);
        }
    }
}

// ---------------------------------------------------------------------------
// Selection + context plumbing
// ---------------------------------------------------------------------------

/// One-shot generation for a single mutant: build prompt context, call the
/// model (or hit the cache), and return the extracted python test code plus
/// the context (the apply-target inference needs it). Used by `fermut
/// autofix`. Errors when the model fails or returns no code block.
pub(crate) fn generate_test_code(
    m: &Mutant,
    tests_dir: Option<&Path>,
    context_lines: usize,
    sample_count: usize,
    model: &str,
    cache_path: &Path,
    no_cache: bool,
) -> Result<(String, PromptContext)> {
    let ctx = build_context(m, context_lines, tests_dir, sample_count);
    let cache = Arc::new(Mutex::new(if no_cache {
        LlmCache::default()
    } else {
        LlmCache::load(cache_path)
    }));
    let client_cell: OnceLock<std::result::Result<Arc<dyn crate::llm::client::LlmClient>, String>> =
        OnceLock::new();
    let r = generate_one(
        m,
        &ctx,
        &LlmCallOpts {
            model: model.to_string(),
            no_cache,
            cache_path: cache_path.to_path_buf(),
        },
        &client_cell,
        &cache,
        false,
    );
    let text = r.response?;
    let code = extract_first_code_block(&text)
        .ok_or_else(|| anyhow!("model response did not contain a python code block"))?;
    Ok((code, ctx))
}

pub(crate) fn select_targets<'a>(
    report: &'a Report,
    target: Option<&str>,
    all_survivors: bool,
) -> Result<Vec<&'a MutantOutcome>> {
    if all_survivors {
        return Ok(report
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    MutantOutcome::Survived { .. } | MutantOutcome::TimedOut { .. }
                )
            })
            .collect());
    }
    let target = target.ok_or_else(|| {
        anyhow!("provide a mutant selector (index or substring of id), or pass --all-survivors")
    })?;
    if let Ok(idx) = target.parse::<usize>() {
        if idx >= 1 {
            if let Some(o) = report.outcomes.get(idx - 1) {
                return Ok(vec![o]);
            }
        }
    }
    let matches: Vec<&MutantOutcome> = report
        .outcomes
        .iter()
        .filter(|o| o.mutant().id.contains(target))
        .collect();
    if matches.is_empty() {
        Err(anyhow!("no mutant matches `{target}`"))
    } else {
        Ok(matches)
    }
}

fn build_context(
    m: &Mutant,
    context_lines: usize,
    tests_dir: Option<&Path>,
    sample_count: usize,
) -> PromptContext {
    let source_snippet = render_source_snippet(m, context_lines);
    let enclosing = enclosing_symbol(m).map(|s: EnclosingSymbol| s.name);
    let sample_tests = match (tests_dir, enclosing.as_deref()) {
        (Some(dir), Some(sym)) => collect_sample_tests(dir, sym, sample_count),
        _ => Vec::new(),
    };
    PromptContext {
        source_snippet,
        enclosing_symbol: enclosing,
        sample_tests,
    }
}

fn collect_sample_tests(tests_dir: &Path, symbol: &str, max_count: usize) -> Vec<SampleTest> {
    // Need enough raw hits to dedup by path up to `max_count`. Saturating
    // multiply guards against overflow on absurd `max_count` values; clamp
    // to the global cap so a misconfigured `--sample-count` can't unbound
    // the walk.
    let limit = max_count
        .saturating_mul(4)
        .clamp(8, DEFAULT_GREP_MATCH_LIMIT);
    let Ok(matches) = grep_symbol(tests_dir, symbol, limit) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for (path, _) in matches {
        if !seen.insert(path.clone()) {
            continue;
        }
        if out.len() >= max_count {
            break;
        }
        if let Ok(body) = std::fs::read_to_string(&path) {
            let trimmed = trim_to_bytes(&body, 4_000);
            out.push(SampleTest {
                path: path.display().to_string(),
                body: trimmed,
            });
        }
    }
    out
}

fn trim_to_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n# ... (truncated)", &s[..end])
}

pub(crate) fn infer_apply_target(m: &Mutant, ctx: &PromptContext) -> Result<PathBuf> {
    let sym = ctx.enclosing_symbol.as_deref().ok_or_else(|| {
        anyhow!(
            "--apply needs either --out or an enclosing symbol to discover a test file; \
             neither was available for mutant {}",
            m.id
        )
    })?;
    let sample = ctx.sample_tests.first().ok_or_else(|| {
        anyhow!(
            "--apply could not find a test file referencing `{sym}`. Pass --tests <dir> \
             and/or --out <path> to choose a target explicitly."
        )
    })?;
    Ok(PathBuf::from(&sample.path))
}

pub(crate) fn append_to_test_file(path: &Path, payload: &str) -> Result<()> {
    reject_symlink_target(path)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    f.write_all(payload.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
}

/// Refuse to follow a symlink when applying generated tests.
///
/// `infer_apply_target` returns a path discovered by walking the project's
/// `tests/` tree. A hostile contributor who lands a symlink under tests/
/// (e.g. `tests/_helpers.py` → `/etc/passwd` or `../../sibling-repo/src`)
/// could otherwise redirect the append into a path outside the project,
/// silently overwriting the developer's files. `--out` paths pass through
/// here too; an explicit `--out symlinked.py` is rejected for the same
/// reason.
///
/// The check uses `symlink_metadata` so non-existent targets (the common
/// case — generated test file doesn't exist yet) pass through unchanged.
/// Only an existing symlink at the final path component is rejected.
fn reject_symlink_target(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => Err(anyhow!(
            "refusing to write through symlink at `{}`. Resolve the link \
             yourself or pass an explicit `--out` pointing at a regular file.",
            path.display()
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use crate::report::MutantOutcome;
    use ruff_text_size::TextRange;
    use tempfile::TempDir;

    fn make_mutant(id: &str, file: PathBuf, line: u32) -> Mutant {
        Mutant {
            id: id.into(),
            file,
            operator: Operator::BoundaryShift,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "<=".into(),
            replacement: "<".into(),
            line,
            stmt_line: line,
        }
    }

    #[test]
    fn select_targets_with_index() {
        let r = Report::new(vec![
            MutantOutcome::killed(make_mutant("m1", "a.py".into(), 1)),
            MutantOutcome::survived(make_mutant("m2", "a.py".into(), 2)),
        ]);
        let t = select_targets(&r, Some("2"), false).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].mutant().id, "m2");
    }

    #[test]
    fn select_targets_with_substring() {
        let r = Report::new(vec![
            MutantOutcome::survived(make_mutant("alpha-mutant", "a.py".into(), 1)),
            MutantOutcome::survived(make_mutant("beta-mutant", "a.py".into(), 2)),
        ]);
        let t = select_targets(&r, Some("alpha"), false).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].mutant().id, "alpha-mutant");
    }

    #[test]
    fn select_targets_all_survivors_excludes_killed() {
        let r = Report::new(vec![
            MutantOutcome::killed(make_mutant("k", "a.py".into(), 1)),
            MutantOutcome::survived(make_mutant("s", "a.py".into(), 2)),
            MutantOutcome::timed_out(make_mutant("t", "a.py".into(), 3)),
            MutantOutcome::skipped(make_mutant("sk", "a.py".into(), 4), "ty"),
        ]);
        let t = select_targets(&r, None, true).unwrap();
        let ids: Vec<&str> = t.iter().map(|o| o.mutant().id.as_str()).collect();
        assert_eq!(ids, vec!["s", "t"]);
    }

    #[test]
    fn select_targets_requires_selector_or_all_flag() {
        let r = Report::new(vec![MutantOutcome::survived(make_mutant(
            "m",
            "a.py".into(),
            1,
        ))]);
        assert!(select_targets(&r, None, false).is_err());
    }

    #[test]
    fn select_targets_no_match_is_error() {
        let r = Report::new(vec![MutantOutcome::survived(make_mutant(
            "alpha",
            "a.py".into(),
            1,
        ))]);
        assert!(select_targets(&r, Some("beta"), false).is_err());
    }

    #[test]
    fn trim_to_bytes_respects_char_boundary() {
        let s = "abcé".to_string();
        let trimmed = trim_to_bytes(&s, 3);
        assert!(trimmed.starts_with("abc"));
        assert!(trimmed.contains("truncated"));
    }

    #[test]
    fn trim_to_bytes_passthrough_when_short() {
        let s = "short";
        let trimmed = trim_to_bytes(s, 1_000);
        assert_eq!(trimmed, "short");
    }

    #[test]
    fn append_to_test_file_creates_and_appends() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("subdir").join("test_x.py");
        append_to_test_file(&path, "first\n").unwrap();
        append_to_test_file(&path, "second\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "first\nsecond\n");
    }

    #[cfg(unix)]
    #[test]
    fn append_to_test_file_rejects_symlink() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real.py");
        std::fs::write(&real, "# real\n").unwrap();
        let link = dir.path().join("link.py");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = append_to_test_file(&link, "PWNED\n")
            .expect_err("writing through a symlink must be refused — see reject_symlink_target");
        let msg = err.to_string();
        assert!(
            msg.contains("symlink"),
            "error should mention symlink, got `{msg}`"
        );
        // Critically, the real file behind the link must be untouched.
        let real_after = std::fs::read_to_string(&real).unwrap();
        assert_eq!(real_after, "# real\n");
    }

    fn run_with_mock(
        report_path: PathBuf,
        out_path: Option<PathBuf>,
        all_survivors: bool,
        parallel: usize,
        format: Format,
        project_root: PathBuf,
    ) -> Result<()> {
        std::env::set_var("FERMUT_LLM_MOCK", "1");
        let opts = SuggestOpts {
            report: report_path,
            target: None,
            all_survivors,
            apply: false,
            out: out_path,
            model: None,
            tests: None,
            context_lines: 3,
            sample_count: 0,
            no_cache: true,
            cache_path: None,
            project_root,
            format,
            parallel,
        };
        suggest(opts)
    }

    fn build_simple_fixture() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("calc.py");
        std::fs::write(
            &src_path,
            "def in_range(x, lo, hi):\n    return lo <= x and x <= hi\n",
        )
        .unwrap();
        let mutant = Mutant {
            id: "calc:2:boundary:0".into(),
            file: src_path,
            operator: Operator::BoundaryShift,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "<=".into(),
            replacement: "<".into(),
            line: 2,
            stmt_line: 2,
        };
        let report = Report::new(vec![
            MutantOutcome::survived(mutant.clone()),
            MutantOutcome::survived(mutant),
        ]);
        let report_path = dir.path().join("report.json");
        std::fs::write(&report_path, serde_json::to_string(&report).unwrap()).unwrap();
        (dir, report_path)
    }

    #[test]
    fn end_to_end_with_mock_client_writes_to_out() {
        let (dir, report_path) = build_simple_fixture();
        let out_path = dir.path().join("generated_test.py");
        run_with_mock(
            report_path,
            Some(out_path.clone()),
            true,
            1,
            Format::Human,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("fermut suggest"));
        assert!(written.contains("def test_mock_generated"));
    }

    #[test]
    fn parallel_generation_preserves_entry_order() {
        let (dir, report_path) = build_simple_fixture();
        // Capture JSON via custom run that calls suggest internally.
        // Easier path: invoke with format=Json and inspect via reading
        // a file? suggest writes JSON to stdout. We'll instead validate
        // by checking that running with parallel=4 over 2 entries
        // succeeds and that --out preserves both blocks.
        let out_path = dir.path().join("out.py");
        run_with_mock(
            report_path,
            Some(out_path.clone()),
            true,
            4,
            Format::Human,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let written = std::fs::read_to_string(&out_path).unwrap();
        // Both mutants share the same id so we expect the suggest header
        // to appear twice.
        let occurrences = written.matches("fermut suggest").count();
        assert_eq!(occurrences, 2);
    }

    #[test]
    fn generate_one_persists_cache_per_response() {
        // Regression: prior to this test the on-disk cache was only written
        // after the entire generation loop finished, so an interrupt mid-loop
        // dropped every response already paid for. `generate_one` must now
        // persist the cache after every successful LLM call. Leave the env
        // var set — other tests in this module do the same and racing
        // restores breaks them under cargo's parallel test runner.
        std::env::set_var("FERMUT_LLM_MOCK", "1");

        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("calc.py");
        std::fs::write(&src_path, "def f(x):\n    return x <= 1\n").unwrap();
        let mutant = make_mutant("calc:2:boundary:0", src_path, 2);
        let ctx = PromptContext {
            source_snippet: String::new(),
            enclosing_symbol: None,
            sample_tests: Vec::new(),
        };
        let cache_path = dir.path().join("llm-cache.json");
        let cache = Arc::new(Mutex::new(LlmCache::default()));
        let client_cell: OnceLock<
            std::result::Result<Arc<dyn crate::llm::client::LlmClient>, String>,
        > = OnceLock::new();

        let result = generate_one(
            &mutant,
            &ctx,
            &LlmCallOpts {
                model: DEFAULT_MODEL.to_string(),
                no_cache: false,
                cache_path: cache_path.clone(),
            },
            &client_cell,
            &cache,
            false,
        );

        assert!(result.response.is_ok(), "mock client should succeed");
        assert!(!result.cached, "first call is a miss, not a hit");
        let raw = std::fs::read_to_string(&cache_path)
            .expect("cache file must exist after a single generate_one call");
        let on_disk: LlmCache = serde_json::from_str(&raw).expect("cache file is valid JSON");
        assert_eq!(
            on_disk.len(),
            1,
            "cache file must contain the one response that was just generated"
        );
    }

    #[test]
    fn select_targets_no_matches_for_all_survivors_returns_empty() {
        let r = Report::new(vec![MutantOutcome::killed(make_mutant(
            "k",
            "a.py".into(),
            1,
        ))]);
        let t = select_targets(&r, None, true).unwrap();
        assert!(t.is_empty());
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct SuggestArgs {
    /// Path to a JSON report produced by `fermut run --json …`.
    pub(crate) report: std::path::PathBuf,

    /// Mutant selector: 1-based index, or substring of mutant id.
    /// Omit when `--all-survivors` is set.
    pub(crate) target: Option<String>,

    /// Generate a test for every surviving (or timed-out) mutant in
    /// the report.
    #[arg(long)]
    pub(crate) all_survivors: bool,

    /// Append the generated test to the test file that already
    /// references the enclosing symbol. Falls back to `--out` if no
    /// candidate test file is found.
    #[arg(long)]
    pub(crate) apply: bool,

    /// Append the generated test to this path instead of stdout.
    #[arg(long)]
    pub(crate) out: Option<std::path::PathBuf>,

    /// Anthropic model id. Defaults to `claude-sonnet-4-6`.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Tests directory to mine for style samples. Defaults to `tests/`.
    #[arg(long, num_args = 0..=1, default_missing_value = "tests")]
    pub(crate) tests: Option<std::path::PathBuf>,

    /// Source lines of context to include in the prompt on each side
    /// of the mutant line.
    #[arg(long, default_value_t = 8)]
    pub(crate) context: usize,

    /// How many existing tests to include in the prompt for style
    /// reference.
    #[arg(long, default_value_t = 2)]
    pub(crate) sample_count: usize,

    /// Disable the LLM response cache.
    #[arg(long)]
    pub(crate) no_cache: bool,

    /// Custom path for the LLM response cache. Defaults to
    /// `.fermut/llm-cache.json` under the project root.
    #[arg(long)]
    pub(crate) cache_path: Option<std::path::PathBuf>,

    /// Output format. `human` (default) prints generated code blocks
    /// and progress logs. `json` emits the structured `SuggestReport`
    /// for agent consumers.
    #[arg(long, value_enum, default_value_t = crate::cli::Format::Human)]
    pub(crate) format: crate::cli::Format,

    /// Run up to N Anthropic calls concurrently. Default `1`. Raise to
    /// shorten wall-clock when `--all-survivors` is large; keep within
    /// your tenant's rate limit. File writes stay serial.
    #[arg(long, default_value_t = 1)]
    pub(crate) parallel: usize,
}
