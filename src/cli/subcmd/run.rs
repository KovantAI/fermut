//! `fermut run` — generate mutants, run pytest against each, report and gate.
//!
//! The bulk here is the arg surface (every knob `run` exposes) plus the
//! report/gate orchestration: build the runtime config, run the engine (or the
//! watch loop), emit every requested report format, then decide the exit code
//! from the score, the regression gate, and `--no-fail`. The gate primitives
//! themselves live in [`crate::cli::gate`] so they stay unit-testable.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::build_config::build_config;
use crate::cli::convert::{CacheScopeCli, Format, IsolationCli, RunnerCli};
use crate::cli::gate::{gate_exits_nonzero, load_prior_excluding, scoreless_note};
use crate::cli::merge::parse_shard_spec;
use crate::cli::FilterArgs;
use crate::history::HistoryEntry;
use crate::report::Report;

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct RunArgs {
    /// Python source root (package or module path).
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Test directory passed to pytest. Defaults to `<path>/tests`.
    #[arg(long)]
    pub tests: Option<PathBuf>,

    /// Parallel worker count. Defaults to logical CPU count.
    #[arg(long)]
    pub jobs: Option<usize>,

    /// Per-mutant pytest timeout, in seconds.
    #[arg(long)]
    pub timeout: Option<u64>,

    /// Skip the ty pre-filter stage.
    #[arg(long)]
    pub no_ty_filter: bool,

    /// Enable the ruff lint pre-filter. Requires `ruff` on PATH.
    #[arg(long)]
    pub ruff_filter: bool,

    /// Pin the Hypothesis seed across every mutant run for determinism.
    /// Passes `--hypothesis-seed=<N>` to pytest. Without this, Hypothesis
    /// tests can mask or fabricate survivors via random example draws.
    #[arg(long)]
    pub hypothesis_seed: Option<u64>,

    /// Extra arguments forwarded to pytest. Repeatable.
    /// Example: `--pytest-arg "-k" --pytest-arg "myfilter"`.
    #[arg(long = "pytest-arg", value_name = "ARG")]
    pub pytest_args: Vec<String>,

    /// Disable the result cache (`.fermut/cache.json`).
    #[arg(long)]
    pub no_cache: bool,

    /// Custom path for the result cache file.
    #[arg(long)]
    pub cache_path: Option<PathBuf>,

    /// Disable the run-history log (`.fermut/history.jsonl`).
    /// History is what `fermut trend` reads.
    #[arg(long)]
    pub no_history: bool,

    /// Custom path for the run-history log.
    #[arg(long)]
    pub history_path: Option<PathBuf>,

    /// Test only this fraction of mutants (0.0–1.0). Deterministic for a
    /// fixed `--sample-seed`. Useful for fast feedback on huge repos.
    #[arg(long, value_name = "RATIO")]
    pub sample: Option<f64>,

    /// Seed for `--sample` selection. Defaults to 0.
    #[arg(long, value_name = "N")]
    pub sample_seed: Option<u64>,

    /// Distributed execution: process only the i-th of n disjoint slices.
    /// Format `i/n`, both 1-based. Run all n in parallel (CI matrix,
    /// separate hosts), then `fermut merge` the JSON reports.
    #[arg(long, value_name = "I/N", value_parser = parse_shard_spec)]
    pub shard: Option<(u32, u32)>,

    /// Test runner. `pytest` (default), `rstest` (pytest-compatible
    /// drop-in), or `unittest`.
    #[arg(long, value_enum)]
    pub runner: Option<RunnerCli>,

    /// Python interpreter (path) or virtualenv (dir) to run pytest with.
    /// fermut invokes `<python> -m pytest`, so it uses that interpreter's
    /// pytest with no reliance on PATH — useful in restricted sandboxes/CI
    /// that won't let you activate a venv. When omitted, fermut
    /// auto-discovers an active venv or a nearby `.venv`, else falls back
    /// to a bare `pytest` on PATH.
    #[arg(long, value_name = "PATH")]
    pub python: Option<PathBuf>,

    /// Per-worker mirror isolation scheme.
    /// `auto` (default) picks reflink/clonefile when supported else copy.
    /// `hardlink` is fastest but shares inodes — unsafe if tests write
    /// back into the source tree. `copy` is the original behavior.
    #[arg(long, value_enum)]
    pub isolation: Option<IsolationCli>,

    /// Disable the equivalent-mutant detector. By default, survivors are
    /// post-processed by an AST-pattern + CPython-bytecode check; mutants
    /// proven equivalent are excluded from the score.
    #[arg(long)]
    pub no_equiv_detect: bool,

    /// Cache-key granularity for source identity.
    /// `file` (default) keys cache entries on the AST hash of the whole
    /// file — any structural edit invalidates every mutant in the file.
    /// `scope` keys on the file prelude + the enclosing top-level
    /// def/class body, so edits inside one function leave cache hits
    /// intact for mutants in sibling functions. `scope` is opt-in
    /// because it can return stale verdicts when a test for one
    /// function indirectly calls another.
    #[arg(long, value_enum)]
    pub cache_scope: Option<CacheScopeCli>,

    /// Emit GitHub Actions workflow-command annotations to stdout for
    /// each survivor / timeout / error. Auto-on when `GITHUB_ACTIONS=true`.
    #[arg(long)]
    pub annotate: bool,

    /// Watch the source root and re-run on every `.py` change. Loops
    /// until interrupted with Ctrl+C.
    #[arg(long)]
    pub watch: bool,

    /// stdout output format.
    #[arg(long, value_enum, default_value_t = Format::Human)]
    pub format: Format,

    /// Also write JSON report to this path (consumed by `fermut show`).
    #[arg(long)]
    pub json: Option<PathBuf>,

    /// Also write JUnit XML report to this path.
    #[arg(long)]
    pub junit: Option<PathBuf>,

    /// Also write HTML report to this path.
    #[arg(long)]
    pub html: Option<PathBuf>,

    /// Also write a Markdown report (suitable for PR comments) to this path.
    #[arg(long)]
    pub markdown: Option<PathBuf>,

    /// Include a compact trend block (sparkline + delta vs previous
    /// run) at the top of the Markdown report. Reads prior entries
    /// from the history log; no-op when there's no prior history or
    /// `--markdown` isn't set.
    #[arg(long)]
    pub trend: bool,

    /// Restrict the markdown trend block's "previous run" lookup to
    /// entries recorded on this branch. Useful in CI where the cache
    /// restores main-branch history into a PR build — pin to `main`
    /// so the trend compares against main, not against another PR.
    #[arg(long, value_name = "NAME", requires = "trend")]
    pub trend_branch: Option<String>,

    /// Exit non-zero if the mutation score dropped more than this
    /// many points vs the most recent prior run on the same git
    /// branch. Requires history (will warn + exit non-zero when
    /// `--no-history` is in effect). Ignored in `--watch` mode.
    #[arg(long, value_name = "PTS")]
    pub fail_on_regression: Option<f64>,

    /// Fail the run only when the mutation score is below this percentage
    /// (0.0–100.0). A score exactly equal to the threshold passes.
    /// Without it, any survivor exits 1.
    #[arg(long, value_name = "SCORE")]
    pub fail_under: Option<f64>,

    /// Never exit non-zero because of the mutation result — write every
    /// report and exit 0 even with survivors. For runs that only produce a
    /// report (a trend shard, a dashboard feed) where the mutants exist to
    /// be recorded, not to gate. Replaces the `--fail-under 0` idiom.
    /// Mutually exclusive with the gate flags. Note this also suppresses
    /// the exit for a scoreless/all-errored run (the vacuous-100% guard):
    /// the broken run is reported as N/A but still exits 0, so pair a
    /// report-only shard with a separate gated step if you need to catch a
    /// suite that errors under mutation.
    #[arg(long, conflicts_with_all = ["fail_under", "fail_on_regression"])]
    pub no_fail: bool,

    /// Skip the pre-flight check that the unmutated suite passes. fermut
    /// runs your full test suite once before mutating; a red or erroring
    /// suite makes every covered mutant look killed and inflates the
    /// score. Pass this only when you've already confirmed the suite is
    /// green (e.g. CI ran it in a prior step).
    #[arg(long)]
    pub no_verify_baseline: bool,

    /// Wall-clock cap (seconds) for the baseline run. The baseline runs
    /// the whole suite once, so this is separate from `--timeout` (which
    /// bounds a single mutant). Default 300. Raise it for large suites; a
    /// suite that exceeds it is killed and the run aborts.
    #[arg(long, value_name = "SECS")]
    pub baseline_timeout: Option<u64>,

    /// Disable smart test ordering. By default, when coverage selects
    /// multiple tests for a mutant, fermut runs the most targeted one first
    /// so pytest's `-x` short-circuits sooner: the cold-start breadth prior
    /// (covering the fewest of the mutated file's lines) sets the order, and
    /// any test that historically killed this file+operator is lifted ahead
    /// of it (kill history in `.fermut/kill-order.json`). Ordering only
    /// permutes the selected set, so it never changes the mutation score —
    /// under `--timeout` it can flip a `timed_out` into a `killed`, but both
    /// count as detected, so the score and the `--fail-on-regression` gate
    /// stay order-invariant. Killer-first ordering helps most *with* a
    /// timeout, by reaching the kill before the deadline. Disabling it only
    /// affects speed. Also settable via `smart_order = false`.
    #[arg(long)]
    pub no_smart_order: bool,

    /// Force smart test ordering on (over `smart_order = false` in config).
    /// Conflicts with `--no-smart-order`.
    #[arg(long, conflicts_with = "no_smart_order")]
    pub smart_order: bool,

    /// Wall-clock ceiling (seconds) on the per-mutant testing phase. When
    /// set, mutants are evaluated highest-value first (covered mutants
    /// before uncovered) and, once the deadline passes, every mutant not
    /// yet started is recorded as `skipped` (filter `time-budget`) instead
    /// of run; mutants already in flight finish. Gives a PR gate a
    /// predictable ceiling — a time cap beats a mutant cap for CI trust.
    /// Bounds only the testing phase: baseline verification, generation,
    /// and the ty pre-filter are separate fixed costs it does not cover.
    #[arg(long, value_name = "SECS")]
    pub max_time: Option<u64>,

    #[command(flatten)]
    pub filter: FilterArgs,
}

