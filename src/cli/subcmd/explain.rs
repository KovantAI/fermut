//! `fermut explain` — diagnose *why* a survivor likely survived and suggest
//! the next test to write.
//!
//! Reads a prior JSON report (same input as `show`), locates one mutant by id
//! or 1-based index, then layers extra signal on top of the bare detail view:
//!
//! - Surrounding source context with the mutant line marked.
//! - Enclosing `def`/`class` discovered by a backward indent scan.
//! - Operator-specific hint ("tests likely lack equality-boundary case", etc).
//! - Coverage signal — line covered / by which tests — when `--coverage` is
//!   passed.
//! - Closest tests by symbol — grep of the tests tree — when `--tests` is
//!   passed.
//! - A pytest skeleton the user can paste to start the kill.
//! - Optional Anthropic-generated prose + killing test when `--llm` is set.
//!
//! The command first builds an [`ExplainReport`] from heuristics + optional
//! LLM call, then renders it. `--format human` (default) prints the prose
//! laid out for terminals; `--format json` emits the report as machine-
//! readable JSON for agent consumption.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::filter::coverage::CoverageContexts;
use crate::llm::cache::{default_cache_path, LlmCache};
use crate::llm::client::client_from_env;
use crate::llm::prompt::{
    build_explain_prompt, cache_key, extract_first_code_block, file_sha256, PromptContext,
    SampleTest,
};
use crate::llm::DEFAULT_MODEL;
use crate::mutator::{Mutant, Operator};
use crate::report::{MutantOutcome, Report};

#[derive(Copy, Clone, Debug)]
pub enum ExplainFormat {
    Human,
    Json,
}

pub struct ExplainOpts {
    pub report: PathBuf,
    pub target: String,
    pub context_lines: usize,
    pub tests: Option<PathBuf>,
    pub coverage: Option<PathBuf>,
    pub llm: bool,
    pub model: Option<String>,
    pub no_cache: bool,
    pub cache_path: Option<PathBuf>,
    pub project_root: PathBuf,
    pub format: ExplainFormat,
}

// ---------------------------------------------------------------------------
// Report shape — stable JSON contract for agent consumers. Add fields with
// care; agents pin on these names.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ExplainReport {
    pub mutant: MutantSummary,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub source_context: SourceContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<EnclosingSummary>,
    pub hint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageSignal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_matches: Option<TestMatchesSignal>,
    pub skeleton: TestSkeleton,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmBlock>,
}

#[derive(Debug, Serialize)]
pub struct MutantSummary {
    pub id: String,
    pub file: String,
    pub line: u32,
    pub operator: String,
    pub original: String,
    pub replacement: String,
}

