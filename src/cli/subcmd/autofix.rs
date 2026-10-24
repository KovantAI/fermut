//! `fermut autofix` — generate a killing test for each survivor, then
//! *verify* it before keeping it.
//!
//! `fermut suggest` writes a test but never proves it works. autofix closes
//! the loop: for each surviving mutant it generates a test, appends it to
//! the discovered test file, then checks two things against the real runner:
//!
//! 1. **Suite stays green** — the unmutated suite (including the new test)
//!    still passes. A generated test that fails on correct code is wrong.
//! 2. **The mutant now dies** — re-applying the mutation makes the suite
//!    fail. That's the proof the test actually catches the bug.
//!
//! Only when both hold is the test kept. Otherwise it's reverted, so a bad
//! suggestion never lands in the tree. The result is a ready-to-commit test
//! per fixed survivor.
//!
//! **Mirror freshness.** The per-worker test mirror is thread-local and only
//! rebuilds when the tests path changes (see `runner::with_worker_mirror`).
//! Since autofix mutates the tests dir between verifications, each
//! mutant-kill check runs on a fresh thread so its mirror snapshots the
//! just-appended test. `baseline()` builds its own one-shot mirror, so it's
//! always current.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::explain::MutantSummary;
use super::suggest::{
    append_to_test_file, build_context, generate_test_code, infer_apply_target, select_targets,
};
use crate::cli::Format;
use crate::equiv::llm::{before_generation, ignore_marker, TriageSummary};
use crate::llm::cache::default_cache_path;
use crate::llm::{LlmCallOpts, LlmSession, TriageMode, DEFAULT_MODEL};
use crate::mutator::Mutant;
use crate::report::MutantOutcome;
use crate::runner::{self, BaselineStatus, Runner};

pub struct AutofixOpts {
    pub report: PathBuf,
    pub target: Option<String>,
    pub all_survivors: bool,
    pub path: PathBuf,
    pub tests: Option<PathBuf>,
    pub python: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub model: Option<String>,
    pub context_lines: usize,
    pub sample_count: usize,
    pub no_cache: bool,
    pub cache_path: Option<PathBuf>,
    pub timeout: Option<u64>,
    /// Keep a generated test even when verification fails. Off by default —
    /// the whole point is to land only proven tests.
    pub keep_failed: bool,
    pub format: Format,
    pub triage: TriageMode,
}

impl From<AutofixArgs> for AutofixOpts {
    fn from(a: AutofixArgs) -> Self {
        let AutofixArgs {
            report,
            target,
            all_survivors,
            path,
            tests,
            python,
            out,
            model,
            context,
            sample_count,
            timeout,
            no_cache,
            cache_path,
            keep_failed,
            format,
            triage,
        } = a;
        AutofixOpts {
            report,
            target,
            all_survivors,
            path,
            tests,
            python,
            out,
            model,
            context_lines: context,
            sample_count,
            timeout,
            no_cache,
            cache_path,
            keep_failed,
            format,
            triage,
        }
    }
}

