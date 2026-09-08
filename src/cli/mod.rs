//! Command-line interface: clap parser + dispatch.
//!
//! - `build_config` — merges CLI args with the loaded config file into a
//!   runtime [`Config`](crate::config::Config). Pure function, easy to test.
//! - `subcmd` — one handler per `Cmd` variant. `Cli::run` is a thin dispatch
//!   match: each arm destructures its variant and delegates to the matching
//!   `subcmd::<name>` handler (e.g. `subcmd::run::run`, `subcmd::list::run`).

pub(crate) mod build_config;
mod merge;
mod subcmd;

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::generate;
use tracing_subscriber::EnvFilter;

use crate::report::{ReportFormat, ReportSinks};

use merge::merge_reports;
use subcmd::{
    autofix::{autofix, AutofixFormat, AutofixOpts},
    baseline::{baseline, BaselineFormat, BaselineOpts},
    clean::clean_cache,
    coverage::{coverage, CoverageOpts},
    dashboard::{dashboard, DashboardOpts},
    doctor::{doctor, DoctorOpts},
    explain::{explain, ExplainFormat, ExplainOpts},
    init::{init, InitOpts},
    list::ListOpts,
    mcp::serve as mcp_serve,
    migrate::{migrate, MigrateOpts, MigrateSource},
    next::{next, NextFormat, NextOpts},
    pr_comment::{pr_comment, PrCommentOpts},
    run::RunOpts,
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
#[derive(Args, Debug, Clone, Default)]
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

/// Config-shaping args for `run` — everything that feeds [`build_config`] into
/// a runtime `Config`. Grouped into a `#[command(flatten)]` struct so the `Run`
/// variant and `build_config` pass one value instead of ~26 positional args.
/// The report/gate flags (annotate, format, json, gate thresholds, …) stay on
/// the variant because the `run` arm consumes them directly.
#[derive(Args, Debug, Default)]
pub(crate) struct RunConfigArgs {
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

    /// Enable the TCE (bytecode-equivalence) pre-filter. Drops mutants that
    /// `compile()` to a byte-identical code object — provably equivalent, so
    /// never worth a test run. Requires `python3` (or `FERMUT_PYTHON`).
    #[arg(long)]
    pub tce: bool,

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
    #[arg(long, value_name = "I/N", value_parser = merge::parse_shard_spec)]
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

    /// Fail the run only when the mutation score is below this percentage
    /// (0.0–100.0). A score exactly equal to the threshold passes.
    /// Without it, any survivor exits 1.
    #[arg(long, value_name = "SCORE")]
    pub fail_under: Option<f64>,

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

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Generate mutants, run pytest against each, report killed/survived.
    Run(subcmd::run::RunArgs),

    /// Enumerate mutations without running tests.
    ///
    /// By default the same pre-test filters as `run` apply (ty type-check,
    /// coverage, diff scope), so the list reflects what `run` would actually
    /// test. Pass `--no-ty-filter` to see the raw generated catalogue before
    /// the ty pre-filter drops type-invalid candidates.
    List(subcmd::list::ListArgs),

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
    Completions(subcmd::completions::CompletionsArgs),

    /// Detect project layout and emit a starter `fermut.toml`.
    ///
    /// Walks up from PATH to find the project root (pyproject.toml or
    /// setup.cfg), detects the source/tests directories and which tools
    /// are on PATH (pytest, unittest, ty), and writes a config tuned to
    /// the repo size. Optionally drops a PR-gate GitHub Actions workflow.
    Init(subcmd::init::InitArgs),

    /// Visualize the mutation-score history (`.fermut/history.jsonl`).
    ///
    /// Each `fermut run` appends one entry; `trend` prints a table of the
    /// most recent ones together with an ASCII sparkline and per-run
    /// deltas. Disable history capture with `--no-history` on `run`.
    Trend(subcmd::trend::TrendArgs),

    /// Emit the agent reward signal for the latest run: score, delta vs a
    /// baseline run, and the new-survivor / newly-killed mutant-id sets.
    ///
    /// Reads `.fermut/history.jsonl` and compares the most recent entry
    /// against an earlier branch-comparable one. Built for an agent's
    /// post-iteration check — "did this iteration help?" — so JSON is the
    /// default format. The `regressed` flag is true when the score dropped
    /// or a previously-killed mutant came back.
    Score(subcmd::score::ScoreArgs),

    /// Generate a self-contained HTML dashboard combining the trend log
    /// with the latest run's survivor drill-down. The page is a single
    /// file with inline CSS and SVG — no JavaScript, no external assets.
    /// Pair with `--report <run.json>` (from `fermut run --json`) to
    /// embed per-survivor source diffs.
    Dashboard(subcmd::dashboard::DashboardArgs),

    /// Diagnose environment + config: report which tools are present,
    /// whether the config is well-formed, and whether common gotchas
    /// (missing coverage contexts, unpinned Hypothesis seed, etc.) apply.
    /// Exits non-zero when any check fails (or any check warns with
    /// `--strict`).
    Doctor(subcmd::doctor::DoctorArgs),

    /// Day-one baseline: line coverage vs mutation score, and the gap
    /// between them. Sanity-checks the environment, builds coverage, runs a
    /// fast sampled mutation pass over covered code, and prints a graded
    /// verdict plus the worst files. Run this first; `next` to act on it.
    Baseline(subcmd::baseline::BaselineArgs),

    /// Wipe the `.fermut/` cache directory under PATH.
    Clean(subcmd::clean::CleanArgs),

    /// Generate or refresh the `.coverage` database used for per-mutant test
    /// selection — the one command to run after you touch your tests.
    ///
    /// With no `.coverage` yet, runs the full suite under coverage. With one
    /// present, runs only the test files changed since it was written and
    /// appends them in (purging their stale contexts first), so adding a test
    /// costs that file's runtime, not the whole suite. fermut reads the
    /// `.coverage` SQLite directly — no `coverage json` export needed.
    Coverage(subcmd::coverage::CoverageArgs),

    /// Merge multiple JSON reports (typically from shard runs) into one.
    Merge(merge::MergeArgs),

    /// Post a Markdown report to a pull request as a sticky comment.
    ///
    /// Edits the previous fermut comment in place on subsequent runs so a
    /// PR doesn't accumulate one comment per CI run. Discovery is based on
    /// the `<!-- fermut:report -->` marker that `fermut --markdown` writes
    /// at the top of every report. Requires `gh` on `PATH` (preinstalled
    /// on GitHub Actions runners; otherwise <https://cli.github.com>).
    PrComment(subcmd::pr_comment::PrCommentArgs),

    /// Translate a mutmut or cosmic-ray config into a starter `[tool.fermut]`,
    /// and (for mutmut) rewrite `# pragma: no mutate` markers to `# fermut: ignore`.
    ///
    /// The translator is intentionally narrow — mutmut and cosmic-ray both
    /// have knobs without a fermut equivalent (celery, pre/post-mutation
    /// hooks, `dict_synonyms`, interceptors). Anything that can't be mapped
    /// is printed under "manual review" so nothing is silently dropped.
    Migrate(subcmd::migrate::MigrateArgs),

    /// Inspect mutants from a prior JSON report.
    Show(subcmd::show::ShowArgs),

    /// Rank surviving mutants by which one to fix next.
    ///
    /// Reads the same JSON report as `show`/`explain`, groups survivors into
    /// `(file, operator)` clusters, and ranks them by expected reward per
    /// test: cluster size first (one test often kills the whole pattern),
    /// then kill-ease, with an estimated score gain per cluster. Built for
    /// an agent picking its next target, so JSON is the default format.
    Next(subcmd::next::NextArgs),

    /// Explain why one mutant likely survived and propose a killing test.
    ///
    /// Reads the same JSON report as `show`, then layers heuristic signal:
    /// surrounding source context, enclosing `def`/`class`, an
    /// operator-specific hint, optional coverage and test-grep lookups, and
    /// a pytest skeleton tailored to the mutation. With `--llm`, also calls
    /// Anthropic Messages API for a richer prose explanation + a generated
    /// killing test.
    Explain(subcmd::explain::ExplainArgs),

    /// Generate a killing test for survivors, then verify it before keeping.
    ///
    /// Like `suggest`, but closes the loop: for each survivor it generates a
    /// test, appends it to the discovered test file, and checks that the
    /// suite stays green *and* the mutant now dies. Tests that fail either
    /// check are reverted, so only proven, ready-to-commit tests land. JSON
    /// is the default format for agent consumers. Requires `ANTHROPIC_API_KEY`
    /// (or `FERMUT_LLM_MOCK=1`) plus a working test runner.
    Autofix(subcmd::autofix::AutofixArgs),

    /// Generate a killing pytest test for surviving mutants via Anthropic.
    ///
    /// Reads the same JSON report as `show`/`explain`, builds a prompt from
    /// the mutation, surrounding source, the enclosing `def`/`class`, and a
    /// few existing tests sampled from `--tests` for style mimicry, then
    /// prints the generated pytest function. With `--out` or `--apply` the
    /// test is appended to a file instead.
    Suggest(subcmd::suggest::SuggestArgs),
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub(crate) enum Format {
    Human,
    Json,
}

/// Define a clap `ValueEnum` CLI enum plus its `From` conversion to the
/// runtime type, one arm per variant. Collapses the def+impl boilerplate that
/// every CLI-facing enum otherwise repeats. Per-variant attrs (e.g.
/// `#[value(alias = "…")]`) are forwarded.
macro_rules! cli_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident => $target:ty {
            $( $(#[$vmeta:meta])* $variant:ident => $tvariant:ident ),+ $(,)?
        }
    ) => {
        #[derive(Copy, Clone, Debug, ValueEnum)]
        $(#[$meta])*
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl From<$name> for $target {
            fn from(v: $name) -> Self {
                match v {
                    $( $name::$variant => <$target>::$tvariant ),+
                }
            }
        }
    };
}

/// Convert the shared `Format` CLI enum into each subcommand's own
/// `Human`/`Json` report-format type — every target has the same two variants.
macro_rules! impl_format_from {
    ($($target:ty),+ $(,)?) => {$(
        impl From<Format> for $target {
            fn from(f: Format) -> Self {
                match f {
                    Format::Human => <$target>::Human,
                    Format::Json => <$target>::Json,
                }
            }
        }
    )+};
}

cli_enum! {
    pub(crate) enum TrendFormatCli => TrendFormat { Human => Human, Json => Json }
}

cli_enum! {
    pub(crate) enum TrendScaleCli => TrendScale { Fixed => Fixed, Auto => Auto }
}

cli_enum! {
    pub(crate) enum TrendGroupByCli => TrendGroupBy { File => File }
}

cli_enum! {
    pub(crate) enum RunnerCli => crate::config::RunnerKind {
        Pytest => Pytest,
        Rstest => Rstest,
        Unittest => Unittest,
    }
}

cli_enum! {
    pub(crate) enum MigrateSourceCli => MigrateSource {
        Mutmut => Mutmut,
        #[value(alias = "cosmic_ray")]
        CosmicRay => CosmicRay,
    }
}

cli_enum! {
    pub(crate) enum IsolationCli => crate::config::IsolationMode {
        Auto => Auto,
        Copy => Copy,
        Hardlink => Hardlink,
        Reflink => Reflink,
    }
}

cli_enum! {
    pub(crate) enum CacheScopeCli => crate::config::CacheScope {
        File => File,
        Scope => Scope,
    }
}

impl_format_from!(
    ReportFormat,
    ExplainFormat,
    SuggestFormat,
    ScoreFormat,
    NextFormat,
    BaselineFormat,
    AutofixFormat,
);

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

/// `From<XArgs> for XOpts` for every subcommand whose CLI struct maps to its
/// options struct by a straight field copy plus trivial per-field transforms
/// (`.into()`, renames, sink nesting, `cwd` lookup). This keeps [`Cli::run`]'s
/// dispatch arms down to a single `x(args.into())` call each. Subcommands with
/// real control flow in their arm (`init`, `clean`, `merge`, `completions`,
/// `show`) are converted inline instead.
mod from_args {
    use super::*;

    impl From<subcmd::run::RunArgs> for RunOpts {
        fn from(a: subcmd::run::RunArgs) -> Self {
            let subcmd::run::RunArgs {
                path,
                cfg_args,
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
                no_fail,
            } = a;
            RunOpts {
                path,
                cfg_args,
                annotate,
                watch,
                format,
                sinks: ReportSinks {
                    json,
                    junit,
                    html,
                    markdown,
                },
                trend,
                trend_branch,
                fail_on_regression,
                no_fail,
            }
        }
    }

    impl From<subcmd::next::NextArgs> for NextOpts {
        fn from(a: subcmd::next::NextArgs) -> Self {
            let subcmd::next::NextArgs {
                report,
                limit,
                all,
                max_tokens,
                format,
            } = a;
            NextOpts {
                report,
                limit: if all { None } else { Some(limit) },
                max_tokens,
                format: format.into(),
            }
        }
    }

    impl From<subcmd::explain::ExplainArgs> for ExplainOpts {
        fn from(a: subcmd::explain::ExplainArgs) -> Self {
            let subcmd::explain::ExplainArgs {
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
            } = a;
            ExplainOpts {
                report,
                target,
                context_lines: context,
                tests,
                coverage,
                llm,
                model,
                no_cache,
                cache_path,
                project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                format: format.into(),
            }
        }
    }

    impl From<subcmd::autofix::AutofixArgs> for AutofixOpts {
        fn from(a: subcmd::autofix::AutofixArgs) -> Self {
            let subcmd::autofix::AutofixArgs {
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
                format: format.into(),
            }
        }
    }

    impl From<subcmd::suggest::SuggestArgs> for SuggestOpts {
        fn from(a: subcmd::suggest::SuggestArgs) -> Self {
            let subcmd::suggest::SuggestArgs {
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
                project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                format: format.into(),
                parallel,
            }
        }
    }

    impl From<subcmd::trend::TrendArgs> for TrendOpts {
        fn from(a: subcmd::trend::TrendArgs) -> Self {
            let subcmd::trend::TrendArgs {
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
            } = a;
            TrendOpts {
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
            }
        }
    }

    impl From<subcmd::score::ScoreArgs> for ScoreOpts {
        fn from(a: subcmd::score::ScoreArgs) -> Self {
            let subcmd::score::ScoreArgs {
                path,
                history_path,
                baseline,
                branch,
                fail_on_regression,
                format,
            } = a;
            ScoreOpts {
                path,
                history_path,
                baseline,
                branch,
                fail_on_regression,
                format: format.into(),
            }
        }
    }

    impl From<subcmd::dashboard::DashboardArgs> for DashboardOpts {
        fn from(a: subcmd::dashboard::DashboardArgs) -> Self {
            let subcmd::dashboard::DashboardArgs {
                path,
                history_path,
                output,
                report,
                limit,
                open,
            } = a;
            DashboardOpts {
                path,
                history_path,
                output,
                report,
                limit,
                open,
            }
        }
    }

    impl From<subcmd::doctor::DoctorArgs> for DoctorOpts {
        fn from(a: subcmd::doctor::DoctorArgs) -> Self {
            let subcmd::doctor::DoctorArgs { path, strict } = a;
            DoctorOpts { path, strict }
        }
    }

    impl From<subcmd::baseline::BaselineArgs> for BaselineOpts {
        fn from(a: subcmd::baseline::BaselineArgs) -> Self {
            let subcmd::baseline::BaselineArgs {
                path,
                full,
                sample,
                top,
                format,
                filter,
            } = a;
            BaselineOpts {
                path,
                full,
                sample,
                top,
                format: format.into(),
                filter,
            }
        }
    }

    impl From<subcmd::pr_comment::PrCommentArgs> for PrCommentOpts {
        fn from(a: subcmd::pr_comment::PrCommentArgs) -> Self {
            let subcmd::pr_comment::PrCommentArgs {
                markdown,
                repo,
                pr,
                marker,
                dry_run,
            } = a;
            PrCommentOpts {
                markdown,
                repo,
                pr,
                marker,
                dry_run,
            }
        }
    }

    impl From<subcmd::migrate::MigrateArgs> for MigrateOpts {
        fn from(a: subcmd::migrate::MigrateArgs) -> Self {
            let subcmd::migrate::MigrateArgs {
                from,
                path,
                config,
                pyproject,
                force,
                dry_run,
                no_pragma_rewrite,
            } = a;
            MigrateOpts {
                source: from.into(),
                path,
                config,
                pyproject,
                force,
                dry_run,
                no_pragma_rewrite,
            }
        }
    }

    impl From<subcmd::coverage::CoverageArgs> for CoverageOpts {
        fn from(a: subcmd::coverage::CoverageArgs) -> Self {
            let subcmd::coverage::CoverageArgs {
                path,
                source,
                tests,
                full,
                output,
                python,
                pytest_args,
            } = a;
            CoverageOpts {
                path,
                source,
                tests,
                full,
                output,
                python,
                pytest_args,
            }
        }
    }

    impl From<subcmd::list::ListArgs> for ListOpts {
        fn from(a: subcmd::list::ListArgs) -> Self {
            let subcmd::list::ListArgs {
                path,
                no_ty_filter,
                ruff_filter,
                tce,
                filter,
            } = a;
            ListOpts {
                path,
                no_ty_filter,
                ruff_filter,
                tce,
                filter,
            }
        }
    }
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.cmd {
            Cmd::Run(args) => subcmd::run::run(args.into()),
            Cmd::Show(args) => {
                let subcmd::show::ShowArgs {
                    report,
                    target,
                    all,
                } = args;
                show(&report, target.as_deref(), all)
            }
            Cmd::Next(args) => next(args.into()),
            Cmd::Explain(args) => explain(args.into()),
            Cmd::Autofix(args) => autofix(args.into()),
            Cmd::Suggest(args) => suggest(args.into()),
            Cmd::Mcp => mcp_serve(),
            Cmd::Completions(args) => {
                let subcmd::completions::CompletionsArgs { shell } = args;
                {
                    let mut cmd = Cli::command();
                    generate(shell, &mut cmd, "fermut", &mut std::io::stdout());
                    Ok(())
                }
            }
            Cmd::Trend(args) => trend(args.into()),
            Cmd::Score(args) => score(args.into()),
            Cmd::Dashboard(args) => dashboard(args.into()),
            Cmd::Doctor(args) => doctor(args.into()),
            Cmd::Baseline(args) => baseline(args.into()),
            Cmd::PrComment(args) => pr_comment(args.into()),
            Cmd::Init(args) => {
                let subcmd::init::InitArgs {
                    path,
                    pyproject,
                    force,
                    with_gha,
                    with_coverage,
                    profile,
                    list_profiles,
                    dry_run,
                } = args;
                {
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
            }
            Cmd::Migrate(args) => migrate(args.into()),
            Cmd::Coverage(args) => coverage(args.into()),
            Cmd::Clean(args) => {
                let subcmd::clean::CleanArgs { path, history_path } = args;
                {
                    let resolved = resolve_history_path(&path, history_path)?;
                    clean_cache(&path, &resolved)
                }
            }
            Cmd::Merge(args) => {
                let merge::MergeArgs {
                    inputs,
                    json,
                    junit,
                    html,
                    markdown,
                    history,
                    config_hash,
                    project,
                } = args;
                merge_reports(
                    &inputs,
                    &ReportSinks {
                        json,
                        junit,
                        html,
                        markdown,
                    },
                    history.as_ref(),
                    config_hash,
                    &project,
                )
            }
            Cmd::List(args) => subcmd::list::run(args.into()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn smart_order_and_no_smart_order_are_mutually_exclusive_in_the_parser() {
        use clap::Parser;
        // The force-on and force-off knobs contradict, so clap must reject both
        // together — otherwise the resolution order in build_config would decide
        // silently.
        assert!(
            Cli::try_parse_from(["fermut", "run", ".", "--smart-order", "--no-smart-order"])
                .is_err()
        );
        // Either alone parses fine.
        assert!(Cli::try_parse_from(["fermut", "run", ".", "--smart-order"]).is_ok());
        assert!(Cli::try_parse_from(["fermut", "run", ".", "--no-smart-order"]).is_ok());
    }
}