pub fn run(args: RunArgs) -> Result<()> {
    let RunArgs {
        path,
        tests,
        jobs,
        timeout,
        no_ty_filter,
        ruff_filter,
        hypothesis_seed,
        pytest_args,
        no_cache,
        cache_path,
        no_history,
        history_path,
        sample,
        sample_seed,
        shard,
        runner,
        python,
        isolation,
        no_equiv_detect,
        cache_scope,
        annotate,
        watch,
        format,
        json,
        junit,
        html,
        markdown,
        trend,
        trend_branch,
        fail_on_regression,
        fail_under,
        no_fail,
        no_verify_baseline,
        baseline_timeout,
        no_smart_order,
        smart_order,
        max_time,
        filter: f,
    } = args;

    let cfg = build_config(
        path,
        tests,
        jobs,
        timeout,
        no_ty_filter,
        ruff_filter,
        hypothesis_seed,
        pytest_args,
        no_cache,
        cache_path,
        no_history,
        history_path,
        sample,
        sample_seed,
        shard,
        runner.map(Into::into),
        python,
        isolation.map(Into::into),
        no_equiv_detect,
        cache_scope.map(Into::into),
        fail_under,
        no_verify_baseline,
        baseline_timeout,
        no_smart_order,
        smart_order,
        max_time,
        f,
    )?;
    let want_annotations = annotate || std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    let fmt = format.into();
    // The closure receives the *prior* history (entries that existed before
    // this run started) so the markdown trend block plots "this run vs every
    // earlier run" without depending on whether `engine::run`'s append to the
    // history file succeeded. Append can silently fail on read-only
    // filesystems or full disks; the caller is responsible for handing us the
    // snapshot.
    let on_report = move |report: &Report, prior: &[HistoryEntry]| -> Result<()> {
        report.print(fmt);
        if want_annotations {
            report.print_github_annotations();
        }
        if let Some(p) = &json {
            report.write_json(p)?;
        }
        if let Some(p) = &junit {
            report.write_junit(p)?;
        }
        if let Some(p) = &html {
            report.write_html(p)?;
        }
        if let Some(p) = &markdown {
            if trend {
                let filtered: Vec<HistoryEntry> = match &trend_branch {
                    Some(b) => prior
                        .iter()
                        .filter(|e| e.git_branch.as_deref() == Some(b.as_str()))
                        .cloned()
                        .collect(),
                    None => prior.to_vec(),
                };
                report.write_markdown_with_history(p, &filtered)?;
            } else {
                report.write_markdown(p)?;
            }
        }
        Ok(())
    };
    if watch {
        crate::watch::watch_loop(&cfg, on_report)
    } else {
        let prior_snapshot = if cfg.history {
            crate::history::load(&cfg.history_path).unwrap_or_default()
        } else {
            Vec::new()
        };
        let (report, current_entry) = crate::engine::run(&cfg)?;
        on_report(&report, &prior_snapshot)?;
        let mut gate_failed = false;
        if let Some(threshold) = fail_on_regression {
            if !cfg.history {
                eprintln!(
                    "--fail-on-regression needs history enabled \
                     (currently --no-history)"
                );
                gate_failed = true;
            } else if let Some(current) = current_entry.as_ref() {
                if current.partial {
                    // A `--max-time` run truncated by the budget scored a
                    // nondeterministic subset, so it can't anchor a regression
                    // comparison — `regression_against` excludes it. Say so, or
                    // a green gate reads as "no regression" when it's really
                    // "not evaluated".
                    eprintln!(
                        "--fail-on-regression skipped: this run hit the --max-time \
                         budget and scored a partial subset (not comparable). Run \
                         without --max-time to gate on regression."
                    );
                }
                // Reload post-run so concurrent writers (e.g. parallel
                // `--shard` workers) become visible, then compare against the
                // in-memory current entry — never against `entries.last()`,
                // which is the *previous* run when our own append silently
                // failed.
                let prior_now = load_prior_excluding(&cfg.history_path, current);
                if let Some(drop) = crate::history::regression_against(&prior_now, current) {
                    if drop > threshold {
                        eprintln!(
                            "mutation score regressed by {drop:.1} pts \
                             (threshold {threshold:.1})"
                        );
                        gate_failed = true;
                    }
                }
            }
        }
        // A zero-denominator run has no score. Say so, so a green gate is never
        // mistaken for a genuine 100% (an all-errored run fails below;
        // nothing-to-score with no errors passes as a legitimate N/A). Under
        // `--no-fail` the exit is suppressed regardless, so the errored case
        // reports N/A too — claiming "the gate fails" would contradict the
        // exit-0 that follows.
        if report.is_scoreless() {
            eprintln!("{}", scoreless_note(&report, no_fail));
        }
        // `--no-fail`: the run exists to produce a report, not to gate.
        // Conflicts with the gate flags at the parser, so `gate_failed` is
        // always false here; the guard is explicit so the intent survives a
        // future flag that sets it.
        if gate_exits_nonzero(no_fail, report.should_fail(cfg.fail_under), gate_failed) {
            std::process::exit(1);
        }
        Ok(())
    }
}