// ---------------------------------------------------------------------------
// Report shape — stable JSON contract for agent consumers.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AutofixReport {
    pub model: String,
    pub fixed: usize,
    pub failed: usize,
    /// Survivors the judge called equivalent under `--triage skip` (no test
    /// generated; neither fixed nor failed).
    #[serde(skip_serializing_if = "is_zero")]
    pub likely_equivalent: usize,
    /// `--triage audit`: how often a survivor judged equivalent was killed
    /// anyway, i.e. a proven false positive of the judge. Precision is at
    /// most `(judged_equivalent - killed_anyway) / judged_equivalent`: autofix
    /// failing to kill one doesn't prove it equivalent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage_audit: Option<TriageAudit>,
    pub entries: Vec<AutofixEntry>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct TriageAudit {
    pub judged_equivalent: usize,
    pub killed_anyway: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct AutofixEntry {
    pub mutant: MutantSummary,
    /// What happened, one of: `fixed`, `suite-red`, `still-survives`,
    /// `generation-failed`, `error`, or `likely-equivalent` (judged
    /// equivalent under `--triage skip`; nothing generated).
    pub outcome: &'static str,
    /// Whether the generated test was left in the tree (true) or reverted.
    pub kept: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The equivalence judge's verdict (`--triage`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage: Option<TriageSummary>,
}

const OUTCOME_LIKELY_EQUIVALENT: &str = "likely-equivalent";
const OUTCOME_FIXED: &str = "fixed";
const OUTCOME_SUITE_RED: &str = "suite-red";
const OUTCOME_STILL_SURVIVES: &str = "still-survives";
const OUTCOME_GENERATION_FAILED: &str = "generation-failed";
const OUTCOME_ERROR: &str = "error";

pub fn autofix(opts: AutofixOpts) -> Result<()> {
    let report = crate::report::load(&opts.report)?;
    let targets = select_targets(&report, opts.target.as_deref(), opts.all_survivors)?;
    let mutants: Vec<Mutant> = targets.iter().map(|o| o.mutant().clone()).collect();

    // Build the runtime config the same way `fermut run` does so the runner
    // mirrors the right tests dir and honors the project's fermut.toml.
    let cfg = build_run_config(&opts)?;
    let runner = runner::build(&cfg);

    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let cache_path = opts
        .cache_path
        .clone()
        .unwrap_or_else(|| default_cache_path(&crate::history::resolve_root(&cfg.source_root)));
    let tests_dir = opts.tests.clone().or_else(|| cfg.tests.clone());

    let core = Core {
        runner: runner.as_ref(),
        tests_dir: tests_dir.as_deref(),
        out: opts.out.as_deref(),
        llm: LlmSession::new(LlmCallOpts {
            model,
            no_cache: opts.no_cache,
            cache_path,
        }),
        triage: opts.triage,
        context_lines: opts.context_lines,
        sample_count: opts.sample_count,
        keep_failed: opts.keep_failed,
        log: matches!(opts.format, Format::Human),
    };

    let report = run_autofix(&core, &mutants);

    match opts.format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        Format::Human => render_human(&report),
    }
    Ok(())
}

/// Build a `Config` for the runner from autofix's run-shape args. Mirrors
/// `fermut run`'s defaults; only `path`, `tests`, and `timeout` are wired
/// through — autofix doesn't mutate, it just needs the runner + baseline.
fn build_run_config(opts: &AutofixOpts) -> Result<crate::config::Config> {
    crate::cli::build_config::build_config(
        opts.path.clone(),
        crate::cli::RunConfigArgs {
            tests: opts.tests.clone(),
            timeout: opts.timeout,
            no_cache: true,   // autofix doesn't read the mutation cache
            no_history: true, // a verify run isn't a real scored run
            python: opts.python.clone(),
            ..Default::default()
        },
    )
}

/// Borrowed parameters for the verification loop. Split from `AutofixOpts`
/// so tests can drive it with a `MockRunner` and without a real `Config`.
struct Core<'a> {
    runner: &'a dyn Runner,
    tests_dir: Option<&'a Path>,
    out: Option<&'a Path>,
    llm: LlmSession,
    triage: TriageMode,
    context_lines: usize,
    sample_count: usize,
    keep_failed: bool,
    log: bool,
}

fn run_autofix(core: &Core, mutants: &[Mutant]) -> AutofixReport {
    let mut entries = Vec::with_capacity(mutants.len());
    for m in mutants {
        entries.push(fix_one(core, m));
    }
    let count = |outcome: &str| entries.iter().filter(|e| e.outcome == outcome).count();
    let fixed = count(OUTCOME_FIXED);
    let likely_equivalent = count(OUTCOME_LIKELY_EQUIVALENT);
    let triage_audit = (core.triage == TriageMode::Audit).then(|| {
        let judged: Vec<&AutofixEntry> = entries
            .iter()
            .filter(|e| e.triage.as_ref().is_some_and(|t| t.verdict == "equivalent"))
            .collect();
        TriageAudit {
            judged_equivalent: judged.len(),
            killed_anyway: judged.iter().filter(|e| e.outcome == OUTCOME_FIXED).count(),
        }
    });
    AutofixReport {
        model: core.llm.opts.model.clone(),
        fixed,
        failed: entries.len() - fixed - likely_equivalent,
        likely_equivalent,
        triage_audit,
        entries,
    }
}

