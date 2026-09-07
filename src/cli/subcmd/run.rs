//! `fermut run` argument struct.
//!
//! The config-shaping flags live in the shared [`crate::cli::RunConfigArgs`]
//! (flattened here and handed to `build_config`); the report/gate flags stay
//! on this struct because the `run` arm consumes them directly.

#[derive(clap::Args, Debug)]
pub(crate) struct RunArgs {
    /// Python source root (package or module path).
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    #[command(flatten)]
    pub(crate) cfg_args: crate::cli::RunConfigArgs,

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
}