#[derive(Debug, Serialize)]
pub struct SourceContext {
    pub start_line: u32,
    pub mutant_line: u32,
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EnclosingSummary {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CoverageSignal {
    pub covered: bool,
    pub tests: Vec<String>,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct TestMatchesSignal {
    pub symbol: String,
    pub matches: Vec<TestMatch>,
}

#[derive(Debug, Serialize)]
pub struct TestMatch {
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize)]
pub struct TestSkeleton {
    pub name: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct LlmBlock {
    pub model: String,
    pub cached: bool,
    pub response: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_code: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn explain(opts: ExplainOpts) -> Result<()> {
    let report_obj = build_explain_report(&opts)?;
    match opts.format {
        ExplainFormat::Json => render_json(&report_obj)?,
        ExplainFormat::Human => render_human(&report_obj),
    }
    Ok(())
}

/// Build the structured `ExplainReport` without rendering it. Shared by the
/// `explain` subcommand and the MCP server, so both surfaces compute the
/// same heuristic signal (hint, source context, coverage, skeleton). The
/// optional LLM block is included only when `opts.llm` is set.
pub(crate) fn build_explain_report(opts: &ExplainOpts) -> Result<ExplainReport> {
    let raw = std::fs::read_to_string(&opts.report)
        .with_context(|| format!("reading {}", opts.report.display()))?;
    let report: Report =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", opts.report.display()))?;

    let outcome = locate_outcome(&report, &opts.target)?;
    let m = outcome.mutant();
    let enclosing = enclosing_symbol(m);

    let coverage = match &opts.coverage {
        Some(p) => Some(build_coverage_signal(p, m)?),
        None => None,
    };
    let test_matches = match (&opts.tests, &enclosing) {
        (Some(dir), Some(sym)) => Some(build_test_matches(dir, &sym.name)?),
        _ => None,
    };
    let llm_block = if opts.llm {
        Some(call_llm(
            m,
            opts.context_lines,
            opts.tests.as_deref(),
            enclosing.as_ref(),
            &opts.model,
            opts.no_cache,
            opts.cache_path.as_deref(),
            &opts.project_root,
        )?)
    } else {
        None
    };

    let report_obj = ExplainReport {
        mutant: MutantSummary {
            id: m.id.clone(),
            file: m.file.display().to_string(),
            line: m.line,
            operator: m.operator.name().to_string(),
            original: m.original.clone(),
            replacement: m.replacement.clone(),
        },
        status: outcome.status_label().to_string(),
        filter: match outcome {
            MutantOutcome::Skipped { filter, .. } => Some(filter.clone()),
            _ => None,
        },
        error: match outcome {
            MutantOutcome::Error { message, .. } => Some(message.clone()),
            _ => None,
        },
        source_context: build_source_context(m, opts.context_lines),
        enclosing: enclosing.as_ref().map(|s| EnclosingSummary {
            kind: s.kind.to_string(),
            name: s.name.clone(),
        }),
        hint: operator_hint(m.operator).to_string(),
        coverage,
        test_matches,
        skeleton: build_skeleton(m, enclosing.as_ref()),
        llm: llm_block,
    };

    Ok(report_obj)
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn build_source_context(m: &Mutant, context_lines: usize) -> SourceContext {
    let mut out = SourceContext {
        start_line: m.line,
        mutant_line: m.line,
        lines: Vec::new(),
    };
    let Ok(src) = std::fs::read_to_string(&m.file) else {
        return out;
    };
    let lines: Vec<&str> = src.lines().collect();
    let n = lines.len();
    let line_idx = m.line.saturating_sub(1) as usize;
    if line_idx >= n {
        return out;
    }
    let lo = line_idx.saturating_sub(context_lines);
    let hi = (line_idx + context_lines + 1).min(n);
    out.start_line = (lo as u32) + 1;
    out.lines = lines[lo..hi].iter().map(|s| s.to_string()).collect();
    out
}

fn build_coverage_signal(coverage_path: &Path, m: &Mutant) -> Result<CoverageSignal> {
    let cwd = std::env::current_dir()?;
    let project_root = crate::runner::find_project_root(&cwd).unwrap_or_else(|| cwd.clone());
    let ctx = match CoverageContexts::from_json(coverage_path, &cwd, &project_root) {
        Ok(c) => c,
        Err(e) => {
            return Ok(CoverageSignal {
                covered: false,
                tests: Vec::new(),
                note: format!("could not load coverage: {e}"),
            });
        }
    };
    match ctx.tests_for(&m.file, m.line) {
        Some(tests) if !tests.is_empty() => Ok(CoverageSignal {
            covered: true,
            tests: tests.to_vec(),
            note:
                "tests executed this line but did not distinguish the mutation; strengthen their assertions"
                    .to_string(),
        }),
        _ => Ok(CoverageSignal {
            covered: false,
            tests: Vec::new(),
            note: "no test executes this line; add one that exercises it at all".to_string(),
        }),
    }
}

fn build_test_matches(tests_root: &Path, symbol: &str) -> Result<TestMatchesSignal> {
    let raw = grep_symbol(tests_root, symbol, DEFAULT_GREP_MATCH_LIMIT)?;
    let matches = raw
        .into_iter()
        .map(|(path, line)| TestMatch {
            file: path.display().to_string(),
            line,
        })
        .collect();
    Ok(TestMatchesSignal {
        symbol: symbol.to_string(),
        matches,
    })
}

fn build_skeleton(m: &Mutant, enclosing: Option<&EnclosingSymbol>) -> TestSkeleton {
    let target = enclosing.map(|s| s.name.as_str()).unwrap_or("subject");
    let slug = operator_slug(m.operator);
    let name = format!("test_{target}_{slug}");
    let code = format!(
        "def {name}():\n    # mutation: `{orig}` → `{rep}` at {file}:{line}\n    # Assert a behavior that differs under the mutation above.\n    ...\n",
        orig = m.original,
        rep = m.replacement,
        file = m.file.display(),
        line = m.line,
    );
    TestSkeleton { name, code }
}

#[allow(clippy::too_many_arguments)]
fn call_llm(
    m: &Mutant,
    context_lines: usize,
    tests_dir: Option<&Path>,
    enclosing: Option<&EnclosingSymbol>,
    model_override: &Option<String>,
    no_cache: bool,
    cache_path_override: Option<&Path>,
    project_root: &Path,
) -> Result<LlmBlock> {
    let model = model_override
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let ctx = PromptContext {
        source_snippet: render_source_snippet(m, context_lines),
        enclosing_symbol: enclosing.map(|s| s.name.clone()),
        sample_tests: sample_tests_for_symbol(tests_dir, enclosing.map(|s| s.name.as_str())),
    };
    let req = build_explain_prompt(m, &ctx, &model);

    let cache_path = cache_path_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_cache_path(project_root));
    let mut cache = if no_cache {
        LlmCache::default()
    } else {
        LlmCache::load(&cache_path)
    };
    let sha = file_sha256(&m.file).unwrap_or_default();
    let key = cache_key(&m.id, &sha, &req.user);

    let (response, cached) = match (no_cache, cache.lookup(&key)) {
        (false, Some(hit)) => (hit.to_string(), true),
        _ => {
            let client = client_from_env()?;
            let text = client.complete(&req)?;
            if !no_cache {
                cache.insert(key, text.clone());
                cache.save(&cache_path)?;
            }
            (text, false)
        }
    };

    let extracted_code = extract_first_code_block(&response);
    Ok(LlmBlock {
        model,
        cached,
        response,
        extracted_code,
    })
}

/// Render `±context_lines` around the mutated line, with a `►` marker on
/// the mutation row. Shared with `suggest` so both subcommands present the
/// same view of the source. Empty string on read error or out-of-range
/// `m.line` — callers are expected to tolerate a missing snippet.
pub(crate) fn render_source_snippet(m: &Mutant, context_lines: usize) -> String {
    let Ok(src) = std::fs::read_to_string(&m.file) else {
        return String::new();
    };
    let lines: Vec<&str> = src.lines().collect();
    let n = lines.len();
    let line_idx = m.line.saturating_sub(1) as usize;
    if line_idx >= n {
        return String::new();
    }
    let lo = line_idx.saturating_sub(context_lines);
    let hi = (line_idx + context_lines + 1).min(n);
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(hi).skip(lo) {
        let marker = if i == line_idx { '►' } else { ' ' };
        out.push(marker);
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn sample_tests_for_symbol(tests_dir: Option<&Path>, symbol: Option<&str>) -> Vec<SampleTest> {
    let (Some(dir), Some(sym)) = (tests_dir, symbol) else {
        return Vec::new();
    };
    // Cap small: this caller only needs the first 2 unique-path matches.
    // 8 leaves headroom for hits clustered in the same file.
    let Ok(matches) = grep_symbol(dir, sym, 8) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for (path, _) in matches {
        if !seen.insert(path.clone()) {
            continue;
        }
        if out.len() >= 2 {
            break;
        }
        if let Ok(body) = std::fs::read_to_string(&path) {
            let snippet = if body.len() > 4_000 {
                format!(
                    "{}\n# ... (truncated)",
                    &body[..safe_boundary(&body, 4_000)]
                )
            } else {
                body
            };
            out.push(SampleTest {
                path: path.display().to_string(),
                body: snippet,
            });
        }
    }
    out
}

fn safe_boundary(s: &str, mut end: usize) -> usize {
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_json(report: &ExplainReport) -> Result<()> {
    let s = serde_json::to_string_pretty(report).context("serializing explain report")?;
    println!("{s}");
    Ok(())
}

fn render_human(r: &ExplainReport) {
    println!("{}:{}", r.mutant.file, r.mutant.line);
    println!("operator : {}", r.mutant.operator);
    println!("status   : {}", r.status);
    println!("id       : {}", r.mutant.id);
    println!(
        "mutation : `{}` → `{}`",
        r.mutant.original, r.mutant.replacement
    );
    if let Some(f) = &r.filter {
        println!("filter   : {f}");
    }
    if let Some(e) = &r.error {
        println!("error    : {e}");
    }
    println!();

    if r.source_context.lines.is_empty() {
        println!("(source unreadable: {})", r.mutant.file);
    } else {
        let max_line = r.source_context.start_line as usize + r.source_context.lines.len();
        let width = max_line.to_string().len().max(3);
        println!("source:");
        for (i, line) in r.source_context.lines.iter().enumerate() {
            let absolute = r.source_context.start_line as usize + i;
            let marker = if absolute as u32 == r.source_context.mutant_line {
                "►"
            } else {
                " "
            };
            println!("  {marker} {:>width$}  {}", absolute, line, width = width);
        }
        println!();
    }

    if let Some(s) = &r.enclosing {
        println!("enclosing: {} {}", s.kind, s.name);
    }
    println!("hint     : {}", r.hint);

    if let Some(c) = &r.coverage {
        if c.covered {
            println!("\ncoverage : {} test(s) executed this line:", c.tests.len());
            for t in c.tests.iter().take(10) {
                println!("  - {t}");
            }
            if c.tests.len() > 10 {
                println!("  ... ({} more)", c.tests.len() - 10);
            }
            println!("→ {}", c.note);
        } else {
            println!(
                "\ncoverage : NO test executes {}:{}",
                r.mutant.file, r.mutant.line
            );
            println!("→ {}", c.note);
        }
    }

    if let Some(t) = &r.test_matches {
        if t.matches.is_empty() {
            println!(
                "\ntests   : no test file mentions `{}`. Likely no direct test exists.",
                t.symbol
            );
        } else {
            println!("\ntests   : `{}` referenced in:", t.symbol);
            for m in t.matches.iter().take(8) {
                println!("  - {}:{}", m.file, m.line);
            }
            if t.matches.len() > 8 {
                println!("  ... ({} more)", t.matches.len() - 8);
            }
        }
    }

    println!("\nsuggested test skeleton:");
    println!("```python");
    print!("{}", r.skeleton.code);
    println!("```");

    if let Some(l) = &r.llm {
        let tag = if l.cached { " (cached)" } else { "" };
        println!("\nllm ({}){tag}:\n{}", l.model, l.response);
    }
}

// ---------------------------------------------------------------------------
// Helpers retained from earlier pass (operator hint table, indent scan, grep)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct EnclosingSymbol {
    pub kind: &'static str,
    pub name: String,
}

/// Walk backwards from the mutant line, find the nearest `def`/`class` whose
/// indentation is strictly less than the mutant line's indentation. Good
/// enough for clean Python; deliberately not a real parser.
pub(crate) fn enclosing_symbol(m: &Mutant) -> Option<EnclosingSymbol> {
    let src = std::fs::read_to_string(&m.file).ok()?;
    let lines: Vec<&str> = src.lines().collect();
    let idx = (m.line as usize).checked_sub(1)?;
    if idx >= lines.len() {
        return None;
    }
    let target_indent = indent_of(lines[idx]);
    for back in (0..=idx).rev() {
        let line = lines[back];
        let ind = indent_of(line);
        if back != idx && ind >= target_indent {
            continue;
        }
        let stripped = &line[ind..];
        if let Some(rest) = stripped.strip_prefix("def ") {
            if let Some(name) = extract_ident(rest) {
                return Some(EnclosingSymbol { kind: "def", name });
            }
        } else if let Some(rest) = stripped.strip_prefix("async def ") {
            if let Some(name) = extract_ident(rest) {
                return Some(EnclosingSymbol { kind: "def", name });
            }
        } else if let Some(rest) = stripped.strip_prefix("class ") {
            if let Some(name) = extract_ident(rest) {
                return Some(EnclosingSymbol {
                    kind: "class",
                    name,
                });
            }
        }
    }
    None
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn extract_ident(s: &str) -> Option<String> {
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            end = i + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        None
    } else {
        Some(s[..end].to_string())
    }
}

pub(crate) fn operator_hint(op: Operator) -> &'static str {
    match op {
        Operator::BoundaryShift => {
            "boundary shift (`>=`↔`>`, `<=`↔`<`). Tests likely cover non-equal cases \
             but miss the exact boundary value. Add a test where the input equals the bound."
        }
        Operator::CompareOpSwap => {
            "comparison flipped. Tests likely assert truthiness, not the specific \
             ordering. Add a case where the swapped comparator would yield a different result."
        }
        Operator::ArithOpSwap => {
            "arithmetic op swapped. Tests likely assert a property (positive, non-empty) \
             rather than the exact numeric result. Pin an exact value."
        }
        Operator::BoolOpSwap => {
            "`and`↔`or` swap. Tests likely cover both-true and both-false; add a case \
             where exactly one operand is true."
        }
        Operator::NotInsertion => {
            "extra `not` survived. Tests likely don't check the negative path. Add a \
             test that exercises the false branch."
        }
        Operator::ReturnValueToNone => {
            "return value replaced with `None`. Tests likely call the function for \
             side effects without asserting the return value."
        }
        Operator::AssignValueToNone => {
            "assignment replaced with `None`. Tests likely never read this attribute/\
             local in a way that would distinguish `None` from the real value."
        }
        Operator::ConstantReplace => {
            "constant tweaked. Tests likely don't assert the exact constant; add an \
             equality assertion against a known expected value."
        }
        Operator::NumberShift | Operator::NumberToZero | Operator::NumberToNeg => {
            "numeric value mutated. Tests likely tolerate a range; assert on the exact \
             expected value."
        }
        Operator::StringToEmpty | Operator::StringSentinel => {
            "string replaced. Tests likely check truthiness only; assert the exact string."
        }
        Operator::BytesSentinel => {
            "bytes replaced. Tests likely check length/truthiness; assert the exact bytes."
        }
        Operator::AugAssignSwap => {
            "augmented assignment swapped (`+=`↔`-=`, ...). Tests likely don't observe \
             the accumulator value over multiple iterations."
        }
        Operator::UnaryOpSwap => {
            "unary op flipped (`-`↔`+`, `not`↔identity). Add a test with a non-zero \
             input that distinguishes sign."
        }
        Operator::BreakContinueSwap => {
            "`break`↔`continue` swap. Tests likely don't observe which iteration the \
             loop exited on. Assert on the loop's side effects, not just the final state."
        }
        Operator::RemoveDecorator => {
            "decorator removed (e.g. `@cache`, `@property`). Tests likely don't exercise \
             the decorator's effect; add one that does."
        }
        Operator::DefaultArgToNone => {
            "default arg replaced with `None`. Tests likely always pass the argument \
             explicitly. Add a test that calls without it."
        }
        Operator::LambdaBodyToNone => {
            "lambda body replaced with `None`. Tests likely don't invoke the lambda \
             or don't assert its return value."
        }
        Operator::SliceBoundDrop | Operator::SliceStepMutate => {
            "slice bounds/step mutated. Tests likely don't assert on slice contents at \
             the boundary; add a case sensitive to off-by-one."
        }
        Operator::KeywordArgDrop => {
            "keyword arg dropped. Tests likely pass this kwarg but don't assert on the \
             behavior it controls."
        }
        Operator::DictItemDrop => {
            "dict item dropped. Tests likely don't read this key; assert it is present \
             with the expected value."
        }
        Operator::ArgToNone => {
            "call argument replaced with `None`. Tests likely don't assert on behavior \
             that depends on this argument's value. Add a case where the argument matters."
        }
        Operator::NoneToValue => {
            "`None` replaced with a value. Tests likely never exercise the `is None` / \
             optional-default branch. Add a case that distinguishes `None` from a value."
        }
        Operator::ExprToNone => {
            "expression replaced with `None` (call result / attribute / subscript). \
             Tests likely don't assert on the value this expression produces."
        }
        Operator::PositionalDrop => {
            "positional argument or container element dropped. Tests likely don't \
             observe the missing item; assert on behavior that depends on it."
        }
        Operator::StringCaseSwap => {
            "string literal case-swapped. Tests likely compare case-insensitively or \
             don't assert the exact string; assert the exact value."
        }
        Operator::ExceptionClassSwap | Operator::BareExcept => {
            "exception handling mutated. Tests likely assert on a value but don't assert \
             the exception type. Add a `pytest.raises(SpecificError)` test."
        }
        Operator::ZeroIterationForLoop | Operator::OneIterationForLoop => {
            "loop iteration count mutated. Tests likely don't assert on the number of \
             iterations or their cumulative effect."
        }
    }
}

/// Default upper bound for `grep_symbol`. Picked to comfortably exceed the
/// dedup-by-path requirements of every current caller (sample collection
/// truncates to ≤8 unique files; the test-matches signal renders at most
/// a few dozen) while still capping the walk on a multi-thousand-file
/// repo.
pub(crate) const DEFAULT_GREP_MATCH_LIMIT: usize = 64;

/// Recursive walk of `root`, returning `(path, line)` pairs where `symbol`
/// appears as a whole word inside a `*.py` file. Returns after collecting
/// `limit` matches so a giant test tree can't stall `explain` / `suggest`.
/// Callers that need stricter caps can pass a smaller limit; callers that
/// genuinely want every match should not be using this function.
pub(crate) fn grep_symbol(
    root: &Path,
    symbol: &str,
    limit: usize,
) -> Result<Vec<(PathBuf, usize)>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            if out.len() >= limit {
                break;
            }
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if name.starts_with('.') || name == "__pycache__" {
                        continue;
                    }
                }
                stack.push(path);
            } else if ft.is_file() && path.extension().and_then(|s| s.to_str()) == Some("py") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    if contains_word(line, symbol) {
                        out.push((path.clone(), i + 1));
                        break;
                    }
                }
            }
        }
    }
    Ok(out)
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(bytes[abs - 1]);
        let after = abs + needle_bytes.len();
        let after_ok = after == bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle_bytes.len();
        if start >= bytes.len() {
            break;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub(crate) fn operator_slug(op: Operator) -> &'static str {
    match op {
        Operator::BoundaryShift => "boundary",
        Operator::CompareOpSwap => "compare",
        Operator::ArithOpSwap => "arith",
        Operator::BoolOpSwap => "bool_logic",
        Operator::NotInsertion => "negation",
        Operator::ReturnValueToNone | Operator::AssignValueToNone => "return_value",
        Operator::ConstantReplace
        | Operator::NumberShift
        | Operator::NumberToZero
        | Operator::NumberToNeg => "constant",
        Operator::StringToEmpty | Operator::StringSentinel | Operator::BytesSentinel => "literal",
        Operator::AugAssignSwap => "aug_assign",
        Operator::UnaryOpSwap => "unary",
        Operator::BreakContinueSwap => "loop_exit",
        Operator::RemoveDecorator => "decorator",
        Operator::DefaultArgToNone => "default_arg",
        Operator::LambdaBodyToNone => "lambda_body",
        Operator::SliceBoundDrop | Operator::SliceStepMutate => "slice",
        Operator::KeywordArgDrop => "kwarg",
        Operator::DictItemDrop => "dict_item",
        Operator::ArgToNone => "arg_value",
        Operator::NoneToValue => "none_value",
        Operator::ExceptionClassSwap | Operator::BareExcept => "exception",
        Operator::ZeroIterationForLoop | Operator::OneIterationForLoop => "loop_count",
        Operator::ExprToNone => "return_value",
        Operator::PositionalDrop => "arg_value",
        Operator::StringCaseSwap => "literal",
    }
}