fn fix_one(core: &Core, m: &Mutant) -> AutofixEntry {
    let summary = MutantSummary {
        id: m.id.clone(),
        file: m.file.display().to_string(),
        line: m.line,
        operator: m.operator.name().to_string(),
        original: m.original.clone(),
        replacement: m.replacement.clone(),
    };
    if core.log {
        eprintln!(
            "→ autofix {}:{} [{}]",
            m.file.display(),
            m.line,
            m.operator.name()
        );
    }

    // 0. triage (`--triage`): skip a survivor judged equivalent, or carry the
    //    judge's explanation of the difference into generation.
    let mut ctx = build_context(m, core.context_lines, core.tests_dir, core.sample_count);
    let llm = &core.llm;
    let (triage, skip) = before_generation(
        core.triage,
        m,
        &mut ctx,
        &llm.opts,
        &llm.client_cell,
        &llm.cache,
    );
    let mut e = if skip {
        let reason = triage.as_ref().map_or("", |t| t.reason.as_str());
        if core.log {
            eprintln!("  ~ judged equivalent; no test generated");
        }
        entry(
            summary,
            OUTCOME_LIKELY_EQUIVALENT,
            false,
            None,
            Some(format!(
                "{reason} If you agree, mark it: {}",
                ignore_marker(m.file.display(), m.line, m.operator.name())
            )),
        )
    } else {
        generate_and_verify(core, m, summary, &ctx)
    };
    e.triage = triage;
    e
}

/// Generate a test from `ctx`, append it, and keep it only if it verifies.
fn generate_and_verify(
    core: &Core,
    m: &Mutant,
    summary: MutantSummary,
    ctx: &crate::llm::prompt::PromptContext,
) -> AutofixEntry {
    // 1. generate
    let llm = &core.llm;
    let code = match generate_test_code(m, ctx, &llm.opts, &llm.client_cell, &llm.cache) {
        Ok(v) => v,
        Err(e) => {
            return entry(
                summary,
                OUTCOME_GENERATION_FAILED,
                false,
                None,
                Some(e.to_string()),
            )
        }
    };

    // 2. pick a target file: --out when given, else inferred from the
    //    enclosing symbol
    let target_path = match core
        .out
        .map(|p| Ok(p.to_path_buf()))
        .unwrap_or_else(|| infer_apply_target(m, ctx))
    {
        Ok(p) => p,
        Err(e) => return entry(summary, OUTCOME_ERROR, false, None, Some(e.to_string())),
    };

    let payload = format!(
        "\n# fermut autofix: kills mutation `{}` → `{}` at {}:{}\n{}\n",
        m.original,
        m.replacement,
        m.file.display(),
        m.line,
        code,
    );

    // 3. snapshot (so we can revert) and append.
    let snapshot = Snapshot::take(&target_path);
    if let Err(e) = append_to_test_file(&target_path, &payload) {
        return entry(summary, OUTCOME_ERROR, false, None, Some(e.to_string()));
    }

    // 4. verify, reverting unless verification passed (or --keep-failed).
    let applied = Some(target_path.display().to_string());
    match verify(core.runner, m) {
        Ok(Verdict::Fixed) => {
            if core.log {
                eprintln!(
                    "  ✓ kept — mutant now killed, suite green ({})",
                    target_path.display()
                );
            }
            entry(summary, OUTCOME_FIXED, true, applied, None)
        }
        Ok(Verdict::SuiteRed(out)) => revert_or_keep(
            core,
            summary,
            snapshot,
            &target_path,
            applied,
            Rejection {
                outcome: OUTCOME_SUITE_RED,
                reason: "generated test fails on unmutated code",
                extra: Some(out),
            },
        ),
        Ok(Verdict::StillSurvives) => revert_or_keep(
            core,
            summary,
            snapshot,
            &target_path,
            applied,
            Rejection {
                outcome: OUTCOME_STILL_SURVIVES,
                reason: "generated test does not catch the mutation",
                extra: None,
            },
        ),
        Err(e) => revert_or_keep(
            core,
            summary,
            snapshot,
            &target_path,
            applied,
            Rejection {
                outcome: OUTCOME_ERROR,
                reason: "verification error",
                extra: Some(e.to_string()),
            },
        ),
    }
}

