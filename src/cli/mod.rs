//! Command-line interface: clap parser + dispatch.
//!
//! - `build_config` — merges CLI args with the loaded config file into a
//!   runtime [`Config`](crate::config::Config). Pure function, easy to test.
//! - `subcmd` — one handler per `Cmd` variant (`show`, `clean`). The `Run` and
//!   `List` variants are handled inline because they touch the engine and
//!   filter chain directly.

pub(crate) mod build_config;
mod merge;
mod subcmd;

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use tracing_subscriber::EnvFilter;

use crate::filter;
use crate::history::HistoryEntry;
use crate::report::{Report, ReportFormat};

use build_config::build_config;
use merge::{merge_reports, parse_shard_spec};
use subcmd::{
    autofix::{autofix, AutofixFormat, AutofixOpts},
    baseline::{baseline, BaselineFormat, BaselineOpts},
    clean::clean_cache,
    coverage::{coverage, CoverageOpts},
    dashboard::{dashboard, DashboardOpts},
    doctor::{doctor, DoctorOpts},
    explain::{explain, ExplainFormat, ExplainOpts},
    init::{init, InitOpts},
    mcp::serve as mcp_serve,
    migrate::{migrate, MigrateOpts, MigrateSource},
    next::{next, NextFormat, NextOpts},
    pr_comment::{pr_comment, PrCommentOpts},
    score::{score, ScoreFormat, ScoreOpts},
    show::show,
    suggest::{suggest, SuggestFormat, SuggestOpts},
    trend::{trend, TrendFormat, TrendGroupBy, TrendOpts, TrendScale},
};

#[derive(Parser, Debug)]
#[command(
    name = "fermut",
    version,
    about = "Rust-powered, ty-aware mutation testing for Python"
)]
pub struct Cli {
    /// Increase log verbosity. `-v` enables DEBUG, `-vv` enables TRACE.
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Suppress all non-error logs. Wins over `-v`.
    #[arg(short = 'q', long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

impl Cli {
    /// Convert verbosity flags to a `tracing` `EnvFilter`. Honors `RUST_LOG`
    /// when present, falls back to a sensible per-flag default otherwise.
    pub fn tracing_filter(&self) -> EnvFilter {
        if let Ok(env) = std::env::var("RUST_LOG") {
            return EnvFilter::new(env);
        }
        let directive = if self.quiet {
            "fermut=error"
        } else {
            match self.verbose {
                0 => "fermut=info",
                1 => "fermut=debug",
                _ => "fermut=trace",
            }
        };
        EnvFilter::new(directive)
    }
}

/// Args common to every subcommand that needs to compute the filter chain.
#[derive(Args, Debug, Clone)]
pub(crate) struct FilterArgs {
    /// Restrict to these operators (comma-separated names, e.g. `arith-op-swap,boundary-shift`).
    #[arg(long, value_delimiter = ',')]
    pub ops: Option<Vec<String>>,

    /// Drop these operators (comma-separated). Wins over `--ops`.
    #[arg(long, value_delimiter = ',')]
    pub skip_ops: Option<Vec<String>>,

    /// Restrict mutations to lines changed vs base ref. Defaults to `main`.
    #[arg(long, num_args = 0..=1, default_missing_value = "main", conflicts_with_all = ["since", "no_diff_only"])]
    pub diff_only: Option<String>,

    /// Restrict mutations to lines touched since a commit or date. The
    /// value can be a git ref (`abc123`, `v1.2.0`, `HEAD~10`, `main`) or a
    /// date string git understands (`2025-12-01`, `'1 week ago'`).
    /// Includes uncommitted edits. Mutually exclusive with `--diff-only`.
    #[arg(long, value_name = "SPEC", conflicts_with = "no_diff_only")]
    pub since: Option<String>,

    /// Disable diff filtering even when `diff_only`/`since` is set in the
    /// config file. Use this from a nightly workflow to force a full sweep
    /// against a `pr-gate`-shaped config.
    #[arg(long, conflicts_with_all = ["diff_only", "since"])]
    pub no_diff_only: bool,

    /// Path to a coverage.json (from `coverage json -o coverage.json`).
    /// Drops mutants on lines no test executes. Defaults to `coverage.json`.
    #[arg(long, num_args = 0..=1, default_missing_value = "coverage.json", conflicts_with = "no_coverage")]
    pub coverage: Option<PathBuf>,

    /// Disable the coverage filter even when `coverage` is set in the
    /// config file. Mirrors `--no-diff-only` for nightly-sweep workflows.
    #[arg(long, conflicts_with = "coverage")]
    pub no_coverage: bool,

    /// Include experimental operators (exception swaps, loop iteration).
    #[arg(long)]
    pub experimental: bool,

    /// Include parity operators (expr→None, positional/element drop, string
    /// case-swap). These exist to broaden overlap with other mutation tools
    /// (mutmut) for cross-tool comparison, NOT for normal scoring — they are
    /// very noisy. Off by default.
    #[arg(long)]
    pub parity: bool,