fn locate_outcome<'a>(report: &'a Report, target: &str) -> Result<&'a MutantOutcome> {
    locate_outcome_impl(&report.outcomes, target)
}

/// Resolve a selector to exactly one outcome: a 1-based index, the full
/// `mutant.id`, or any substring of the printed `describe()` row (so the
/// `file:line@offset …` form shown in `run`/`show` works). A partial selector
/// like `file:line@offset` can match several mutants when one offset hosts
/// multiple operators — return an error listing the candidates rather than
/// silently picking the first (which may be a *killed* mutant, poisoning an
/// agent loop). An exact `id` match always wins and is never ambiguous.
pub(crate) fn locate_outcome_impl<'a>(
    outcomes: &'a [MutantOutcome],
    target: &str,
) -> Result<&'a MutantOutcome> {
    if let Ok(idx) = target.parse::<usize>() {
        if idx >= 1 {
            return outcomes
                .get(idx - 1)
                .ok_or_else(|| anyhow::anyhow!("no outcome at index {idx}"));
        }
    }
    // Exact id wins outright — unique by construction, never ambiguous.
    if let Some(o) = outcomes.iter().find(|o| o.mutant().id == target) {
        return Ok(o);
    }
    let matches: Vec<&MutantOutcome> = outcomes
        .iter()
        .filter(|o| o.mutant().id.contains(target) || o.mutant().describe().contains(target))
        .collect();
    match matches.as_slice() {
        [] => Err(anyhow::anyhow!("no mutant matches `{target}`")),
        [one] => Ok(one),
        many => {
            let list = many
                .iter()
                .map(|o| format!("  {}  [{}]", o.mutant().id, o.status_label()))
                .collect::<Vec<_>>()
                .join("\n");
            Err(anyhow::anyhow!(
                "selector `{target}` is ambiguous — {} mutants match (e.g. multiple operators \
                 at one offset):\n{list}\nPass the full mutant id or the outcome index.",
                many.len()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fermut-explain-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn make_mutant(file: PathBuf, line: u32) -> Mutant {
        Mutant {
            id: format!("{}:{}:boundary:0", file.display(), line),
            file,
            operator: Operator::BoundaryShift,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: ">=".into(),
            replacement: ">".into(),
            line,
        }
    }

    #[test]
    fn locate_accepts_printed_offset_form() {
        // The `file:line@offset` form printed by `run`/`show` (the `@offset`
        // is not in this mutant's id) must select via the describe() fallback.
        let m = make_mutant(PathBuf::from("calc.py"), 14);
        let report = Report::new(vec![MutantOutcome::survived(m)]);
        assert!(
            locate_outcome(&report, "@0").is_ok(),
            "bare @offset selector should match"
        );
        assert!(
            locate_outcome(&report, "calc.py:14@0").is_ok(),
            "printed `file:line@offset` row should be a valid selector"
        );
        assert!(
            locate_outcome(&report, "nope:9@99").is_err(),
            "non-matching selector must miss"
        );
    }

    #[test]
    fn ambiguous_selector_errors_instead_of_picking_first() {
        // Two operators at the same offset → the printed `file:line@offset`
        // prefix matches both. Must error (listing candidates), not silently
        // return the first (which could be a killed mutant).
        let f = PathBuf::from("calc.py");
        let mut killed = make_mutant(f.clone(), 14);
        killed.id = "calc.py@216:compare-op-swap:<=->>=".into();
        killed.range = TextRange::new(216u32.into(), 218u32.into());
        let mut survived = make_mutant(f, 14);
        survived.id = "calc.py@216:boundary-shift:<=-><".into();
        survived.range = TextRange::new(216u32.into(), 218u32.into());

        let report = Report::new(vec![
            MutantOutcome::killed(killed),
            MutantOutcome::survived(survived.clone()),
        ]);

        let err = locate_outcome(&report, "calc.py:14@216").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ambiguous"), "got: {msg}");
        assert!(
            msg.contains("compare-op-swap"),
            "should list candidates: {msg}"
        );

        // The full id still resolves to exactly the survivor.
        let hit = locate_outcome(&report, &survived.id).unwrap();
        assert_eq!(hit.mutant().id, survived.id);
    }

    #[test]
    fn enclosing_symbol_finds_def() {
        let src = "def outer():\n    pass\n\ndef target(x):\n    if x >= 0:\n        return x\n";
        let path = write_tmp("encl_def.py", src);
        let m = make_mutant(path, 5);
        let sym = enclosing_symbol(&m).expect("found");
        assert_eq!(sym.kind, "def");
        assert_eq!(sym.name, "target");
    }

    #[test]
    fn enclosing_symbol_finds_class_when_no_def() {
        let src = "class C:\n    x = 1\n    y = 2\n";
        let path = write_tmp("encl_class.py", src);
        let m = make_mutant(path, 3);
        let sym = enclosing_symbol(&m).expect("found");
        assert_eq!(sym.kind, "class");
        assert_eq!(sym.name, "C");
    }

    #[test]
    fn enclosing_symbol_handles_async_def() {
        let src = "async def fetch(u):\n    return await go(u)\n";
        let path = write_tmp("encl_async.py", src);
        let m = make_mutant(path, 2);
        let sym = enclosing_symbol(&m).expect("found");
        assert_eq!(sym.kind, "def");
        assert_eq!(sym.name, "fetch");
    }

    #[test]
    fn enclosing_symbol_none_at_module_scope() {
        let src = "x = 1\ny = 2\n";
        let path = write_tmp("encl_module.py", src);
        let m = make_mutant(path, 1);
        assert!(enclosing_symbol(&m).is_none());
    }

    #[test]
    fn contains_word_matches_whole_word_only() {
        assert!(contains_word("foo bar baz", "bar"));
        assert!(contains_word("(bar)", "bar"));
        assert!(!contains_word("foobar", "bar"));
        assert!(!contains_word("barbecue", "bar"));
        assert!(contains_word("bar", "bar"));
    }

    #[test]
    fn grep_symbol_finds_match() {
        let dir = std::env::temp_dir().join(format!("fermut-grep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test_a.py"),
            "def test_target_thing():\n    assert target(1)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("test_b.py"),
            "def test_unrelated():\n    assert other()\n",
        )
        .unwrap();
        let hits = grep_symbol(&dir, "target", DEFAULT_GREP_MATCH_LIMIT).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.ends_with("test_a.py"));
    }

    /// Regression: large test trees used to scan every file. With the
    /// limit honored, the walk stops once `limit` distinct matches have
    /// been collected.
    #[test]
    fn grep_symbol_honors_limit() {
        let dir = std::env::temp_dir().join(format!("fermut-grep-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..20 {
            std::fs::write(
                dir.join(format!("test_{i}.py")),
                "def test_x():\n    assert target()\n",
            )
            .unwrap();
        }
        let hits = grep_symbol(&dir, "target", 5).unwrap();
        assert_eq!(hits.len(), 5, "limit was not respected: {hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_symbol_limit_zero_returns_empty_without_walking() {
        let dir = std::env::temp_dir().join(format!("fermut-grep-zero-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.py"), "target\n").unwrap();
        let hits = grep_symbol(&dir, "target", 0).unwrap();
        assert!(hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn operator_hint_covers_every_variant() {
        for op in [
            Operator::ArithOpSwap,
            Operator::CompareOpSwap,
            Operator::BoolOpSwap,
            Operator::ConstantReplace,
            Operator::BoundaryShift,
            Operator::AugAssignSwap,
            Operator::UnaryOpSwap,
            Operator::NumberShift,
            Operator::ReturnValueToNone,
            Operator::BreakContinueSwap,
            Operator::NotInsertion,
            Operator::RemoveDecorator,
            Operator::DefaultArgToNone,
            Operator::LambdaBodyToNone,
            Operator::SliceBoundDrop,
            Operator::SliceStepMutate,
            Operator::AssignValueToNone,
            Operator::NumberToZero,
            Operator::NumberToNeg,
            Operator::StringToEmpty,
            Operator::StringSentinel,
            Operator::BytesSentinel,
            Operator::KeywordArgDrop,
            Operator::DictItemDrop,
            Operator::ExceptionClassSwap,
            Operator::BareExcept,
            Operator::ZeroIterationForLoop,
            Operator::OneIterationForLoop,
        ] {
            assert!(!operator_hint(op).is_empty(), "missing hint for {op:?}");
            assert!(!operator_slug(op).is_empty(), "missing slug for {op:?}");
        }
    }

    #[test]
    fn build_skeleton_uses_enclosing_and_operator_slug() {
        let m = make_mutant(PathBuf::from("x.py"), 1);
        let enc = EnclosingSymbol {
            kind: "def",
            name: "in_range".into(),
        };
        let skel = build_skeleton(&m, Some(&enc));
        assert_eq!(skel.name, "test_in_range_boundary");
        assert!(skel.code.contains("def test_in_range_boundary()"));
    }

    #[test]
    fn build_source_context_marks_mutant_line() {
        let src = "a\nb\nc\nd\ne\n";
        let path = write_tmp("src_ctx.py", src);
        let m = make_mutant(path, 3);
        let ctx = build_source_context(&m, 1);
        // Start at line 2, three lines b/c/d.
        assert_eq!(ctx.start_line, 2);
        assert_eq!(ctx.mutant_line, 3);
        assert_eq!(ctx.lines, vec!["b".to_string(), "c".into(), "d".into()]);
    }
}