/// Why a generated test was rejected at verification, and how to label the
/// resulting entry. Bundles the three fields that vary per rejection branch so
/// `revert_or_keep` takes the run environment plus one decision value.
struct Rejection {
    /// Stable `OUTCOME_*` label recorded on the entry.
    outcome: &'static str,
    /// Human reason, prefixed onto the detail line and the log message.
    reason: &'static str,
    /// Extra context (suite tail / error message) appended to `reason`.
    extra: Option<String>,
}

fn revert_or_keep(
    core: &Core,
    summary: MutantSummary,
    snapshot: Snapshot,
    path: &Path,
    applied: Option<String>,
    rej: Rejection,
) -> AutofixEntry {
    let Rejection {
        outcome,
        reason,
        extra,
    } = rej;
    let detail = match extra {
        Some(x) => Some(format!("{reason}: {x}")),
        None => Some(reason.to_string()),
    };
    if core.keep_failed {
        if core.log {
            eprintln!(
                "  ! {reason} — kept anyway (--keep-failed) at {}",
                path.display()
            );
        }
        return entry(summary, outcome, true, applied, detail);
    }
    if let Err(e) = snapshot.restore(path) {
        if core.log {
            eprintln!("  ! revert of {} failed: {e}", path.display());
        }
        return entry(
            summary,
            outcome,
            true, // could not revert — the test is still on disk
            applied,
            Some(format!("{reason}; ALSO failed to revert: {e}")),
        );
    }
    if core.log {
        eprintln!("  ✗ {reason} — reverted {}", path.display());
    }
    entry(summary, outcome, false, applied, detail)
}

fn entry(
    mutant: MutantSummary,
    outcome: &'static str,
    kept: bool,
    applied_to: Option<String>,
    detail: Option<String>,
) -> AutofixEntry {
    AutofixEntry {
        mutant,
        outcome,
        kept,
        applied_to,
        detail,
        triage: None,
    }
}

enum Verdict {
    /// Suite green and the mutant is now killed — keep the test.
    Fixed,
    /// The new test made the unmutated suite fail. `String` is the tail.
    SuiteRed(String),
    /// Suite green but the mutant still survives — the test doesn't catch it.
    StillSurvives,
}

/// Run the two verification steps against the real runner.
fn verify(runner: &dyn Runner, m: &Mutant) -> Result<Verdict> {
    // Suite-green check first: `baseline()` builds a one-shot mirror, so it
    // reflects the just-appended test on whatever thread we call it.
    match runner.baseline()? {
        BaselineStatus::Failed { output } => return Ok(Verdict::SuiteRed(output)),
        BaselineStatus::Passed { .. } => {}
    }
    // Mutant-kill check on a fresh thread → fresh thread-local mirror that
    // snapshots the updated tests dir.
    let outcome = run_on_fresh_thread(runner, m)?;
    Ok(match outcome {
        MutantOutcome::Killed { .. } | MutantOutcome::TimedOut { .. } => Verdict::Fixed,
        _ => Verdict::StillSurvives,
    })
}

