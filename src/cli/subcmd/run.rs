//! `fermut run` argument struct (flattened into `Cmd::Run`).

#[derive(clap::Args, Debug)]
pub(crate) struct RunArgs {
    /// Python source root (package or module path).
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    /// Test directory passed to pytest. Defaults to `<path>/tests`.
    #[arg(long)]
    pub(crate) tests: Option<std::path::PathBuf>,

    /// Parallel worker count. Defaults to logical CPU count.
    #[arg(long)]
    pub(crate) jobs: Option<usize>,

    /// Per-mutant pytest timeout, in seconds.
    #[arg(long)]
    pub(crate) timeout: Option<u64>,

    /// Skip the ty pre-filter stage.
    #[arg(long)]
    pub(crate) no_ty_filter: bool,

    /// Enable the ruff lint pre-filter. Requires `ruff` on PATH.
    #[arg(long)]
    pub(crate) ruff_filter: bool,

    /// Enable the TCE (bytecode-equivalence) pre-filter. Drops mutants that
    /// `compile()` to a byte-identical code object — provably equivalent, so
    /// never worth a test run. Requires `python3` (or `FERMUT_PYTHON`).
    #[arg(long)]
    pub(crate) tce: bool,

    /// Pin the Hypothesis seed across every mutant run for determinism.
    /// Passes `--hypothesis-seed=<N>` to pytest. Without this, Hypothesis
    /// tests can mask or fabricate survivors via random example draws.
    #[arg(long)]
    pub(crate) hypothesis_seed: Option<u64>,

    /// Extra arguments forwarded to pytest. Repeatable.
    /// Example: `--pytest-arg "-k" --pytest-arg "myfilter"`.
    #[arg(long = "pytest-arg", value_name = "ARG")]
    pub(crate) pytest_args: Vec<String>,

    /// Disable the result cache (`.fermut/cache.json`).
    #[arg(long)]
    pub(crate) no_cache: bool,

    /// Custom path for the result cache file.
    #[arg(long)]
    pub(crate) cache_path: Option<std::path::PathBuf>,

    /// Disable the run-history log (`.fermut/history.jsonl`).
    /// History is what `fermut trend` reads.
    #[arg(long)]
    pub(crate) no_history: bool,

    /// Custom path for the run-history log.
    #[arg(long)]
    pub(crate) history_path: Option<std::path::PathBuf>,

    /// Test only this fraction of mutants (0.0–1.0). Deterministic for a
    /// fixed `--sample-seed`. Useful for fast feedback on huge repos.
    #[arg(long, value_name = "RATIO")]
    pub(crate) sample: Option<f64>,

    /// Seed for `--sample` selection. Defaults to 0.
    #[arg(long, value_name = "N")]
    pub(crate) sample_seed: Option<u64>,

    /// Distributed execution: process only the i-th of n disjoint slices.
    /// Format `i/n`, both 1-based. Run all n in parallel (CI matrix,
    /// separate hosts), then `fermut merge` the JSON reports.
    #[arg(long, value_name = "I/N", value_parser = crate::cli::merge::parse_shard_spec)]
    pub(crate) shard: Option<(u32, u32)>,

    /// Test runner. `pytest` (default), `rstest` (pytest-compatible
    /// drop-in), or `unittest`.
    #[arg(long, value_enum)]
    pub(crate) runner: Option<crate::cli::RunnerCli>,

    /// Python interpreter (path) or virtualenv (dir) to run pytest with.
    /// fermut invokes `<python> -m pytest`, so it uses that interpreter's
    /// pytest with no reliance on PATH — useful in restricted sandboxes/CI
    /// that won't let you activate a venv. When omitted, fermut
    /// auto-discovers an active venv or a nearby `.venv`, else falls back
    /// to a bare `pytest` on PATH.
    #[arg(long, value_name = "PATH")]
    pub(crate) python: Option<std::path::PathBuf>,

    /// Per-worker mirror isolation scheme.
    /// `auto` (default) picks reflink/clonefile when supported else copy.
    /// `hardlink` is fastest but shares inodes — unsafe if tests write
    /// back into the source tree. `copy` is the original behavior.
    #[arg(long, value_enum)]
    pub(crate) isolation: Option<crate::cli::IsolationCli>,

    /// Disable the equivalent-mutant detector. By default, survivors are
    /// post-processed by an AST-pattern + CPython-bytecode check; mutants
    /// proven equivalent are excluded from the score.
    #[arg(long)]
    pub(crate) no_equiv_detect: bool,