    /// Exclude paths from mutation collection. Repeatable. Patterns are
    /// globs matched against paths relative to the source root. Examples:
    /// `--exclude 'alembic/**' --exclude 'tests/integration/**'`.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Generate mutants, run pytest against each, report killed/survived.
    Run {
        /// Python source root (package or module path).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Test directory passed to pytest. Defaults to `<path>/tests`.
        #[arg(long)]
        tests: Option<PathBuf>,

        /// Parallel worker count. Defaults to logical CPU count.
        #[arg(long)]
        jobs: Option<usize>,

        /// Per-mutant pytest timeout, in seconds.
        #[arg(long)]
        timeout: Option<u64>,

        /// Skip the ty pre-filter stage.
        #[arg(long)]
        no_ty_filter: bool,

        /// Enable the ruff lint pre-filter. Requires `ruff` on PATH.
        #[arg(long)]
        ruff_filter: bool,

        /// Pin the Hypothesis seed across every mutant run for determinism.
        /// Passes `--hypothesis-seed=<N>` to pytest. Without this, Hypothesis
        /// tests can mask or fabricate survivors via random example draws.
        #[arg(long)]
        hypothesis_seed: Option<u64>,

        /// Extra arguments forwarded to pytest. Repeatable.
        /// Example: `--pytest-arg "-k" --pytest-arg "myfilter"`.
        #[arg(long = "pytest-arg", value_name = "ARG")]
        pytest_args: Vec<String>,

        /// Disable the result cache (`.fermut/cache.json`).
        #[arg(long)]
        no_cache: bool,

        /// Custom path for the result cache file.
        #[arg(long)]
        cache_path: Option<PathBuf>,

        /// Disable the run-history log (`.fermut/history.jsonl`).
        /// History is what `fermut trend` reads.
        #[arg(long)]
        no_history: bool,

        /// Custom path for the run-history log.
        #[arg(long)]
        history_path: Option<PathBuf>,

        /// Test only this fraction of mutants (0.0–1.0). Deterministic for a
        /// fixed `--sample-seed`. Useful for fast feedback on huge repos.
        #[arg(long, value_name = "RATIO")]
        sample: Option<f64>,

        /// Seed for `--sample` selection. Defaults to 0.
        #[arg(long, value_name = "N")]
        sample_seed: Option<u64>,

        /// Distributed execution: process only the i-th of n disjoint slices.
        /// Format `i/n`, both 1-based. Run all n in parallel (CI matrix,
        /// separate hosts), then `fermut merge` the JSON reports.
        #[arg(long, value_name = "I/N", value_parser = parse_shard_spec)]
        shard: Option<(u32, u32)>,

        /// Test runner. `pytest` (default), `rstest` (pytest-compatible
        /// drop-in), or `unittest`.
        #[arg(long, value_enum)]
        runner: Option<RunnerCli>,

        /// Python interpreter (path) or virtualenv (dir) to run pytest with.
        /// fermut invokes `<python> -m pytest`, so it uses that interpreter's
        /// pytest with no reliance on PATH — useful in restricted sandboxes/CI
        /// that won't let you activate a venv. When omitted, fermut
        /// auto-discovers an active venv or a nearby `.venv`, else falls back
        /// to a bare `pytest` on PATH.
        #[arg(long, value_name = "PATH")]
        python: Option<PathBuf>,

        /// Per-worker mirror isolation scheme.
        /// `auto` (default) picks reflink/clonefile when supported else copy.
        /// `hardlink` is fastest but shares inodes — unsafe if tests write
        /// back into the source tree. `copy` is the original behavior.
        #[arg(long, value_enum)]
        isolation: Option<IsolationCli>,

        /// Disable the equivalent-mutant detector. By default, survivors are
        /// post-processed by an AST-pattern + CPython-bytecode check; mutants
        /// proven equivalent are excluded from the score.
        #[arg(long)]
        no_equiv_detect: bool,

        /// Cache-key granularity for source identity.
        /// `file` (default) keys cache entries on the AST hash of the whole
        /// file — any structural edit invalidates every mutant in the file.
        /// `scope` keys on the file prelude + the enclosing top-level
        /// def/class body, so edits inside one function leave cache hits
        /// intact for mutants in sibling functions. `scope` is opt-in
        /// because it can return stale verdicts when a test for one
        /// function indirectly calls another.
        #[arg(long, value_enum)]
        cache_scope: Option<CacheScopeCli>,

        /// Emit GitHub Actions workflow-command annotations to stdout for
        /// each survivor / timeout / error. Auto-on when `GITHUB_ACTIONS=true`.
        #[arg(long)]
        annotate: bool,

        /// Watch the source root and re-run on every `.py` change. Loops
        /// until interrupted with Ctrl+C.
        #[arg(long)]
        watch: bool,

        /// stdout output format.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,

        /// Also write JSON report to this path (consumed by `fermut show`).
        #[arg(long)]
        json: Option<PathBuf>,

        /// Also write JUnit XML report to this path.
        #[arg(long)]
        junit: Option<PathBuf>,

        /// Also write HTML report to this path.
        #[arg(long)]
        html: Option<PathBuf>,

        /// Also write a Markdown report (suitable for PR comments) to this path.
        #[arg(long)]
        markdown: Option<PathBuf>,

        /// Include a compact trend block (sparkline + delta vs previous
        /// run) at the top of the Markdown report. Reads prior entries
        /// from the history log; no-op when there's no prior history or
        /// `--markdown` isn't set.
        #[arg(long)]
        trend: bool,

        /// Restrict the markdown trend block's "previous run" lookup to
        /// entries recorded on this branch. Useful in CI where the cache
        /// restores main-branch history into a PR build — pin to `main`
        /// so the trend compares against main, not against another PR.
        #[arg(long, value_name = "NAME", requires = "trend")]
        trend_branch: Option<String>,

        /// Exit non-zero if the mutation score dropped more than this
        /// many points vs the most recent prior run on the same git
        /// branch. Requires history (will warn + exit non-zero when
        /// `--no-history` is in effect). Ignored in `--watch` mode.
        #[arg(long, value_name = "PTS")]
        fail_on_regression: Option<f64>,

        /// Fail the run only when the mutation score is below this percentage
        /// (0.0–100.0). A score exactly equal to the threshold passes.
        /// Without it, any survivor exits 1.
        #[arg(long, value_name = "SCORE")]
        fail_under: Option<f64>,

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
        no_fail: bool,

        /// Skip the pre-flight check that the unmutated suite passes. fermut
        /// runs your full test suite once before mutating; a red or erroring
        /// suite makes every covered mutant look killed and inflates the
        /// score. Pass this only when you've already confirmed the suite is
        /// green (e.g. CI ran it in a prior step).
        #[arg(long)]
        no_verify_baseline: bool,

        /// Wall-clock cap (seconds) for the baseline run. The baseline runs
        /// the whole suite once, so this is separate from `--timeout` (which
        /// bounds a single mutant). Default 300. Raise it for large suites; a
        /// suite that exceeds it is killed and the run aborts.
        #[arg(long, value_name = "SECS")]
        baseline_timeout: Option<u64>,

        /// Wall-clock ceiling (seconds) on the per-mutant testing phase. When
        /// set, mutants are evaluated highest-value first (covered mutants
        /// before uncovered) and, once the deadline passes, every mutant not
        /// yet started is recorded as `skipped` (filter `time-budget`) instead
        /// of run; mutants already in flight finish. Gives a PR gate a
        /// predictable ceiling — a time cap beats a mutant cap for CI trust.
        /// Bounds only the testing phase: baseline verification, generation,
        /// and the ty pre-filter are separate fixed costs it does not cover.
        #[arg(long, value_name = "SECS")]
        max_time: Option<u64>,

        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Enumerate mutations without running tests.
    List {
        #[arg(default_value = ".")]
        path: PathBuf,

        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Run a Model Context Protocol server over stdio.
    ///
    /// Exposes fermut to a coding agent (Claude Code, Cursor, …) as native
    /// MCP tools — `fermut_run`, `fermut_next`, `fermut_score`,
    /// `fermut_list_survivors` — instead of the agent shelling out and
    /// parsing CLI output. Speaks newline-delimited JSON-RPC 2.0 on
    /// stdin/stdout; stdout is the protocol channel, so logs go to stderr.
    /// Launched by an MCP client, not run interactively.
    Mcp,

    /// Print shell completion script to stdout (`bash`, `zsh`, `fish`, `powershell`, `elvish`).
    /// Example: `fermut completions zsh > ~/.zfunc/_fermut`.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Detect project layout and emit a starter `fermut.toml`.
    ///
    /// Walks up from PATH to find the project root (pyproject.toml or
    /// setup.cfg), detects the source/tests directories and which tools
    /// are on PATH (pytest, unittest, ty), and writes a config tuned to
    /// the repo size. Optionally drops a PR-gate GitHub Actions workflow.
    Init {
        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Write `[tool.fermut]` into `pyproject.toml` instead of a
        /// standalone `fermut.toml`.
        #[arg(long)]
        pyproject: bool,

        /// Overwrite an existing `fermut.toml` or `[tool.fermut]` block.
        #[arg(long)]
        force: bool,

        /// Also drop a PR-gate workflow at `.github/workflows/fermut.yml`.
        #[arg(long)]
        with_gha: bool,

        /// Wire `coverage = "coverage.json"` into the config even when no
        /// coverage dependency is detected in the project. Use when you'll
        /// install `coverage` yourself — `init` prints the install command
        /// matching your package manager.
        #[arg(long)]
        with_coverage: bool,

        /// Pre-seed the config with one of fermut's curated profiles:
        /// `pr-gate` (CI gate), `nightly` (full sweep), `local` (dev loop),
        /// or `library` (lib authors). Without `--profile`, init falls back
        /// to a size-based heuristic.
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,

        /// Print the profile catalogue (name, description, key overrides)
        /// and exit. Nothing is written.
        #[arg(long)]
        list_profiles: bool,

        /// Print what would be written without touching the filesystem.
        #[arg(long)]
        dry_run: bool,
    },

    /// Visualize the mutation-score history (`.fermut/history.jsonl`).
    ///
    /// Each `fermut run` appends one entry; `trend` prints a table of the
    /// most recent ones together with an ASCII sparkline and per-run
    /// deltas. Disable history capture with `--no-history` on `run`.
    Trend {
        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Custom path to the history log. Defaults to `<path>/.fermut/history.jsonl`.
        #[arg(long)]
        history_path: Option<PathBuf>,

        /// Show only the most recent N entries (default 10). Ignored when `--all` is set.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Show every recorded entry.
        #[arg(long)]
        all: bool,

        /// Keep only entries recorded on this git branch. Useful in CI where
        /// feature-branch runs would otherwise dilute the main-branch trend.
        #[arg(long)]
        branch: Option<String>,

        /// Keep only entries newer than this date. Accepts `YYYY-MM-DD`
        /// (start-of-day UTC) or a full `YYYY-MM-DDTHH:MM:SSZ` timestamp.
        #[arg(long, value_name = "DATE")]
        since: Option<String>,

        /// Keep only entries older than this date. Accepts `YYYY-MM-DD`
        /// (end-of-day UTC, so the full day is included) or a full
        /// `YYYY-MM-DDTHH:MM:SSZ` timestamp.
        #[arg(long, value_name = "DATE")]
        until: Option<String>,

        /// Exit non-zero if the most recent run dropped more than this many
        /// score points vs the previous filtered entry. CI gate.
        #[arg(long, value_name = "PTS")]
        fail_on_regression: Option<f64>,

        /// Sparkline scaling. `fixed` (default) renders against [0, 100]
        /// so charts from different windows are visually comparable.
        /// `auto` rescales to the window's own min/max to expose small
        /// drifts; loses cross-window comparability and falls back to
        /// fixed when the window's span is under 1 point.
        #[arg(long, value_enum, default_value_t = TrendScaleCli::Fixed)]
        scale: TrendScaleCli,

        /// Expand the per-run survivor diff into the full new-survivor and
        /// newly-killed mutant-id lists below the table. Without this flag,
        /// only the counts are shown.
        #[arg(long)]
        diff: bool,

        /// Aggregate the latest run's survivors. `file` groups by source
        /// file and prints counts + the oldest survivor's age per group.
        /// Reads only data already stored in `history.jsonl`, no report
        /// needed.
        #[arg(long, value_enum, value_name = "AXIS")]
        by: Option<TrendGroupByCli>,

        /// stdout output format.
        #[arg(long, value_enum, default_value_t = TrendFormatCli::Human)]
        format: TrendFormatCli,

        /// Fail if the history log has any malformed (corrupt, unparseable)
        /// line instead of silently skipping it, so a `fermut trend --strict`
        /// CI step catches a truncated history file before the trend is
        /// trusted. Newer-schema lines an older binary can't read are warned,
        /// not failed — nothing this binary can do about them. Does not change
        /// the `fermut run --fail-on-regression` gate, which loads history on
        /// its own; run this as a separate step to guard the shared file.
        #[arg(long)]
        strict: bool,
    },

    /// Emit the agent reward signal for the latest run: score, delta vs a
    /// baseline run, and the new-survivor / newly-killed mutant-id sets.
    ///
    /// Reads `.fermut/history.jsonl` and compares the most recent entry
    /// against an earlier branch-comparable one. Built for an agent's
    /// post-iteration check — "did this iteration help?" — so JSON is the
    /// default format. The `regressed` flag is true when the score dropped
    /// or a previously-killed mutant came back.
    Score {
        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Custom path to the history log. Defaults to `<path>/.fermut/history.jsonl`.
        #[arg(long)]
        history_path: Option<PathBuf>,

        /// Compare against the entry this many branch-comparable runs back.
        /// `1` (default) is the immediately prior run.
        #[arg(long, default_value_t = 1, value_name = "N")]
        baseline: usize,

        /// Restrict current/baseline selection to this git branch. Pin to
        /// `main` in CI where the cache restores main-branch history into a
        /// PR build.
        #[arg(long)]
        branch: Option<String>,

        /// Exit non-zero if the score dropped more than this many points vs
        /// the baseline. Agent rollback / CI gate.
        #[arg(long, value_name = "PTS")]
        fail_on_regression: Option<f64>,

        /// Output format. `json` (default) emits the reward signal for
        /// machine consumers; `human` prints a short summary.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// Generate a self-contained HTML dashboard combining the trend log
    /// with the latest run's survivor drill-down. The page is a single
    /// file with inline CSS and SVG — no JavaScript, no external assets.
    /// Pair with `--report <run.json>` (from `fermut run --json`) to
    /// embed per-survivor source diffs.
    Dashboard {
        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Custom path to the history log. Defaults to `<path>/.fermut/history.jsonl`.
        #[arg(long)]
        history_path: Option<PathBuf>,

        /// Output path for the generated HTML. Created if missing.
        #[arg(long, default_value = "fermut-dashboard.html")]
        output: PathBuf,

        /// Optional JSON report (from `fermut run --json`) used to
        /// attach inline source diffs to each survivor. Without it the
        /// survivor list still renders, but only the IDs are shown.
        #[arg(long)]
        report: Option<PathBuf>,

        /// How many trailing history entries to show in the chart and
        /// table. Defaults to 30.
        #[arg(long, default_value_t = 30)]
        limit: usize,

        /// After writing, hand the file off to the OS's default browser
        /// (`open` / `xdg-open` / `start`). Best-effort: a failure to
        /// launch the helper degrades to a printed hint, not a non-zero
        /// exit.
        #[arg(long)]
        open: bool,
    },

    /// Diagnose environment + config: report which tools are present,
    /// whether the config is well-formed, and whether common gotchas
    /// (missing coverage contexts, unpinned Hypothesis seed, etc.) apply.
    /// Exits non-zero when any check fails (or any check warns with
    /// `--strict`).
    Doctor {
        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Treat warnings as failures (exit non-zero on any warn).
        #[arg(long)]
        strict: bool,
    },

    /// Day-one baseline: line coverage vs mutation score, and the gap
    /// between them. Sanity-checks the environment, builds coverage, runs a
    /// fast sampled mutation pass over covered code, and prints a graded
    /// verdict plus the worst files. Run this first; `next` to act on it.
    Baseline {
        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Mutate every covered mutant for the exact score instead of a
        /// sampled estimate. Slower; use once the sampled run looks right.
        #[arg(long)]
        full: bool,

        /// Sampling fraction (0.0–1.0) for the fast pass. Defaults to 0.1.
        /// Ignored with `--full`.
        #[arg(long, value_name = "RATIO")]
        sample: Option<f64>,

        /// How many worst-offender files to list. Defaults to 3.
        #[arg(long, default_value_t = 3, value_name = "N")]
        top: usize,

        /// Output format. `human` (default) prints the graded verdict;
        /// `json` emits the same numbers for an agent / dashboard.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,

        #[command(flatten)]
        filter: FilterArgs,
    },

    /// Wipe the `.fermut/` cache directory under PATH.
    Clean {
        /// Where to look for `.fermut/`. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Path of the history log to preserve. Overrides any value
        /// resolved from the config file. Defaults to
        /// `<path>/.fermut/history.jsonl`.
        #[arg(long)]
        history_path: Option<PathBuf>,
    },

    /// Generate or refresh the `.coverage` database used for per-mutant test
    /// selection — the one command to run after you touch your tests.
    ///
    /// With no `.coverage` yet, runs the full suite under coverage. With one
    /// present, runs only the test files changed since it was written and
    /// appends them in (purging their stale contexts first), so adding a test
    /// costs that file's runtime, not the whole suite. fermut reads the
    /// `.coverage` SQLite directly — no `coverage json` export needed.
    Coverage {
        /// Where to start project-root discovery. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Measured source root (`--cov=<this>`). Defaults to the configured
        /// `source_root`, else the project root.
        #[arg(long)]
        source: Option<PathBuf>,

        /// Tests location. Defaults to the configured `tests`, else `<source>/tests`.
        #[arg(long)]
        tests: Option<PathBuf>,

        /// Re-run the full suite even when an up-to-date `.coverage` exists.
        #[arg(long)]
        full: bool,

        /// Database output path. Defaults to `<project>/.coverage`.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Python interpreter (path) or virtualenv (dir) to run pytest with,
        /// same as `fermut run --python`. Lets coverage generation work
        /// without `pytest` on PATH. Auto-discovers a venv when omitted.
        #[arg(long, value_name = "PATH")]
        python: Option<PathBuf>,

        /// Extra arguments forwarded to pytest. Repeatable.
        #[arg(long = "pytest-arg", value_name = "ARG")]
        pytest_args: Vec<String>,
    },

    /// Merge multiple JSON reports (typically from shard runs) into one.
    Merge {
        /// Input JSON reports to combine. At least one required.
        #[arg(required = true, num_args = 1..)]
        inputs: Vec<PathBuf>,

        /// Write the combined JSON report here. Otherwise prints to stdout.
        #[arg(long)]
        json: Option<PathBuf>,

        /// Also write a JUnit XML report.
        #[arg(long)]
        junit: Option<PathBuf>,

        /// Also write an HTML report.
        #[arg(long)]
        html: Option<PathBuf>,

        /// Also write a Markdown report.
        #[arg(long)]
        markdown: Option<PathBuf>,

        /// Write a single history entry built from the merged report to this
        /// path (overwriting). Git sha/branch are read from the merge checkout
        /// and the fermut version is stamped automatically; the counts are the
        /// merged full-universe totals. Lets a sharded run record its trend
        /// point without harvesting a shard's history line as a template.
        #[arg(long, value_name = "PATH")]
        history: Option<PathBuf>,

        /// `config_hash` to stamp into the `--history` entry. Merge can't derive
        /// it (the run config lives in the shard jobs), so pass the value the
        /// shards recorded, e.g. `$(jq -r .config_hash shard-1-entry.json)`.
        #[arg(long, value_name = "HEX")]
        config_hash: Option<String>,

        /// Project root for git sha/branch discovery in the `--history` entry.
        /// Defaults to the current directory (the merge checkout).
        #[arg(long, value_name = "DIR", default_value = ".")]
        project: PathBuf,
    },

    /// Post a Markdown report to a pull request as a sticky comment.
    ///
    /// Edits the previous fermut comment in place on subsequent runs so a
    /// PR doesn't accumulate one comment per CI run. Discovery is based on
    /// the `<!-- fermut:report -->` marker that `fermut --markdown` writes
    /// at the top of every report. Requires `gh` on `PATH` (preinstalled
    /// on GitHub Actions runners; otherwise <https://cli.github.com>).
    PrComment {
        /// Markdown file to post (produced by a prior `fermut run --markdown …`).
        #[arg(long)]
        markdown: PathBuf,

        /// `owner/repo`. Defaults to `$GITHUB_REPOSITORY` (set automatically on GHA).
        #[arg(long)]
        repo: Option<String>,

        /// Pull-request number. Defaults to one inferred from `$GITHUB_REF` or `$PR_NUMBER`.
        #[arg(long)]
        pr: Option<u64>,

        /// Override the comment marker. Defaults to `<!-- fermut:report -->`.
        /// Use to keep multiple gates (e.g. per-shard) as independent comments.
        #[arg(long)]
        marker: Option<String>,

        /// Print the resolved plan without contacting GitHub.
        #[arg(long)]
        dry_run: bool,
    },

    /// Translate a mutmut or cosmic-ray config into a starter `[tool.fermut]`,
    /// and (for mutmut) rewrite `# pragma: no mutate` markers to `# fermut: ignore`.
    ///
    /// The translator is intentionally narrow — mutmut and cosmic-ray both
    /// have knobs without a fermut equivalent (celery, pre/post-mutation
    /// hooks, `dict_synonyms`, interceptors). Anything that can't be mapped
    /// is printed under "manual review" so nothing is silently dropped.
    Migrate {
        /// Which tool to migrate from.
        #[arg(value_enum)]
        from: MigrateSourceCli,

        /// Where to start the project-root walk. Defaults to cwd.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Explicit source config file. Defaults: `pyproject.toml` /
        /// `setup.cfg` for mutmut, `cosmic-ray.toml` for cosmic-ray.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,

        /// Write `[tool.fermut]` into `pyproject.toml` instead of a
        /// standalone `fermut.toml`.
        #[arg(long)]
        pyproject: bool,

        /// Overwrite an existing `fermut.toml` or `[tool.fermut]` block.
        #[arg(long)]
        force: bool,

        /// Print what would be written and which files would be rewritten,
        /// without touching the filesystem.
        #[arg(long)]
        dry_run: bool,

        /// Skip rewriting `# pragma: no mutate` → `# fermut: ignore`
        /// (mutmut only; no-op for cosmic-ray).
        #[arg(long)]
        no_pragma_rewrite: bool,
    },

    /// Inspect mutants from a prior JSON report.
    Show {
        /// Path to a JSON report produced by `fermut run --json …`.
        report: PathBuf,

        /// Optional mutant selector: 1-based index, or substring of mutant id.
        /// Omit to print a summary list of survivors.
        target: Option<String>,

        /// Show all outcomes, not just survivors, when target is omitted.
        #[arg(long)]
        all: bool,
    },

    /// Rank surviving mutants by which one to fix next.
    ///
    /// Reads the same JSON report as `show`/`explain`, groups survivors into
    /// `(file, operator)` clusters, and ranks them by expected reward per
    /// test: cluster size first (one test often kills the whole pattern),
    /// then kill-ease, with an estimated score gain per cluster. Built for
    /// an agent picking its next target, so JSON is the default format.
    Next {
        /// Path to a JSON report produced by `fermut run --json …`.
        report: PathBuf,

        /// Number of ranked survivors to emit. Defaults to 1 — the single
        /// best next target.
        #[arg(long, default_value_t = 1, value_name = "N")]
        limit: usize,

        /// Emit every ranked survivor instead of just `--limit`.
        #[arg(long, conflicts_with = "limit")]
        all: bool,

        /// Cap the output at this many estimated tokens: emit the
        /// highest-ranked survivors that fit, drop the rest (count logged to
        /// stderr). Overrides `--limit`. The single top target is always
        /// included. For agents budgeting context window.
        #[arg(long, value_name = "N", conflicts_with_all = ["limit", "all"])]
        max_tokens: Option<usize>,

        /// Output format. `json` (default) emits the ranked list for machine
        /// consumers; `human` prints a readable summary.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// Explain why one mutant likely survived and propose a killing test.
    ///
    /// Reads the same JSON report as `show`, then layers heuristic signal:
    /// surrounding source context, enclosing `def`/`class`, an
    /// operator-specific hint, optional coverage and test-grep lookups, and
    /// a pytest skeleton tailored to the mutation. With `--llm`, also calls
    /// Anthropic Messages API for a richer prose explanation + a generated
    /// killing test.
    Explain {
        /// Path to a JSON report produced by `fermut run --json …`.
        report: PathBuf,

        /// Mutant selector: 1-based index into the report, or substring of
        /// mutant id (matches `fermut show`'s selector grammar).
        target: String,

        /// Number of source lines to show on each side of the mutant line.
        #[arg(long, default_value_t = 5)]
        context: usize,

        /// Tests directory to grep for the enclosing symbol. Defaults to
        /// `tests/` if it exists under cwd.
        #[arg(long, num_args = 0..=1, default_missing_value = "tests")]
        tests: Option<PathBuf>,

        /// Path to a `coverage.json` (with contexts). When present, the
        /// explanation includes whether any test executed the mutant line
        /// and which tests they were.
        #[arg(long, num_args = 0..=1, default_missing_value = "coverage.json")]
        coverage: Option<PathBuf>,

        /// Augment the heuristic explanation with an Anthropic-generated
        /// prose explanation + killing test. Requires `ANTHROPIC_API_KEY`
        /// (or `FERMUT_LLM_MOCK=1` for offline runs).
        #[arg(long)]
        llm: bool,

        /// Anthropic model id. Defaults to `claude-sonnet-4-6`.
        #[arg(long)]
        model: Option<String>,

        /// Disable the LLM response cache.
        #[arg(long)]
        no_cache: bool,

        /// Custom path for the LLM response cache. Defaults to
        /// `.fermut/llm-cache.json` under the project root.
        #[arg(long)]
        cache_path: Option<PathBuf>,

        /// Output format. `human` (default) prints the laid-out terminal
        /// view. `json` emits the structured `ExplainReport` for agent
        /// consumers.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,
    },

    /// Generate a killing test for survivors, then verify it before keeping.
    ///
    /// Like `suggest`, but closes the loop: for each survivor it generates a
    /// test, appends it to the discovered test file, and checks that the
    /// suite stays green *and* the mutant now dies. Tests that fail either
    /// check are reverted, so only proven, ready-to-commit tests land. JSON
    /// is the default format for agent consumers. Requires `ANTHROPIC_API_KEY`
    /// (or `FERMUT_LLM_MOCK=1`) plus a working test runner.
    Autofix {
        /// Path to a JSON report produced by `fermut run --json …`.
        report: PathBuf,

        /// Mutant selector: 1-based index, or substring of mutant id.
        /// Omit when `--all-survivors` is set.
        target: Option<String>,

        /// Fix every surviving (or timed-out) mutant in the report.
        #[arg(long)]
        all_survivors: bool,

        /// Python source root (for the runner + baseline). Defaults to the
        /// configured source_root or `.`.
        #[arg(long, default_value = ".")]
        path: PathBuf,

        /// Tests directory: mined for style samples + the apply target, and
        /// mirrored by the verifier. Defaults to the configured tests dir.
        #[arg(long)]
        tests: Option<PathBuf>,

        /// Python interpreter (path) or virtualenv (dir) the verifier runs
        /// pytest with (`<python> -m pytest`). Same discovery as `fermut run`.
        #[arg(long, value_name = "PATH")]
        python: Option<PathBuf>,

        /// Force generated tests into this file instead of the inferred one.
        /// Must live inside the tests tree or the verifier won't see it.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Anthropic model id. Defaults to `claude-sonnet-4-6`.
        #[arg(long)]
        model: Option<String>,

        /// Source lines of context around the mutant in the prompt.
        #[arg(long, default_value_t = 8)]
        context: usize,

        /// How many existing tests to include in the prompt for style.
        #[arg(long, default_value_t = 2)]
        sample_count: usize,

        /// Per-mutant verification timeout, in seconds.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,

        /// Disable the LLM response cache.
        #[arg(long)]
        no_cache: bool,

        /// Custom path for the LLM response cache.
        #[arg(long)]
        cache_path: Option<PathBuf>,

        /// Keep generated tests even when verification fails (default reverts
        /// them). Useful for inspecting why a suggestion didn't work.
        #[arg(long)]
        keep_failed: bool,

        /// Output format. `json` (default) emits the structured report;
        /// `human` prints a per-mutant summary.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// Generate a killing pytest test for surviving mutants via Anthropic.
    ///
    /// Reads the same JSON report as `show`/`explain`, builds a prompt from
    /// the mutation, surrounding source, the enclosing `def`/`class`, and a
    /// few existing tests sampled from `--tests` for style mimicry, then
    /// prints the generated pytest function. With `--out` or `--apply` the
    /// test is appended to a file instead.
    Suggest {
        /// Path to a JSON report produced by `fermut run --json …`.
        report: PathBuf,

        /// Mutant selector: 1-based index, or substring of mutant id.
        /// Omit when `--all-survivors` is set.
        target: Option<String>,

        /// Generate a test for every surviving (or timed-out) mutant in
        /// the report.
        #[arg(long)]
        all_survivors: bool,

        /// Append the generated test to the test file that already
        /// references the enclosing symbol. Falls back to `--out` if no
        /// candidate test file is found.
        #[arg(long)]
        apply: bool,

        /// Append the generated test to this path instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Anthropic model id. Defaults to `claude-sonnet-4-6`.
        #[arg(long)]
        model: Option<String>,

        /// Tests directory to mine for style samples. Defaults to `tests/`.
        #[arg(long, num_args = 0..=1, default_missing_value = "tests")]
        tests: Option<PathBuf>,

        /// Source lines of context to include in the prompt on each side
        /// of the mutant line.
        #[arg(long, default_value_t = 8)]
        context: usize,

        /// How many existing tests to include in the prompt for style
        /// reference.
        #[arg(long, default_value_t = 2)]
        sample_count: usize,

        /// Disable the LLM response cache.
        #[arg(long)]
        no_cache: bool,

        /// Custom path for the LLM response cache. Defaults to
        /// `.fermut/llm-cache.json` under the project root.
        #[arg(long)]
        cache_path: Option<PathBuf>,

        /// Output format. `human` (default) prints generated code blocks
        /// and progress logs. `json` emits the structured `SuggestReport`
        /// for agent consumers.
        #[arg(long, value_enum, default_value_t = Format::Human)]
        format: Format,

        /// Run up to N Anthropic calls concurrently. Default `1`. Raise to
        /// shorten wall-clock when `--all-survivors` is large; keep within
        /// your tenant's rate limit. File writes stay serial.
        #[arg(long, default_value_t = 1)]
        parallel: usize,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Format {
    Human,
    Json,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum TrendFormatCli {
    Human,
    Json,
}

impl From<TrendFormatCli> for TrendFormat {
    fn from(f: TrendFormatCli) -> Self {
        match f {
            TrendFormatCli::Human => Self::Human,
            TrendFormatCli::Json => Self::Json,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum TrendScaleCli {
    Fixed,
    Auto,
}

impl From<TrendScaleCli> for TrendScale {
    fn from(s: TrendScaleCli) -> Self {
        match s {
            TrendScaleCli::Fixed => Self::Fixed,
            TrendScaleCli::Auto => Self::Auto,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum TrendGroupByCli {
    File,
}

impl From<TrendGroupByCli> for TrendGroupBy {
    fn from(g: TrendGroupByCli) -> Self {
        match g {
            TrendGroupByCli::File => Self::File,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum RunnerCli {
    Pytest,
    Rstest,
    Unittest,
}

impl From<RunnerCli> for crate::config::RunnerKind {
    fn from(r: RunnerCli) -> Self {
        match r {
            RunnerCli::Pytest => Self::Pytest,
            RunnerCli::Rstest => Self::Rstest,
            RunnerCli::Unittest => Self::Unittest,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum MigrateSourceCli {
    Mutmut,
    #[value(alias = "cosmic_ray")]
    CosmicRay,
}

impl From<MigrateSourceCli> for MigrateSource {
    fn from(s: MigrateSourceCli) -> Self {
        match s {
            MigrateSourceCli::Mutmut => Self::Mutmut,
            MigrateSourceCli::CosmicRay => Self::CosmicRay,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum IsolationCli {
    Auto,
    Copy,
    Hardlink,
    Reflink,
}

impl From<IsolationCli> for crate::config::IsolationMode {
    fn from(m: IsolationCli) -> Self {
        match m {
            IsolationCli::Auto => Self::Auto,
            IsolationCli::Copy => Self::Copy,
            IsolationCli::Hardlink => Self::Hardlink,
            IsolationCli::Reflink => Self::Reflink,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum CacheScopeCli {
    File,
    Scope,
}

impl From<CacheScopeCli> for crate::config::CacheScope {
    fn from(c: CacheScopeCli) -> Self {
        match c {
            CacheScopeCli::File => Self::File,
            CacheScopeCli::Scope => Self::Scope,
        }
    }
}

impl From<Format> for ReportFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => ReportFormat::Human,
            Format::Json => ReportFormat::Json,
        }
    }
}

impl From<Format> for ExplainFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => ExplainFormat::Human,
            Format::Json => ExplainFormat::Json,
        }
    }
}

impl From<Format> for SuggestFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => SuggestFormat::Human,
            Format::Json => SuggestFormat::Json,
        }
    }
}

impl From<Format> for ScoreFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => ScoreFormat::Human,
            Format::Json => ScoreFormat::Json,
        }
    }
}

impl From<Format> for NextFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => NextFormat::Human,
            Format::Json => NextFormat::Json,
        }
    }
}

impl From<Format> for BaselineFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => BaselineFormat::Human,
            Format::Json => BaselineFormat::Json,
        }
    }
}

impl From<Format> for AutofixFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Human => AutofixFormat::Human,
            Format::Json => AutofixFormat::Json,
        }
    }
}

fn print_profile_catalogue() {
    println!("Available profiles (pass with --profile <name>):\n");
    let name_width = subcmd::init::Profile::ALL
        .iter()
        .map(|p| p.name().len())
        .max()
        .unwrap_or(0);
    for p in subcmd::init::Profile::ALL {
        println!(
            "  {:<width$}  {}",
            p.name(),
            p.description(),
            width = name_width
        );
    }
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.cmd {
            Cmd::Run {
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
                max_time,
                filter: f,
            } => {
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
                    max_time,
                    f,
                )?;
                let want_annotations =
                    annotate || std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
                let fmt = format.into();
                // The closure receives the *prior* history (entries that
                // existed before this run started) so the markdown trend
                // block plots "this run vs every earlier run" without
                // depending on whether `engine::run`'s append to the
                // history file succeeded. Append can silently fail on
                // read-only filesystems or full disks; the caller is
                // responsible for handing us the snapshot.
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
                                // A `--max-time` run truncated by the budget
                                // scored a nondeterministic subset, so it can't
                                // anchor a regression comparison — `regression_
                                // against` excludes it. Say so, or a green gate
                                // reads as "no regression" when it's really "not
                                // evaluated".
                                eprintln!(
                                    "--fail-on-regression skipped: this run hit the --max-time \
                                     budget and scored a partial subset (not comparable). Run \
                                     without --max-time to gate on regression."
                                );
                            }
                            // Reload post-run so concurrent writers (e.g.
                            // parallel `--shard` workers) become visible,
                            // then compare against the in-memory current
                            // entry — never against `entries.last()`,
                            // which is the *previous* run when our own
                            // append silently failed.
                            let prior_now = load_prior_excluding(&cfg.history_path, current);
                            if let Some(drop) =
                                crate::history::regression_against(&prior_now, current)
                            {
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
                    // A zero-denominator run has no score. Say so, so a green
                    // gate is never mistaken for a genuine 100% (an all-errored
                    // run fails below; nothing-to-score with no errors passes as
                    // a legitimate N/A). Under `--no-fail` the exit is suppressed
                    // regardless, so the errored case reports N/A too — claiming
                    // "the gate fails" would contradict the exit-0 that follows.
                    if report.is_scoreless() {
                        eprintln!("{}", scoreless_note(&report, no_fail));
                    }
                    // `--no-fail`: the run exists to produce a report, not to
                    // gate. Conflicts with the gate flags at the parser, so
                    // `gate_failed` is always false here; the guard is explicit
                    // so the intent survives a future flag that sets it.
                    if gate_exits_nonzero(no_fail, report.should_fail(cfg.fail_under), gate_failed)
                    {
                        std::process::exit(1);
                    }
                    Ok(())
                }
            }
            Cmd::Show {
                report,
                target,
                all,
            } => show(&report, target.as_deref(), all),
            Cmd::Next {
                report,
                limit,
                all,
                max_tokens,
                format,
            } => next(NextOpts {
                report,
                limit: if all { None } else { Some(limit) },
                max_tokens,
                format: format.into(),
            }),
            Cmd::Explain {
                report,
                target,
                context,
                tests,
                coverage,
                llm,
                model,
                no_cache,
                cache_path,
                format,
            } => {
                let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                explain(ExplainOpts {
                    report,
                    target,
                    context_lines: context,
                    tests,
                    coverage,
                    llm,
                    model,
                    no_cache,
                    cache_path,
                    project_root,
                    format: format.into(),
                })
            }
            Cmd::Autofix {
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
            } => autofix(AutofixOpts {
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
                format: format.into(),
            }),
            Cmd::Suggest {
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
            } => {
                let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                suggest(SuggestOpts {
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
                    format: format.into(),
                    parallel,
                })
            }
            Cmd::Mcp => mcp_serve(),
            Cmd::Completions { shell } => {
                let mut cmd = Cli::command();
                generate(shell, &mut cmd, "fermut", &mut std::io::stdout());
                Ok(())
            }
            Cmd::Trend {
                path,
                history_path,
                limit,
                all,
                branch,
                since,
                until,
                fail_on_regression,
                scale,
                diff,
                by,
                format,
                strict,
            } => trend(TrendOpts {
                path,
                history_path,
                limit,
                all,
                branch,
                since,
                until,
                fail_on_regression,
                scale: scale.into(),
                diff,
                group_by: by.map(Into::into),
                format: format.into(),
                strict,
            }),
            Cmd::Score {
                path,
                history_path,
                baseline,
                branch,
                fail_on_regression,
                format,
            } => score(ScoreOpts {
                path,
                history_path,
                baseline,
                branch,
                fail_on_regression,
                format: format.into(),
            }),
            Cmd::Dashboard {
                path,
                history_path,
                output,
                report,
                limit,
                open,
            } => dashboard(DashboardOpts {
                path,
                history_path,
                output,
                report,
                limit,
                open,
            }),
            Cmd::Doctor { path, strict } => doctor(DoctorOpts { path, strict }),
            Cmd::Baseline {
                path,
                full,
                sample,
                top,
                format,
                filter: f,
            } => baseline(BaselineOpts {
                path,
                full,
                sample,
                top,
                format: format.into(),
                filter: f,
            }),
            Cmd::PrComment {
                markdown,
                repo,
                pr,
                marker,
                dry_run,
            } => pr_comment(PrCommentOpts {
                markdown,
                repo,
                pr,
                marker,
                dry_run,
            }),
            Cmd::Init {
                path,
                pyproject,
                force,
                with_gha,
                with_coverage,
                profile,
                list_profiles,
                dry_run,
            } => {
                if list_profiles {
                    print_profile_catalogue();
                    return Ok(());
                }
                let profile = match profile {
                    Some(name) => Some(
                        subcmd::init::Profile::parse(&name).ok_or_else(|| {
                            anyhow::anyhow!(
                                "unknown profile `{name}`. Run `fermut init --list-profiles` to see the catalogue."
                            )
                        })?,
                    ),
                    None => None,
                };
                init(InitOpts {
                    path,
                    pyproject,
                    force,
                    with_gha,
                    with_coverage,
                    profile,
                    dry_run,
                })
            }
            Cmd::Migrate {
                from,
                path,
                config,
                pyproject,
                force,
                dry_run,
                no_pragma_rewrite,
            } => migrate(MigrateOpts {
                source: from.into(),
                path,
                config,
                pyproject,
                force,
                dry_run,
                no_pragma_rewrite,
            }),
            Cmd::Coverage {
                path,
                source,
                tests,
                full,
                output,
                python,
                pytest_args,
            } => coverage(CoverageOpts {
                path,
                source,
                tests,
                full,
                output,
                python,
                pytest_args,
            }),
            Cmd::Clean { path, history_path } => {
                let resolved = resolve_history_path(&path, history_path)?;
                clean_cache(&path, &resolved)
            }
            Cmd::Merge {
                inputs,
                json,
                junit,
                html,
                markdown,
                history,
                config_hash,
                project,
            } => merge_reports(
                &inputs,
                json.as_ref(),
                junit.as_ref(),
                html.as_ref(),
                markdown.as_ref(),
                history.as_ref(),
                config_hash,
                &project,
            ),
            Cmd::List { path, filter: f } => {
                let cfg = build_config(
                    path,
                    None,
                    None,
                    None,
                    false,
                    false,
                    None,
                    Vec::new(),
                    true,
                    None,
                    true,
                    None,
                    None, // sample
                    None, // sample_seed
                    None, // shard
                    None, // runner
                    None, // python
                    None, // isolation
                    true,
                    None,
                    None,
                    true,
                    None,
                    None,
                    f,
                )?;
                let mutants = crate::mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)?;
                let chain = filter::build_chain_for_list(&cfg)?;
                let mut kept = 0usize;
                for m in &mutants {
                    if filter::first_rejector(&chain, m)?.is_none() {
                        println!("{}", m.describe());
                        kept += 1;
                    }
                }
                println!("\n{kept} mutant(s) total");
                Ok(())
            }
        }
    }
}

/// Load history from disk and drop the tail entry if it is the run we
/// just finished. When append succeeded the tail equals `current` and is
/// stripped; when it failed the tail is some earlier run and stays in
/// the prior window. Either way the returned slice never includes
/// `current`, so [`crate::history::regression_against`] sees a clean
/// "everything before this run" view.
fn load_prior_excluding(path: &std::path::Path, current: &HistoryEntry) -> Vec<HistoryEntry> {
    let mut prior = crate::history::load(path).unwrap_or_default();
    if prior.last().map(|e| same_run(e, current)).unwrap_or(false) {
        prior.pop();
    }
    prior
}

/// Whether `fermut run` should exit non-zero after producing its report.
/// `--no-fail` suppresses the exit unconditionally — the run existed to
/// produce a report, not to gate — otherwise the run fails if the mutation
/// result is below the bar (`result_fails`) or a regression gate tripped
/// (`gate_failed`). Extracted so the gate decision is unit-testable without a
/// live pytest run.
fn gate_exits_nonzero(no_fail: bool, result_fails: bool, gate_failed: bool) -> bool {
    !no_fail && (result_fails || gate_failed)
}

/// Human note for a scoreless (zero-denominator) run — its score is undefined
/// (N/A), never a vacuous 100%. Distinguishes the causes so the message is
/// actionable: all-errored (which fails the gate unless `--no-fail`), a
/// `--max-time` budget that expired before any mutant ran, or a genuinely
/// empty scope. Pure so the branch selection is unit-tested.
fn scoreless_note(report: &Report, no_fail: bool) -> String {
    let errored = report.counts().errored;
    if errored > 0 && !no_fail {
        format!(
            "no mutants scored: {errored} errored, so the score is \
             undefined (not 100%) and the gate fails"
        )
    } else if errored > 0 {
        format!(
            "no mutants scored: {errored} errored, so the score is \
             undefined (not 100%) — reported N/A, exit suppressed by --no-fail"
        )
    } else if let Some((_, n)) = report
        .skipped_by_filter()
        .into_iter()
        .find(|(f, _)| f == crate::engine::TIME_BUDGET_FILTER)
    {
        // The budget expired before any mutant was scored — distinct from an
        // empty scope; say so rather than claiming "nothing to mutate".
        format!(
            "no mutants scored: --max-time budget expired before any mutant \
             ran ({n} skipped/`time-budget`) — mutation score N/A (not 100%)"
        )
    } else {
        "no mutants scored: nothing to mutate in scope — \
         mutation score N/A (not 100%)"
            .to_string()
    }
}

/// Resolve the run-history log path the same way `build_config` does, but
/// without requiring the full runtime `Config`. Used by `fermut clean` so
/// it preserves the user's configured history file (which may live under
/// `.fermut/` with a custom name) instead of only the default
/// `history.jsonl`.
fn resolve_history_path(
    path: &std::path::Path,
    cli_history_path: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = cli_history_path {
        return Ok(p);
    }
    let loaded = crate::config::LoadedConfig::load(path)?;
    Ok(loaded
        .file
        .history_path
        .clone()
        .map(|p| loaded.resolve_path(p))
        .unwrap_or_else(|| {
            crate::history::default_history_path(&crate::history::resolve_root(path))
        }))
}

/// Two entries describe the same run when the identifying fields the
/// engine writes — timestamp, git sha, mutation score — all match.
/// Exact match because the engine constructs both sides from the same
/// `Report` and `Instant` window; serde round-tripping preserves bits.
fn same_run(a: &HistoryEntry, b: &HistoryEntry) -> bool {
    a.timestamp == b.timestamp
        && a.git_sha == b.git_sha
        && a.mutation_score.to_bits() == b.mutation_score.to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_fail_suppresses_every_gate_exit() {
        // Whatever the result/regression state, --no-fail exits 0.
        for result_fails in [false, true] {
            for gate_failed in [false, true] {
                assert!(
                    !gate_exits_nonzero(true, result_fails, gate_failed),
                    "no_fail must suppress exit (result_fails={result_fails}, gate_failed={gate_failed})"
                );
            }
        }
    }

    #[test]
    fn without_no_fail_a_failing_result_or_gate_exits_nonzero() {
        assert!(!gate_exits_nonzero(false, false, false), "clean run passes");
        assert!(
            gate_exits_nonzero(false, true, false),
            "survivors/threshold fail"
        );
        assert!(
            gate_exits_nonzero(false, false, true),
            "regression gate fails"
        );
        assert!(gate_exits_nonzero(false, true, true), "both fail");
    }

    #[test]
    fn no_fail_and_gate_flags_are_mutually_exclusive_in_the_parser() {
        use clap::Parser;
        // clap rejects --no-fail alongside a gate flag, so gate_failed can never
        // be set under --no-fail — the runtime guard is belt-and-suspenders.
        assert!(Cli::try_parse_from(["fermut", "run", "--no-fail", "--fail-under", "50"]).is_err());
        assert!(
            Cli::try_parse_from(["fermut", "run", "--no-fail", "--fail-on-regression", "5"])
                .is_err()
        );
        // --no-fail alone parses fine.
        assert!(Cli::try_parse_from(["fermut", "run", "--no-fail"]).is_ok());
    }

    fn scoreless_mutant(id: &str) -> crate::mutator::Mutant {
        use crate::mutator::{Mutant, Operator};
        use ruff_text_size::TextRange;
        Mutant {
            id: id.into(),
            file: PathBuf::from("a.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn scoreless_note_flags_time_budget_exhaustion() {
        use crate::report::MutantOutcome;
        // Budget expired before any mutant ran: every mutant is a time-budget
        // skip, no errors. The note must name the budget, not "nothing to
        // mutate".
        let report = Report::new(vec![
            MutantOutcome::skipped(scoreless_mutant("a"), crate::engine::TIME_BUDGET_FILTER),
            MutantOutcome::skipped(scoreless_mutant("b"), crate::engine::TIME_BUDGET_FILTER),
        ]);
        let note = scoreless_note(&report, false);
        assert!(note.contains("--max-time budget expired"), "got: {note}");
        assert!(note.contains("2 skipped"), "reports the count: {note}");
        assert!(!note.contains("nothing to mutate"));
    }

    #[test]
    fn scoreless_note_empty_scope_vs_errored() {
        use crate::report::MutantOutcome;
        // No mutants at all → empty scope wording.
        let empty = Report::new(vec![]);
        assert!(scoreless_note(&empty, false).contains("nothing to mutate in scope"));

        // All errored, gate live → the gate-fails wording.
        let errored = Report::new(vec![MutantOutcome::error(
            scoreless_mutant("e"),
            "boom".into(),
        )]);
        assert!(scoreless_note(&errored, false).contains("and the gate fails"));
        // Same run under --no-fail → exit-suppressed wording, never "gate fails".
        let note = scoreless_note(&errored, true);
        assert!(note.contains("exit suppressed by --no-fail"));
        assert!(!note.contains("and the gate fails"));
    }
}