/// Evaluate one mutant on a dedicated thread so `with_worker_mirror`'s
/// thread-local cache starts empty and rebuilds against the current tests.
fn run_on_fresh_thread(runner: &dyn Runner, m: &Mutant) -> Result<MutantOutcome> {
    // Not the hot loop — one survivor verified at a time, each running the full
    // suite — so wrapping in a fresh `Arc` here (one clone) is negligible and
    // keeps `verify`'s public `&Mutant` signature.
    let m = std::sync::Arc::new(m.clone());
    std::thread::scope(|s| {
        s.spawn(|| runner.run(&m))
            .join()
            .map_err(|_| anyhow::anyhow!("verification thread panicked"))?
    })
}

/// The prior contents of a test file, captured before an append so a failed
/// verification can be rolled back. `Absent` means the file didn't exist —
/// reverting deletes it.
enum Snapshot {
    Absent,
    Existing(Vec<u8>),
}

impl Snapshot {
    fn take(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Snapshot::Existing(bytes),
            Err(_) => Snapshot::Absent,
        }
    }

    fn restore(&self, path: &Path) -> Result<()> {
        match self {
            Snapshot::Existing(bytes) => {
                std::fs::write(path, bytes).with_context(|| format!("restoring {}", path.display()))
            }
            Snapshot::Absent => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
            },
        }
    }
}