    /// Cache-key granularity for source identity.
    /// `file` (default) keys cache entries on the AST hash of the whole
    /// file — any structural edit invalidates every mutant in the file.
    /// `scope` keys on the file prelude + the enclosing top-level
    /// def/class body, so edits inside one function leave cache hits
    /// intact for mutants in sibling functions. `scope` is opt-in
    /// because it can return stale verdicts when a test for one
    /// function indirectly calls another.
    #[arg(long, value_enum)]
    pub(crate) cache_scope: Option<crate::cli::CacheScopeCli>,

    /// Emit GitHub Actions workflow-command annotations to stdout for
    /// each survivor / timeout / error. Auto-on when `GITHUB_ACTIONS=true`.
    #[arg(long)]
    pub(crate) annotate: bool,

    /// Watch the source root and re-run on every `.py` change. Loops
    /// until interrupted with Ctrl+C.
    #[arg(long)]
    pub(crate) watch: bool,

    /// stdout output format.
    #[arg(long, value_enum, default_value_t = crate::cli::Format::Human)]
    pub(crate) format: crate::cli::Format,

    /// Also write JSON report to this path (consumed by `fermut show`).
    #[arg(long)]
    pub(crate) json: Option<std::path::PathBuf>,

    /// Also write JUnit XML report to this path.
    #[arg(long)]
    pub(crate) junit: Option<std::path::PathBuf>,

    /// Also write HTML report to this path.
    #[arg(long)]
    pub(crate) html: Option<std::path::PathBuf>,

    /// Also write a Markdown report (suitable for PR comments) to this path.
    #[arg(long)]
    pub(crate) markdown: Option<std::path::PathBuf>,

    /// Include a compact trend block (sparkline + delta vs previous
    /// run) at the top of the Markdown report. Reads prior entries
    /// from the history log; no-op when there's no prior history or
    /// `--markdown` isn't set.
    #[arg(long)]
    pub(crate) trend: bool,

    /// Restrict the markdown trend block's "previous run" lookup to
    /// entries recorded on this branch. Useful in CI where the cache
    /// restores main-branch history into a PR build — pin to `main`
    /// so the trend compares against main, not against another PR.
    #[arg(long, value_name = "NAME", requires = "trend")]
    pub(crate) trend_branch: Option<String>,

    /// Exit non-zero if the mutation score dropped more than this
    /// many points vs the most recent prior run on the same git
    /// branch. Requires history (will warn + exit non-zero when
    /// `--no-history` is in effect). Ignored in `--watch` mode.
    #[arg(long, value_name = "PTS")]
    pub(crate) fail_on_regression: Option<f64>,

    /// Fail the run only when the mutation score is below this percentage
    /// (0.0–100.0). A score exactly equal to the threshold passes.
    /// Without it, any survivor exits 1.
    #[arg(long, value_name = "SCORE")]
    pub(crate) fail_under: Option<f64>,

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
    pub(crate) no_fail: bool,

    /// Skip the pre-flight check that the unmutated suite passes. fermut
    /// runs your full test suite once before mutating; a red or erroring
    /// suite makes every covered mutant look killed and inflates the
    /// score. Pass this only when you've already confirmed the suite is
    /// green (e.g. CI ran it in a prior step).
    #[arg(long)]
    pub(crate) no_verify_baseline: bool,

    /// Wall-clock cap (seconds) for the baseline run. The baseline runs
    /// the whole suite once, so this is separate from `--timeout` (which
    /// bounds a single mutant). Default 300. Raise it for large suites; a
    /// suite that exceeds it is killed and the run aborts.
    #[arg(long, value_name = "SECS")]
    pub(crate) baseline_timeout: Option<u64>,

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
    pub(crate) no_smart_order: bool,

    /// Force smart test ordering on (over `smart_order = false` in config).
    /// Conflicts with `--no-smart-order`.
    #[arg(long, conflicts_with = "no_smart_order")]
    pub(crate) smart_order: bool,

    /// Wall-clock ceiling (seconds) on the per-mutant testing phase. When
    /// set, mutants are evaluated highest-value first (covered mutants
    /// before uncovered) and, once the deadline passes, every mutant not
    /// yet started is recorded as `skipped` (filter `time-budget`) instead
    /// of run; mutants already in flight finish. Gives a PR gate a
    /// predictable ceiling — a time cap beats a mutant cap for CI trust.
    /// Bounds only the testing phase: baseline verification, generation,
    /// and the ty pre-filter are separate fixed costs it does not cover.
    #[arg(long, value_name = "SECS")]
    pub(crate) max_time: Option<u64>,

    #[command(flatten)]
    pub(crate) filter: crate::cli::FilterArgs,
}