fn render_human(report: &AutofixReport) {
    for e in &report.entries {
        let m = &e.mutant;
        let mark = if e.kept { "kept" } else { "reverted" };
        println!(
            "[{}] {}:{} [{}] — {} ({mark})",
            e.outcome,
            m.file,
            m.line,
            m.operator,
            e.detail.as_deref().unwrap_or("ok")
        );
    }
    println!(
        "\nautofix: {} fixed, {} failed, {} likely equivalent ({} total)",
        report.fixed,
        report.failed,
        report.likely_equivalent,
        report.entries.len()
    );
    if let Some(a) = &report.triage_audit {
        println!(
            "triage: {} judged equivalent, {} of them killed anyway{}",
            a.judged_equivalent,
            a.killed_anyway,
            if a.judged_equivalent > 0 {
                format!(
                    " (judge precision at most {:.0}%: a survivor autofix couldn't kill may still not be equivalent)",
                    100.0 * (a.judged_equivalent - a.killed_anyway) as f64
                        / a.judged_equivalent as f64
                )
            } else {
                String::new()
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use crate::runner::BaselineStatus;
    use ruff_text_size::TextRange;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// A runner whose verdicts are scripted, so we can exercise the
    /// keep/revert logic without pytest or a real mirror.
    struct MockRunner {
        baseline: Mutex<Vec<BaselineStatus>>,
        run_kills: bool,
    }

    impl Runner for MockRunner {
        fn run(&self, m: &std::sync::Arc<Mutant>) -> Result<MutantOutcome> {
            Ok(if self.run_kills {
                MutantOutcome::killed(m.clone())
            } else {
                MutantOutcome::survived(m.clone())
            })
        }
        fn baseline(&self) -> Result<BaselineStatus> {
            let mut q = self.baseline.lock().unwrap();
            Ok(if q.is_empty() {
                BaselineStatus::Passed {
                    wall: std::time::Duration::ZERO,
                }
            } else {
                q.remove(0)
            })
        }
    }

    fn mock(baseline_passed: bool, run_kills: bool) -> MockRunner {
        let baseline = if baseline_passed {
            vec![BaselineStatus::Passed {
                wall: std::time::Duration::ZERO,
            }]
        } else {
            vec![BaselineStatus::Failed {
                output: "1 failed".into(),
            }]
        };
        MockRunner {
            baseline: Mutex::new(baseline),
            run_kills,
        }
    }

    fn mutant(file: PathBuf) -> Mutant {
        Mutant {
            id: "calc:2:boundary:0".into(),
            file,
            operator: Operator::BoundaryShift,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "<=".into(),
            replacement: "<".into(),
            line: 2,
            stmt_line: 2,
            site: None,
        }
    }

    /// A fixture project: a source file, a tests/ dir with one test file
    /// that references the enclosing symbol (so apply-target inference finds
    /// it), and FERMUT_LLM_MOCK so generation returns a canned test.
    fn fixture() -> (TempDir, Mutant, PathBuf) {
        std::env::set_var("FERMUT_LLM_MOCK", "1");
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("calc.py");
        std::fs::write(
            &src,
            "def in_range(x, lo, hi):\n    return lo <= x and x <= hi\n",
        )
        .unwrap();
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        let test_file = tests.join("test_calc.py");
        std::fs::write(
            &test_file,
            "def test_in_range():\n    assert in_range(1, 0, 2)\n",
        )
        .unwrap();
        (dir, mutant(src), test_file)
    }

    fn core<'a>(runner: &'a dyn Runner, tests: &'a Path, keep_failed: bool) -> Core<'a> {
        Core {
            runner,
            tests_dir: Some(tests),
            out: None,
            llm: LlmSession::new(LlmCallOpts {
                model: DEFAULT_MODEL.into(),
                no_cache: true,
                cache_path: PathBuf::from("/nonexistent/llm-cache.json"),
            }),
            triage: TriageMode::Off,
            context_lines: 3,
            sample_count: 1,
            keep_failed,
            log: false,
        }
    }

    /// Answers the equivalence judge with `judge` and test generation with a
    /// passing test; records every request so tests can count and inspect them.
    struct RoutedClient {
        judge: String,
        requests: Mutex<Vec<crate::llm::client::LlmRequest>>,
    }

    impl crate::llm::LlmClient for RoutedClient {
        fn complete(&self, req: &crate::llm::client::LlmRequest) -> Result<String> {
            self.requests.lock().unwrap().push(req.clone());
            Ok(if req.system.contains("BEHAVIORALLY EQUIVALENT") {
                self.judge.clone()
            } else {
                "```python\ndef test_generated():\n    assert True\n```".into()
            })
        }
    }

    fn triaging<'a>(
        runner: &'a dyn Runner,
        tests: &'a Path,
        mode: TriageMode,
        judge: &str,
    ) -> (Core<'a>, std::sync::Arc<RoutedClient>) {
        let client = std::sync::Arc::new(RoutedClient {
            judge: judge.into(),
            requests: Mutex::new(Vec::new()),
        });
        let mut c = core(runner, tests, false);
        c.triage = mode;
        let _ = c.llm.client_cell.set(Ok(
            client.clone() as std::sync::Arc<dyn crate::llm::LlmClient>
        ));
        (c, client)
    }

    const JUDGED_EQUIVALENT: &str = r#"{"equivalent": true, "confidence": 0.97, "reason": "both forms agree on every input.", "counterexample": null}"#;
    const JUDGED_DIFFERENT: &str = r#"{"equivalent": false, "confidence": 0.9, "reason": "differs when x equals lo", "counterexample": "in_range(0, 0, 2)"}"#;

    #[test]
    fn triage_skip_reports_likely_equivalent_without_generating() {
        let (dir, m, test_file) = fixture();
        let before = std::fs::read_to_string(&test_file).unwrap();
        let runner = mock(true, true);
        let tests_dir = dir.path().join("tests");
        let (c, client) = triaging(&runner, &tests_dir, TriageMode::Skip, JUDGED_EQUIVALENT);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        let e = &report.entries[0];
        assert_eq!(e.outcome, OUTCOME_LIKELY_EQUIVALENT);
        assert_eq!(e.triage.as_ref().unwrap().verdict, "equivalent");
        assert!(e
            .detail
            .as_ref()
            .unwrap()
            .contains("# fermut: ignore[boundary-shift]"));
        assert_eq!(
            (report.fixed, report.failed, report.likely_equivalent),
            (0, 0, 1)
        );
        assert_eq!(
            client.requests.lock().unwrap().len(),
            1,
            "judge only, no generation"
        );
        assert_eq!(std::fs::read_to_string(&test_file).unwrap(), before);
        assert!(report.triage_audit.is_none());
    }

    #[test]
    fn triage_audit_still_fixes_and_counts_a_judge_false_positive() {
        let (dir, m, _test_file) = fixture();
        let runner = mock(true, true); // the generated test kills the mutant
        let tests_dir = dir.path().join("tests");
        let (c, client) = triaging(&runner, &tests_dir, TriageMode::Audit, JUDGED_EQUIVALENT);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.entries[0].outcome, OUTCOME_FIXED);
        assert_eq!(
            report.triage_audit,
            Some(TriageAudit {
                judged_equivalent: 1,
                killed_anyway: 1
            })
        );
        assert_eq!(
            client.requests.lock().unwrap().len(),
            2,
            "judge, then generation"
        );
    }

    #[test]
    fn triage_different_steers_generation_with_the_judges_explanation() {
        let (dir, m, _test_file) = fixture();
        let runner = mock(true, true);
        let tests_dir = dir.path().join("tests");
        let (c, client) = triaging(&runner, &tests_dir, TriageMode::Skip, JUDGED_DIFFERENT);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.entries[0].outcome, OUTCOME_FIXED);
        assert_eq!(
            report.entries[0].triage.as_ref().unwrap().verdict,
            "different"
        );
        let requests = client.requests.lock().unwrap();
        let generation = &requests[1].user;
        assert!(
            generation.contains("differs when x equals lo"),
            "{generation}"
        );
        assert!(generation.contains("in_range(0, 0, 2)"), "{generation}");
    }

    #[test]
    fn fixed_when_suite_green_and_mutant_dies_keeps_test() {
        let (dir, m, test_file) = fixture();
        let before = std::fs::read_to_string(&test_file).unwrap();
        let runner = mock(true, true);
        let tests_dir = dir.path().join("tests");
        let c = core(&runner, &tests_dir, false);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.fixed, 1);
        assert_eq!(report.entries[0].outcome, OUTCOME_FIXED);
        assert!(report.entries[0].kept);
        // The generated test was appended and left in place.
        let after = std::fs::read_to_string(&test_file).unwrap();
        assert!(after.len() > before.len());
        assert!(after.contains("fermut autofix"));
    }

    #[test]
    fn still_survives_reverts_the_test() {
        let (dir, m, test_file) = fixture();
        let before = std::fs::read_to_string(&test_file).unwrap();
        let runner = mock(true, false); // suite green, but mutant survives
        let tests_dir = dir.path().join("tests");
        let c = core(&runner, &tests_dir, false);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.fixed, 0);
        assert_eq!(report.failed, 1);
        assert_eq!(report.entries[0].outcome, OUTCOME_STILL_SURVIVES);
        assert!(!report.entries[0].kept);
        // File restored byte-for-byte.
        let after = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn suite_red_reverts_the_test() {
        let (dir, m, test_file) = fixture();
        let before = std::fs::read_to_string(&test_file).unwrap();
        let runner = mock(false, true); // appended test breaks the suite
        let tests_dir = dir.path().join("tests");
        let c = core(&runner, &tests_dir, false);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.entries[0].outcome, OUTCOME_SUITE_RED);
        assert!(!report.entries[0].kept);
        assert_eq!(std::fs::read_to_string(&test_file).unwrap(), before);
    }

    #[test]
    fn keep_failed_leaves_a_failing_suggestion_in_place() {
        let (dir, m, test_file) = fixture();
        let before = std::fs::read_to_string(&test_file).unwrap();
        let runner = mock(true, false);
        let tests_dir = dir.path().join("tests");
        let c = core(&runner, &tests_dir, true); // keep_failed
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.entries[0].outcome, OUTCOME_STILL_SURVIVES);
        assert!(report.entries[0].kept);
        assert!(std::fs::read_to_string(&test_file).unwrap().len() > before.len());
    }

    #[test]
    fn out_overrides_the_inferred_target() {
        let (dir, m, test_file) = fixture();
        let before = std::fs::read_to_string(&test_file).unwrap();
        let runner = mock(true, true);
        let tests_dir = dir.path().join("tests");
        let forced = tests_dir.join("test_forced.py");
        let mut c = core(&runner, &tests_dir, false);
        c.out = Some(&forced);
        let report = run_autofix(&c, std::slice::from_ref(&m));
        assert_eq!(report.entries[0].outcome, OUTCOME_FIXED);
        // Written to --out, not the inferred test_calc.py.
        assert!(std::fs::read_to_string(&forced)
            .unwrap()
            .contains("fermut autofix"));
        assert_eq!(std::fs::read_to_string(&test_file).unwrap(), before);
    }

    #[test]
    fn snapshot_absent_revert_deletes_created_file() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("new_test.py");
        let snap = Snapshot::take(&p);
        std::fs::write(&p, "created\n").unwrap();
        snap.restore(&p).unwrap();
        assert!(
            !p.exists(),
            "revert must delete a file that didn't exist before"
        );
    }

    #[test]
    fn snapshot_existing_revert_restores_bytes() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("t.py");
        std::fs::write(&p, "original\n").unwrap();
        let snap = Snapshot::take(&p);
        std::fs::write(&p, "original\nappended\n").unwrap();
        snap.restore(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "original\n");
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct AutofixArgs {
    /// Path to a JSON report produced by `fermut run --json …`.
    pub(crate) report: std::path::PathBuf,

    /// Mutant selector: 1-based index, or substring of mutant id.
    /// Omit when `--all-survivors` is set.
    pub(crate) target: Option<String>,

    /// Fix every surviving (or timed-out) mutant in the report.
    #[arg(long)]
    pub(crate) all_survivors: bool,

    /// Python source root (for the runner + baseline). Defaults to the
    /// configured source_root or `.`.
    #[arg(long, default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    /// Tests directory: mined for style samples + the apply target, and
    /// mirrored by the verifier. Defaults to the configured tests dir.
    #[arg(long)]
    pub(crate) tests: Option<std::path::PathBuf>,

    /// Python interpreter (path) or virtualenv (dir) the verifier runs
    /// pytest with (`<python> -m pytest`). Same discovery as `fermut run`.
    #[arg(long, value_name = "PATH")]
    pub(crate) python: Option<std::path::PathBuf>,

    /// Force generated tests into this file instead of the inferred one.
    /// Must live inside the tests tree or the verifier won't see it.
    #[arg(long)]
    pub(crate) out: Option<std::path::PathBuf>,

    /// Anthropic model id. Defaults to `claude-sonnet-4-6`.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Source lines of context around the mutant in the prompt.
    #[arg(long, default_value_t = 8)]
    pub(crate) context: usize,

    /// How many existing tests to include in the prompt for style.
    #[arg(long, default_value_t = 2)]
    pub(crate) sample_count: usize,

    /// Per-mutant verification timeout, in seconds.
    #[arg(long, value_name = "SECS")]
    pub(crate) timeout: Option<u64>,

    /// Disable the LLM response cache.
    #[arg(long)]
    pub(crate) no_cache: bool,

    /// Custom path for the LLM response cache.
    #[arg(long)]
    pub(crate) cache_path: Option<std::path::PathBuf>,

    /// Keep generated tests even when verification fails (default reverts
    /// them). Useful for inspecting why a suggestion didn't work.
    #[arg(long)]
    pub(crate) keep_failed: bool,

    /// Output format. `json` (default) emits the structured report;
    /// `human` prints a per-mutant summary.
    #[arg(long, value_enum, default_value_t = crate::cli::Format::Json)]
    pub(crate) format: crate::cli::Format,

    /// Ask an LLM equivalence judge about each survivor first (one extra,
    /// cached call each). `skip`: report survivors judged equivalent as
    /// `likely-equivalent` without generating, and steer the rest with the
    /// judge's explanation of how the mutant behaves differently. `audit`:
    /// judge, but still generate and verify every survivor, and count how
    /// often a "likely equivalent" one was killed anyway (the judge's
    /// measured false positives). The mutation score never changes.
    #[arg(long, value_enum, default_value_t = TriageMode::Off)]
    pub(crate) triage: TriageMode,
}
