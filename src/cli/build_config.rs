//! Merge CLI arguments with the loaded config file into a runtime `Config`.
//!
//! Precedence: **CLI explicit value > config-file value > built-in default**.
//! Each list (`ops`, `skip_ops`) merges independently — a CLI `--ops`
//! overrides the file's `ops` but leaves the file's `skip_ops` intact, and
//! vice versa.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::RunConfigArgs;
use crate::config::{CacheScope, Config, ConfigSource, IsolationMode, LoadedConfig, RunnerKind};
use crate::filter;
use crate::mutator::Operator;

use super::merge::parse_shard_spec;

pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default wall-clock cap for the baseline (full-suite) run. Generous because
/// the whole suite runs once; raise with `--baseline-timeout` for big suites.
pub(super) const DEFAULT_BASELINE_TIMEOUT_SECS: u64 = 300;

/// Resolve smart test ordering from the CLI force-off/force-on flags and the
/// config value. Pure so the precedence is unit-tested.
///
/// Precedence: `--no-smart-order` wins; then `--smart-order` /
/// `smart_order = true` force it on; otherwise it defaults on.
///
/// A per-mutant timeout does **not** disable ordering. Ordering only permutes
/// the coverage-selected set, and under pytest `-x` that can convert a
/// `killed`↔`timed_out` outcome — but *never* produce or remove a `survived`
/// one (a failing test still fails whenever it runs; a timeout merely relabels
/// the kill). fermut's score counts `timed_out` as detected, identical to
/// `killed` (see [`crate::report::Counts`]), so the mutation score and the
/// `--fail-on-regression` gate (score + survivor ids) are order-invariant with
/// or without a timeout. Killer-first ordering only changes speed — and helps
/// *most* under a timeout, by reaching the kill before the deadline instead of
/// burning it on slow non-killer tests.
fn resolve_smart_order(cli_no: bool, cli_yes: bool, file: Option<bool>) -> bool {
    if cli_no {
        false
    } else if cli_yes {
        true
    } else {
        file.unwrap_or(true)
    }
}

/// Merge the parsed `run` CLI args (`cli_path` positional + the flattened
/// [`RunConfigArgs`]) with the loaded config file into a runtime `Config`.
pub(crate) fn build_config(cli_path: PathBuf, args: RunConfigArgs) -> Result<Config> {
    let RunConfigArgs {
        tests: cli_tests,
        jobs: cli_jobs,
        timeout: cli_timeout,
        no_ty_filter: cli_no_ty_filter,
        ruff_filter: cli_ruff_filter,
        tce: cli_tce,
        hypothesis_seed: cli_hypothesis_seed,
        pytest_args: cli_pytest_args,
        no_cache: cli_no_cache,
        cache_path: cli_cache_path,
        cache_audit_rate: cli_cache_audit_rate,
        no_cache_audit: cli_no_cache_audit,
        no_history: cli_no_history,
        history_path: cli_history_path,
        sample: cli_sample,
        sample_seed: cli_sample_seed,
        shard: cli_shard,
        runner: cli_runner,
        python: cli_python,
        isolation: cli_isolation,
        no_equiv_detect: cli_no_equiv_detect,
        cache_scope: cli_cache_scope,
        fail_under: cli_fail_under,
        no_verify_baseline: cli_no_verify_baseline,
        baseline_timeout: cli_baseline_timeout,
        no_smart_order: cli_no_smart_order,
        smart_order: cli_smart_order,
        no_pytest_plugin: cli_no_pytest_plugin,
        schemata: cli_schemata,
        max_time: cli_max_time,
        timeout_factor: cli_timeout_factor,
        baseline_secs: cli_baseline_secs,
        record_kill_sets: cli_record_kill_sets,
        only_dominators,
        audit_inferred,
        filter: f,
    } = args;
    let cli_runner: Option<RunnerKind> = cli_runner.map(Into::into);
    if cli_record_kill_sets.is_some() {
        warn!("`--record-kill-sets` is experimental: its output format may change");
    }
    let cli_isolation: Option<IsolationMode> = cli_isolation.map(Into::into);
    let cli_cache_scope: Option<CacheScope> = cli_cache_scope.map(Into::into);

    let loaded = LoadedConfig::load(&cli_path)?;
    let file = &loaded.file;

    if loaded.source != ConfigSource::None {
        info!(
            base = %loaded.base_dir.display(),
            source = ?loaded.source,
            "loaded fermut config"
        );
    }

    let source_root = if cli_path == Path::new(".") {
        // No explicit path on CLI — prefer config's source_root.
        file.source_root
            .clone()
            .map(|p| loaded.resolve_path(p))
            .unwrap_or(cli_path)
    } else {
        cli_path
    };
    // Mirror::build → find_project_root walks parents looking for
    // pyproject.toml / setup.cfg. With a relative `source_root` or `tests`
    // the walk's empty PathBuf can match a sibling marker in cwd and yield
    // an empty project_root, breaking the mirror copy. Absolutize both so
    // the walk always anchors on a real directory.
    let source_root = absolutize(source_root);
    // `.fermut/` sidecars (cache + history) share one anchor — the project
    // root (`history::resolve_root`: nearest pyproject.toml/setup.cfg, else an
    // existing `.fermut/`, else the target dir; a file target resolves to its
    // parent). Co-locating them means `fermut run src/` and a bare
    // `fermut trend` agree, and the documented default `.fermut/cache.json`
    // (and the `actions/cache` CI key) actually points at where the cache
    // lands instead of `src/.fermut/`.
    let artifact_root = crate::history::resolve_root(&source_root);
    let tests = cli_tests
        .or_else(|| file.tests.clone().map(|p| loaded.resolve_path(p)))
        .map(absolutize);

    let jobs = cli_jobs.or(file.jobs);
    let timeout_secs = cli_timeout.or(file.timeout).unwrap_or(DEFAULT_TIMEOUT_SECS);

    // CLI flag presence overrides config-on; otherwise config wins.
    let ty_filter = if cli_no_ty_filter {
        false
    } else {
        file.ty_filter.unwrap_or(true)
    };
    let ruff_filter = cli_ruff_filter || file.ruff_filter.unwrap_or(false);
    let tce = cli_tce || file.tce.unwrap_or(false);
    let experimental = f.experimental || file.experimental.unwrap_or(false);
    let parity = f.parity || file.parity.unwrap_or(false);
    if matches!(f.operators, Some(crate::cli::OperatorProfileCli::Default)) {
        warn!("{}", crate::config::DEPRECATED_DEFAULT_PROFILE);
    }
    let operator_profile = f
        .operators
        .map(Into::into)
        .or(file.operators)
        .unwrap_or_default();

    let ops_allow = merge_op_list(f.ops, &file.ops)?;
    let ops_deny = merge_op_list(f.skip_ops, &file.skip_ops)?.unwrap_or_default();

    let (diff_base, since) = if f.no_diff_only {
        (None, None)
    } else {
        match (f.diff_only, f.since) {
            // A CLI scope replaces the config's (CLI > config), so
            // `--since HEAD` works on a pr-gate config's `diff_only = "main"`.
            // clap already rejects both flags together.
            (Some(base), _) => (Some(base), None),
            (None, Some(spec)) => (None, Some(spec)),
            (None, None) => {
                if file.diff_only.is_some() && file.since.is_some() {
                    return Err(anyhow::anyhow!(
                        "config sets both `diff_only` and `since`; they are mutually exclusive"
                    ));
                }
                (file.diff_only.clone(), file.since.clone())
            }
        }
    };
    // Inferred kills are for full nightly sweeps. A diff-scoped run is the PR
    // gate, which must score observed verdicts only — refuse even when the
    // scope came from the config file, where clap's conflict can't see it.
    if only_dominators && (diff_base.is_some() || since.is_some()) {
        anyhow::bail!(
            "`--only-dominators` can't be combined with `--diff-only`/`--since` \
             (from the CLI or config): a diff-scoped run must score observed verdicts"
        );
    }
    if only_dominators {
        warn!("`--only-dominators` is experimental: some kills are inferred, not run");
    }

    let project_root =
        crate::runner::find_project_root(&source_root).unwrap_or_else(|| source_root.clone());
    // Resolve the coverage file and remember whether it was *explicitly* wired
    // (CLI/config) or *auto-discovered*. The provenance decides how we react to
    // a stale file below: an explicit path is the user's choice (warn, obey),
    // an auto-discovered one is opportunistic (decline rather than silently
    // narrow test sets with outdated data).
    let explicit_coverage = f
        .coverage
        .or_else(|| file.coverage.clone().map(|p| loaded.resolve_path(p)));
    let (mut coverage_path, coverage_auto) = if f.no_coverage {
        (None, false)
    } else if let Some(p) = explicit_coverage {
        (Some(p), false)
    } else {
        // Nothing wired explicitly: fall back to coverage.py's canonical
        // `.coverage` SQLite DB at the project root if one is present. fermut
        // reads it directly, so a plain `pytest --cov-context=test` is enough —
        // no config edit and no `coverage json` export needed.
        match discover_coverage_db(&project_root) {
            Some(p) => (Some(p), true),
            None => (None, false),
        }
    };

    // Freshness guard: a `.coverage` older than the source/tests it claims to
    // describe narrows each mutant's test set from stale data — mutants on
    // newly-added or edited lines get wrongly skipped (false survivors / vacuous
    // 100%). Detect it by mtime and act on provenance.
    if let Some(p) = &coverage_path {
        if let Some(newer) =
            newer_source_than_coverage(p, &source_root, tests.as_deref(), &project_root)
        {
            if coverage_auto {
                warn!(
                    "ignoring auto-discovered coverage {} — it is older than e.g. {} \
                     (and possibly other sources/tests). Using stale coverage would \
                     wrongly skip mutants on changed lines, so this run proceeds \
                     without coverage selection. Refresh it with \
                     `pytest --cov={src} --cov-context=test` (or `fermut coverage`).",
                    p.display(),
                    newer.display(),
                    src = source_root.display(),
                );
                coverage_path = None;
            } else {
                warn!(
                    "coverage {} looks stale — it is older than e.g. {} (and possibly \
                     other sources/tests). Survivors on changed lines may be false; \
                     regenerate before trusting results: \
                     `pytest --cov={src} --cov-context=test` (or `fermut coverage`).",
                    p.display(),
                    newer.display(),
                    src = source_root.display(),
                );
            }
        }
    }

    let mut coverage = match &coverage_path {
        Some(p) => Some(crate::filter::coverage::CoverageContexts::from_path(
            p,
            &source_root,
            &project_root,
        )?),
        None => None,
    };
    // `--select-tests` narrows `coverage` to the allowlist; the full contexts
    // stay behind for the coverage filter (see `filter::coverage::select`).
    let mut coverage_unselected = None;
    if let Some(list) = &f.select_tests {
        match coverage.take() {
            Some(full) => {
                let selected = crate::filter::coverage::read_selection(list, full.node_root())?;
                info!(
                    tests = selected.len(),
                    "narrowing test selection to --select-tests"
                );
                let narrowed = full.select(&selected);
                // An empty narrowing skips every mutant as `selection` and the
                // run scores N/A — a green gate that tested nothing. Refuse it.
                if narrowed.test_ids().next().is_none() {
                    anyhow::bail!(
                        "--select-tests {}: per the coverage data (node root {}), none of \
                         its {} test(s) execute the code under mutation, so no mutant \
                         would be tested. Select tests that exercise it, or refresh \
                         coverage (`fermut coverage`). If they do exercise it and \
                         coverage is fresh, fermut mis-mapped the node ids: please \
                         report it at {}",
                        list.display(),
                        full.node_root().display(),
                        selected.len(),
                        crate::ISSUES_URL,
                    );
                }
                coverage = Some(narrowed);
                coverage_unselected = Some(full);
            }
            None => warn!(
                "--select-tests needs coverage data to map tests to lines; ignoring it, \
                 mutants run against the full suite"
            ),
        }
    }

    let hypothesis_seed = cli_hypothesis_seed.or(file.hypothesis_seed);
    let pytest_args = if !cli_pytest_args.is_empty() {
        cli_pytest_args
    } else {
        file.pytest_args.clone().unwrap_or_default()
    };

    let cache = if cli_no_cache {
        false
    } else {
        file.cache.unwrap_or(true)
    };
    let cache_path = cli_cache_path
        .or_else(|| file.cache_path.clone().map(|p| loaded.resolve_path(p)))
        .unwrap_or_else(|| crate::cache::default_cache_path(&artifact_root));

    let cache_audit_rate = if cli_no_cache_audit {
        0.0
    } else {
        match cli_cache_audit_rate {
            Some(r) => r,
            None => file
                .cache_audit_rate
                .map(check_audit_rate)
                .transpose()
                .map_err(|e| anyhow::anyhow!("cache_audit_rate: {e}"))?
                .unwrap_or(crate::config::DEFAULT_CACHE_AUDIT_RATE),
        }
    };

    let history = if cli_no_history {
        false
    } else if coverage_unselected.is_some() {
        // A score against a test subset isn't comparable with full runs;
        // recording it would corrupt the trend baseline.
        info!("not recording history: --select-tests narrows the test set");
        false
    } else {
        file.history.unwrap_or(true)
    };
    let history_path = cli_history_path
        .or_else(|| file.history_path.clone().map(|p| loaded.resolve_path(p)))
        .unwrap_or_else(|| crate::history::default_history_path(&artifact_root));

    // Smart test ordering is verdict-neutral even with a per-mutant timeout:
    // reordering only permutes the selected set, so it can flip
    // `killed`↔`timed_out` (both count as *detected* in the score) but never
    // produce or remove a `survived` one. The mutation score and the
    // `--fail-on-regression` gate are therefore order-invariant, so an explicit
    // timeout does *not* disable ordering — `--no-smart-order` /
    // `smart_order = false` remain the only off switches. The sidecar lives
    // beside cache/history.
    let smart_order = resolve_smart_order(cli_no_smart_order, cli_smart_order, file.smart_order);
    let pytest_plugin = !cli_no_pytest_plugin && file.pytest_plugin.unwrap_or(true);
    let schemata = cli_schemata || file.schemata.unwrap_or(false);
    let kill_order_path = file
        .kill_order_path
        .clone()
        .map(|p| loaded.resolve_path(p))
        .unwrap_or_else(|| crate::kill_order::default_kill_order_path(&artifact_root));
    let dominators_path = crate::subsume::default_dominators_path(&artifact_root);

    let sample_ratio = cli_sample.or(file.sample);
    let sample_seed = cli_sample_seed.or(file.sample_seed);
    let shard = match cli_shard {
        Some(s) => Some(s),
        None => file
            .shard
            .as_deref()
            .map(parse_shard_spec)
            .transpose()
            .map_err(anyhow::Error::msg)?,
    };
    let fail_under = cli_fail_under.or(file.fail_under);
    if let Some(v) = fail_under {
        if !(0.0..=100.0).contains(&v) {
            return Err(anyhow::anyhow!(
                "`--fail-under` must be between 0.0 and 100.0, got {v}"
            ));
        }
    }

    // CLI `--exclude` (when present) overrides file `exclude`. Validate now
    // so a bad glob surfaces at config-load time, not deep in the walk.
    let exclude = if !f.exclude.is_empty() {
        f.exclude
    } else {
        file.exclude.clone().unwrap_or_default()
    };
    for pat in &exclude {
        globset::Glob::new(pat).with_context(|| format!("invalid exclude glob: {pat:?}"))?;
    }
    let runner = cli_runner.or(file.runner).unwrap_or_default();
    if schemata {
        check_schemata(runner)?;
        warn!("`--schemata` is experimental; verdicts should match a normal run, please report any that don't");
    }
    // Resolve the pytest interpreter: explicit `--python` / config `python`
    // (relative values anchored at the config base dir) is authoritative;
    // otherwise auto-discover a venv near the source root. `None` keeps the
    // historical bare-`pytest`-on-PATH behavior.
    let python = {
        let explicit = cli_python.or_else(|| file.python.clone().map(|p| loaded.resolve_path(p)));
        crate::runner::resolve_python(&source_root, explicit.as_deref())
    };
    let isolation = cli_isolation
        .or_else(|| {
            std::env::var("FERMUT_ISOLATION")
                .ok()
                .and_then(parse_isolation_env)
        })
        .or(file.isolation)
        .unwrap_or_default();
    let equiv_detect = if cli_no_equiv_detect {
        false
    } else {
        file.equiv_detect.unwrap_or(true)
    };
    let cache_scope = cli_cache_scope.or(file.cache_scope).unwrap_or_default();
    // CLI flag presence forces off; otherwise config wins, defaulting on.
    let verify_baseline = if cli_no_verify_baseline {
        false
    } else {
        file.verify_baseline.unwrap_or(true)
    };
    let baseline_timeout_secs = cli_baseline_timeout
        .or(file.baseline_timeout)
        .unwrap_or(DEFAULT_BASELINE_TIMEOUT_SECS);
    let max_time_secs = cli_max_time.or(file.max_time);
    if let Some(s) = cli_baseline_secs {
        anyhow::ensure!(
            s.is_finite() && s >= 0.0,
            "--baseline-secs must be a non-negative number of seconds (got {s})"
        );
    }
    // On by default; `0` turns it off (flat `--timeout`).
    let explicit_factor = cli_timeout_factor.or(file.timeout_factor);
    let timeout_factor = match explicit_factor {
        Some(0.0) => None,
        Some(f) => Some(f),
        None => Some(crate::config::DEFAULT_TIMEOUT_FACTOR),
    };
    if let Some(f) = timeout_factor {
        anyhow::ensure!(
            f.is_finite() && f >= 2.0,
            "timeout factor must be at least 2, or 0 to turn it off (got {f}): a mutant \
             can rerun the whole suite (unittest, or pytest without coverage) beside other \
             workers, so a tighter budget times out real survivors and scores them detected"
        );
        // Only warn when asked for: the default quietly falls back to the flat timeout.
        if explicit_factor.is_some() && !verify_baseline && cli_baseline_secs.is_none() {
            warn!(
                "--timeout-factor needs the baseline run to size timeouts; with \
                 --no-verify-baseline (and no --baseline-secs or \
                 FERMUT_BASELINE_SECS) every mutant uses the flat --timeout"
            );
        }
    }

    Ok(Config {
        source_root,
        tests,
        jobs,
        timeout_secs,
        ty_filter,
        ruff_filter,
        tce,
        experimental,
        parity,
        operator_profile,
        ops_allow,
        ops_deny,
        diff_base,
        since,
        coverage_path,
        coverage,
        coverage_unselected,
        hypothesis_seed,
        pytest_args,
        cache,
        cache_path,
        cache_audit_rate,
        smart_order,
        pytest_plugin,
        schemata,
        kill_order_path,
        dominators_path,
        history,
        history_path,
        sample_ratio,
        sample_seed,
        shard,
        runner,
        python,
        unittest_pattern: file.unittest_pattern.clone(),
        isolation,
        equiv_detect,
        cache_scope,
        fail_under,
        exclude,
        verify_baseline,
        baseline_timeout_secs,
        max_time_secs,
        timeout_factor,
        baseline_secs: cli_baseline_secs,
        record_kill_sets: cli_record_kill_sets.map(absolutize),
        only_dominators,
        inferred_audit_rate: audit_inferred.unwrap_or(crate::config::DEFAULT_CACHE_AUDIT_RATE),
    })
}

/// Clap value parser for `--cache-audit-rate`: a fraction in `[0, 1]`.
/// Schemata forks a warm pytest process: pytest runner only, and `fork` only
/// exists on Unix.
fn check_schemata(runner: RunnerKind) -> Result<()> {
    if runner != RunnerKind::Pytest {
        anyhow::bail!("`--schemata` works with the pytest runner only (runner is {runner:?})");
    }
    if !cfg!(unix) {
        anyhow::bail!("`--schemata` needs fork(), which this platform doesn't have");
    }
    Ok(())
}

pub(crate) fn parse_audit_rate(s: &str) -> Result<f64, String> {
    let r: f64 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    check_audit_rate(r)
}

/// A killer-hit audit rate must be a finite fraction in `[0, 1]`.
fn check_audit_rate(r: f64) -> Result<f64, String> {
    if (0.0..=1.0).contains(&r) {
        Ok(r)
    } else {
        Err(format!("audit rate must be between 0 and 1, got {r}"))
    }
}

/// Make `p` absolute. Prefer `canonicalize` so symlinks resolve to a real
/// directory; fall back to `cwd.join(p)` for paths that don't exist yet.
fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        return p;
    }
    if let Ok(c) = std::fs::canonicalize(&p) {
        return c;
    }
    std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
}

/// Auto-discover coverage.py's canonical `.coverage` SQLite DB at the project
/// root when no coverage path was wired. Returns it only when the file exists
/// *and* sniffs as a real SQLite DB — guarding against a stray/empty `.coverage`
/// that isn't a coverage database. Verifying it actually carries per-test
/// contexts is left to `from_path`, which errors with the regeneration recipe.
pub(crate) fn discover_coverage_db(project_root: &Path) -> Option<PathBuf> {
    let candidate = project_root.join(".coverage");
    if !candidate.is_file() {
        return None;
    }
    match filter::coverage::is_sqlite(&candidate) {
        Ok(true) => {
            info!(
                "auto-discovered coverage DB at {} (no `coverage` key set)",
                candidate.display()
            );
            Some(candidate)
        }
        _ => None,
    }
}

/// Freshness check for a coverage file: return the first `.py` whose mtime is
/// newer than the coverage file's, i.e. code that changed after the coverage
/// was recorded and would therefore be described by stale test-context data.
/// `None` means the coverage is at least as new as every source/test file
/// (fresh), or that mtimes couldn't be read (be permissive — never block a run
/// on a stat failure).
///
/// Which trees are scanned depends on whether a `tests` dir is known:
///  - `tests` given → `source_root` plus `tests` (a nested tests dir is folded
///    into the `source_root` walk to avoid double-stating).
///  - `tests` unknown → the whole `project_root`, so a newly-added test
///    *anywhere* (the primary staleness case) is still caught rather than
///    silently missed. Vendored/cache dirs are pruned so this stays a cheap
///    stat walk instead of a venv/site-packages crawl.
///
/// The returned path is just *an* offending file for the warning message, not
/// necessarily the newest — the walk short-circuits on the first one found.
pub(crate) fn newer_source_than_coverage(
    coverage: &Path,
    source_root: &Path,
    tests: Option<&Path>,
    project_root: &Path,
) -> Option<PathBuf> {
    let cov_mtime = std::fs::metadata(coverage).ok()?.modified().ok()?;
    let mut roots: Vec<&Path> = Vec::new();
    match tests {
        Some(t) => {
            roots.push(source_root);
            // Skip a tests dir nested under source_root — walking source_root
            // already covers it, and double-walking is wasted stats.
            if !t.starts_with(source_root) {
                roots.push(t);
            }
        }
        // No configured tests dir: scan the whole project so a new test file
        // located outside source_root still trips the guard.
        None => roots.push(project_root),
    }
    for root in roots {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_pruned_dir(e))
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "py") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if mtime > cov_mtime {
                return Some(path.to_path_buf());
            }
        }
    }
    None
}

/// Directories never worth scanning for freshness: version control, virtual
/// envs, vendored deps, and tool caches. Pruning them keeps the project-root
/// walk cheap and stops a stale dependency `.py` from spuriously tripping the
/// guard. Only prunes directories — files always pass through.
fn is_pruned_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    matches!(
        entry.file_name().to_str(),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | ".venv"
                | "venv"
                | "node_modules"
                | "__pycache__"
                | "site-packages"
                | ".fermut"
                | ".tox"
                | ".nox"
                | ".mypy_cache"
                | ".pytest_cache"
                | ".ruff_cache"
        )
    )
}

fn parse_isolation_env(s: String) -> Option<IsolationMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(IsolationMode::Auto),
        "copy" => Some(IsolationMode::Copy),
        "hardlink" => Some(IsolationMode::Hardlink),
        "reflink" => Some(IsolationMode::Reflink),
        _ => None,
    }
}

/// CLI list (when present) overrides file list. Empty CLI list = no allowlist.
///
/// Under `value_delimiter = ','`, clap turns `--ops ""` into `vec![""]`, not
/// `vec![]`. Filter blanks before the empty-check so `--ops ""` clears the
/// list (intended escape hatch) instead of erroring on an unknown operator.
fn merge_op_list(
    cli: Option<Vec<String>>,
    file: &Option<Vec<String>>,
) -> Result<Option<HashSet<Operator>>> {
    if let Some(list) = cli {
        let cleaned: Vec<String> = list.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if cleaned.is_empty() {
            return Ok(None);
        }
        return Ok(Some(filter::parse_op_names(&cleaned)?));
    }
    if let Some(list) = file {
        if list.is_empty() {
            return Ok(None);
        }
        return Ok(Some(filter::parse_op_names(list)?));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FilterArgs;

    fn empty_filter() -> FilterArgs {
        FilterArgs {
            ops: None,
            skip_ops: None,
            diff_only: None,
            since: None,
            no_diff_only: false,
            coverage: None,
            no_coverage: false,
            select_tests: None,
            experimental: false,
            parity: false,
            operators: None,
            exclude: Vec::new(),
        }
    }

    #[test]
    fn discover_coverage_db_finds_sqlite_at_root() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".coverage");
        // Minimal real SQLite DB (rusqlite writes the magic header on create).
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("CREATE TABLE coverage_schema (version integer)", [])
            .unwrap();
        drop(conn);
        assert_eq!(discover_coverage_db(tmp.path()), Some(db));
    }

    #[test]
    fn discover_coverage_db_ignores_non_sqlite_file() {
        // A stray `.coverage` that isn't a SQLite DB must not be picked up.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".coverage"), "not a database").unwrap();
        assert_eq!(discover_coverage_db(tmp.path()), None);
    }

    #[test]
    fn discover_coverage_db_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(discover_coverage_db(tmp.path()), None);
    }

    /// Stamp `path`'s mtime to `epoch_secs` so freshness tests are deterministic
    /// regardless of filesystem timestamp resolution.
    fn set_mtime(path: &Path, epoch_secs: i64) {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(epoch_secs, 0)).unwrap();
    }

    #[test]
    fn freshness_none_when_coverage_newer_than_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let py = src.join("a.py");
        std::fs::write(&py, "x = 1\n").unwrap();
        let cov = tmp.path().join(".coverage");
        std::fs::write(&cov, "db").unwrap();
        set_mtime(&py, 1000);
        set_mtime(&cov, 2000); // coverage recorded after the source → fresh
        assert_eq!(newer_source_than_coverage(&cov, &src, None, &src), None);
    }

    #[test]
    fn freshness_flags_source_edited_after_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let py = src.join("a.py");
        std::fs::write(&py, "x = 1\n").unwrap();
        let cov = tmp.path().join(".coverage");
        std::fs::write(&cov, "db").unwrap();
        set_mtime(&cov, 1000);
        set_mtime(&py, 2000); // source edited after coverage → stale
        assert_eq!(newer_source_than_coverage(&cov, &src, None, &src), Some(py));
    }

    #[test]
    fn freshness_flags_new_test_after_coverage() {
        // A test added after coverage was recorded isn't reflected in the
        // contexts → its would-be kills become false survivors. Detected via
        // the separate tests dir.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let tests = tmp.path().join("tests");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&tests).unwrap();
        let s = src.join("a.py");
        std::fs::write(&s, "x = 1\n").unwrap();
        let cov = tmp.path().join(".coverage");
        std::fs::write(&cov, "db").unwrap();
        set_mtime(&s, 500);
        set_mtime(&cov, 1000);
        let t = tests.join("test_a.py");
        std::fs::write(&t, "def test(): pass\n").unwrap();
        set_mtime(&t, 2000); // new test after coverage → stale
        assert_eq!(
            newer_source_than_coverage(&cov, &src, Some(&tests), &src),
            Some(t)
        );
    }

    #[test]
    fn freshness_flags_new_test_via_project_root_when_tests_unset() {
        // No configured tests dir: a test added anywhere under the project after
        // coverage was recorded must still be caught by walking project_root.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let tests = tmp.path().join("tests");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&tests).unwrap();
        let s = src.join("a.py");
        std::fs::write(&s, "x = 1\n").unwrap();
        let cov = tmp.path().join(".coverage");
        std::fs::write(&cov, "db").unwrap();
        set_mtime(&s, 500);
        set_mtime(&cov, 1000);
        let t = tests.join("test_a.py");
        std::fs::write(&t, "def test(): pass\n").unwrap();
        set_mtime(&t, 2000); // new test after coverage, tests dir unknown → stale
        assert_eq!(
            newer_source_than_coverage(&cov, &src, None, tmp.path()),
            Some(t)
        );
    }

    #[test]
    fn freshness_prunes_vendored_dirs() {
        // A newer `.py` inside a pruned dir (e.g. .venv/site-packages) must not
        // trip the guard — those are dependencies, not the code under test.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let venv = tmp.path().join(".venv");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&venv).unwrap();
        let s = src.join("a.py");
        std::fs::write(&s, "x = 1\n").unwrap();
        let cov = tmp.path().join(".coverage");
        std::fs::write(&cov, "db").unwrap();
        set_mtime(&s, 500);
        set_mtime(&cov, 1000);
        let dep = venv.join("dep.py");
        std::fs::write(&dep, "y = 2\n").unwrap();
        set_mtime(&dep, 2000); // newer, but vendored → pruned
        assert_eq!(
            newer_source_than_coverage(&cov, &src, None, tmp.path()),
            None
        );
    }

    #[test]
    fn freshness_ignores_non_python_files() {
        // A newer README/data file must not trip the guard — only `.py` counts.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let py = src.join("a.py");
        std::fs::write(&py, "x = 1\n").unwrap();
        let cov = tmp.path().join(".coverage");
        std::fs::write(&cov, "db").unwrap();
        set_mtime(&py, 500);
        set_mtime(&cov, 1000);
        let readme = src.join("notes.md");
        std::fs::write(&readme, "hi\n").unwrap();
        set_mtime(&readme, 2000); // newer, but not .py → ignored
        assert_eq!(newer_source_than_coverage(&cov, &src, None, &src), None);
    }

    /// A `RunConfigArgs` with every flag defaulted off/none, carrying the
    /// given `filter`. Tests tweak individual fields before calling.
    fn run_args(filter: FilterArgs) -> RunConfigArgs {
        RunConfigArgs {
            tests: None,
            jobs: None,
            timeout: None,
            no_ty_filter: false,
            ruff_filter: false,
            tce: false,
            hypothesis_seed: None,
            pytest_args: Vec::new(),
            no_cache: false,
            cache_path: None,
            no_history: false,
            history_path: None,
            sample: None,
            sample_seed: None,
            shard: None,
            runner: None,
            python: None,
            isolation: None,
            no_equiv_detect: false,
            cache_scope: None,
            fail_under: None,
            no_verify_baseline: false,
            baseline_timeout: None,
            no_smart_order: false,
            smart_order: false,
            no_pytest_plugin: false,
            schemata: false,
            max_time: None,
            timeout_factor: None,
            baseline_secs: None,
            record_kill_sets: None,
            only_dominators: false,
            audit_inferred: None,
            cache_audit_rate: None,
            no_cache_audit: false,
            filter,
        }
    }

    fn call(cli_path: PathBuf, cli_fail_under: Option<f64>) -> Result<Config> {
        call_with_filter(cli_path, cli_fail_under, empty_filter())
    }

    fn call_with_filter(
        cli_path: PathBuf,
        cli_fail_under: Option<f64>,
        filter: FilterArgs,
    ) -> Result<Config> {
        let mut args = run_args(filter);
        args.fail_under = cli_fail_under;
        build_config(cli_path, args)
    }

    #[test]
    fn cache_and_history_co_locate_at_project_root() {
        // Run targets a `src/` subdir but the project root (pyproject.toml) is
        // one level up. Both cache and history must anchor at the project root
        // `.fermut/`, not under `src/` — otherwise the cache and history
        // diverge and the documented `actions/cache` key misses.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();

        let cfg = call(src.clone(), None).unwrap();
        let dot = root.join(".fermut");
        assert_eq!(cfg.cache_path, dot.join("cache.json"));
        assert_eq!(cfg.history_path, dot.join("history.jsonl"));
        assert!(
            !cfg.cache_path.starts_with(&src),
            "cache must anchor at the project root, not under src/"
        );
    }

    #[test]
    fn file_target_anchors_cache_and_history_in_parent_dir() {
        // Regression: a single-file target set source_root to the file, so
        // the default cache/history paths became `file.py/.fermut/...` and
        // mkdir failed at save time — silently disabling the cache.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("mod.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let cfg = call(file.clone(), None).unwrap();
        let parent_dot = tmp.path().join(".fermut");
        assert_eq!(cfg.cache_path, parent_dot.join("cache.json"));
        assert_eq!(cfg.history_path, parent_dot.join("history.jsonl"));
        // Sanity: the bogus file-as-dir path is not used.
        assert!(!cfg.cache_path.starts_with(&file));
    }

    #[test]
    fn dir_target_anchors_cache_under_the_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = call(tmp.path().to_path_buf(), None).unwrap();
        assert_eq!(
            cfg.cache_path,
            tmp.path().join(".fermut").join("cache.json")
        );
    }

    #[test]
    fn fail_under_cli_overrides_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "fail_under = 50.0\n").unwrap();
        let cfg = call(tmp.path().to_path_buf(), Some(90.0)).unwrap();
        assert_eq!(cfg.fail_under, Some(90.0));
    }

    #[test]
    fn fail_under_file_used_when_cli_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "fail_under = 75.0\n").unwrap();
        let cfg = call(tmp.path().to_path_buf(), None).unwrap();
        assert_eq!(cfg.fail_under, Some(75.0));
    }

    #[test]
    fn fail_under_none_when_neither_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = call(tmp.path().to_path_buf(), None).unwrap();
        assert_eq!(cfg.fail_under, None);
    }

    #[test]
    fn fail_under_rejects_value_above_100() {
        let tmp = tempfile::tempdir().unwrap();
        let err = call(tmp.path().to_path_buf(), Some(150.0)).unwrap_err();
        assert!(
            err.to_string().contains("between 0.0 and 100.0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fail_under_rejects_negative_value() {
        let tmp = tempfile::tempdir().unwrap();
        let err = call(tmp.path().to_path_buf(), Some(-1.0)).unwrap_err();
        assert!(
            err.to_string().contains("between 0.0 and 100.0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fail_under_rejects_out_of_range_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "fail_under = 101.0\n").unwrap();
        let err = call(tmp.path().to_path_buf(), None).unwrap_err();
        assert!(
            err.to_string().contains("between 0.0 and 100.0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn exclude_from_file_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "exclude = [\"alembic/**\", \"tests/integration/**\"]\n",
        )
        .unwrap();
        let cfg = call(tmp.path().to_path_buf(), None).unwrap();
        assert_eq!(
            cfg.exclude,
            vec!["alembic/**".to_string(), "tests/integration/**".to_string()]
        );
    }

    #[test]
    fn exclude_cli_overrides_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "exclude = [\"a/**\"]\n").unwrap();
        let mut f = empty_filter();
        f.exclude = vec!["b/**".to_string()];
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert_eq!(cfg.exclude, vec!["b/**".to_string()]);
    }

    #[test]
    fn exclude_invalid_glob_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "exclude = [\"[bad\"]\n").unwrap();
        let err = call(tmp.path().to_path_buf(), None).unwrap_err();
        assert!(
            err.to_string().contains("invalid exclude glob"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_diff_only_clears_config_diff_base() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "diff_only = \"origin/main\"\n",
        )
        .unwrap();
        let mut f = empty_filter();
        f.no_diff_only = true;
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert!(cfg.diff_base.is_none(), "diff_base = {:?}", cfg.diff_base);
        assert!(cfg.since.is_none());
    }

    #[test]
    fn select_tests_narrows_coverage_and_disables_history() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        let src = root.join("m.py");
        std::fs::write(&src, "x = 1\ny = 2\n").unwrap();
        let cov = root.join("coverage.json");
        let doc = serde_json::json!({"files": {src.to_str().unwrap(): {"contexts": {
            "1": ["tests/test_a.py::t|run"],
            "2": ["tests/test_b.py::t|run"],
        }}}});
        std::fs::write(&cov, doc.to_string()).unwrap();
        let sel = root.join("sel.txt");
        std::fs::write(&sel, "tests/test_a.py::t\n").unwrap();
        let mut f = empty_filter();
        f.coverage = Some(cov);
        f.select_tests = Some(sel);
        let cfg = call_with_filter(root.clone(), None, f).unwrap();
        assert!(!cfg.history, "a narrowed run must not record history");
        let full = cfg
            .coverage_unselected
            .as_ref()
            .expect("full contexts kept");
        let narrowed = cfg.coverage.as_ref().unwrap();
        assert!(full.tests_for(&src, 2).is_some());
        assert!(narrowed.tests_for(&src, 2).is_none());
        assert!(narrowed.tests_for(&src, 1).is_some());
    }

    #[test]
    fn select_tests_covering_nothing_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("pyproject.toml"), "").unwrap();
        let src = root.join("m.py");
        std::fs::write(&src, "x = 1\n").unwrap();
        let cov = root.join("coverage.json");
        let doc = serde_json::json!({"files": {src.to_str().unwrap(): {"contexts": {
            "1": ["tests/test_a.py::t|run"],
        }}}});
        std::fs::write(&cov, doc.to_string()).unwrap();
        let sel = root.join("sel.txt");
        std::fs::write(&sel, "tests/test_other.py::t\n").unwrap();
        let mut f = empty_filter();
        f.coverage = Some(cov);
        f.select_tests = Some(sel);
        let err = call_with_filter(root, None, f).unwrap_err().to_string();
        assert!(err.contains("no mutant would be tested"), "{err}");
        assert!(err.contains(crate::ISSUES_URL), "{err}");
    }

    #[test]
    fn cli_since_overrides_config_diff_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "diff_only = \"main\"\n").unwrap();
        let mut f = empty_filter();
        f.since = Some("HEAD".into());
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert_eq!(cfg.since.as_deref(), Some("HEAD"));
        assert!(cfg.diff_base.is_none());
    }

    #[test]
    fn no_diff_only_clears_config_since() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "since = \"HEAD~10\"\n").unwrap();
        let mut f = empty_filter();
        f.no_diff_only = true;
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert!(cfg.since.is_none());
        assert!(cfg.diff_base.is_none());
    }

    #[test]
    fn no_coverage_clears_config_coverage() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "coverage = \"coverage.json\"\n",
        )
        .unwrap();
        let mut f = empty_filter();
        f.no_coverage = true;
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert!(cfg.coverage_path.is_none());
        assert!(cfg.coverage.is_none());
    }

    #[test]
    fn schemata_needs_the_pytest_runner() {
        assert!(check_schemata(RunnerKind::Rstest).is_err());
        assert!(check_schemata(RunnerKind::Unittest).is_err());
        assert_eq!(check_schemata(RunnerKind::Pytest).is_ok(), cfg!(unix));
    }

    #[test]
    fn resolve_smart_order_precedence() {
        // (cli_no, cli_yes, file) -> on. A timeout is deliberately absent from
        // the signature: ordering is verdict-neutral even under a timeout (it
        // only flips killed↔timed_out, both detected), so a timeout must never
        // enter the decision. This is the guard for finding #1 — the old code
        // auto-disabled the default under an explicit timeout.
        let cases = [
            // Default on when nothing forces it.
            ((false, false, None), true),
            // Config value honored.
            ((false, false, Some(false)), false),
            ((false, false, Some(true)), true),
            // `--no-smart-order` wins over everything.
            ((true, false, None), false),
            ((true, false, Some(true)), false),
            // `--smart-order` forces on over a config-off.
            ((false, true, Some(false)), true),
            ((false, true, None), true),
        ];
        for ((cli_no, cli_yes, file), want) in cases {
            assert_eq!(
                resolve_smart_order(cli_no, cli_yes, file),
                want,
                "resolve_smart_order({cli_no}, {cli_yes}, {file:?})"
            );
        }
    }

    #[test]
    fn smart_order_defaults_on() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert!(cfg.smart_order, "smart ordering is on by default");
        assert!(cfg.kill_order_path.ends_with(".fermut/kill-order.json"));
    }

    #[test]
    fn kill_order_path_from_config_resolves_against_config_dir() {
        // A relative `kill_order_path` in config resolves against the config
        // dir, not left hardcoded to `.fermut/kill-order.json`. Guards the
        // sidecar-path knob against regressing back to a fixed location.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "kill_order_path = \"artifacts/ko.json\"\n",
        )
        .unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        // `resolve_path` canonicalizes (on macOS `/var` → `/private/var`), so
        // compare the tail + absoluteness rather than the exact temp prefix.
        assert!(cfg.kill_order_path.is_absolute());
        assert!(
            cfg.kill_order_path.ends_with("artifacts/ko.json"),
            "kill_order_path {:?} should resolve under the config dir",
            cfg.kill_order_path
        );
    }

    #[test]
    fn only_dominators_refuses_a_diff_scope_from_config() {
        // clap's conflict can't see `diff_only` in the config file; the PR-gate
        // refusal must still hold.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "diff_only = \"main\"\n").unwrap();
        let mut args = run_args(empty_filter());
        args.only_dominators = true;
        let err = build_config(tmp.path().to_path_buf(), args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--only-dominators"), "{err}");
    }

    #[test]
    fn operator_profile_from_config_and_cli_override() {
        use crate::config::OperatorProfile;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert_eq!(
            cfg.operator_profile,
            OperatorProfile::Minimal,
            "minimal is the default"
        );

        std::fs::write(tmp.path().join("fermut.toml"), "operators = \"classic\"\n").unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert_eq!(cfg.operator_profile, OperatorProfile::Classic);

        let f = FilterArgs {
            operators: Some(crate::cli::OperatorProfileCli::Full),
            ..empty_filter()
        };
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert_eq!(cfg.operator_profile, OperatorProfile::Full, "CLI wins");
    }

    #[test]
    fn deprecated_default_profile_name_means_classic() {
        use crate::config::OperatorProfile;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "operators = \"default\"\n").unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert_eq!(
            cfg.operator_profile,
            OperatorProfile::Classic,
            "config file"
        );

        let f = FilterArgs {
            operators: Some(crate::cli::OperatorProfileCli::Default),
            ..empty_filter()
        };
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert_eq!(cfg.operator_profile, OperatorProfile::Classic, "CLI");

        std::fs::write(tmp.path().join("fermut.toml"), "operators = \"bogus\"\n").unwrap();
        assert!(call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).is_err());
    }

    #[test]
    fn timeout_factor_from_config_and_rejects_below_two() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "timeout_factor = 3.0\n").unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert_eq!(cfg.timeout_factor, Some(3.0));
        let mut args = run_args(empty_filter());
        args.timeout_factor = Some(1.5);
        let err = build_config(tmp.path().to_path_buf(), args)
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least 2"), "{err}");

        // Default is on; 0 turns it off, from the flag or from config.
        let none = tempfile::tempdir().unwrap();
        let cfg = call_with_filter(none.path().to_path_buf(), None, empty_filter()).unwrap();
        assert_eq!(
            cfg.timeout_factor,
            Some(crate::config::DEFAULT_TIMEOUT_FACTOR)
        );
        let mut args = run_args(empty_filter());
        args.timeout_factor = Some(0.0);
        let cfg = build_config(tmp.path().to_path_buf(), args).unwrap();
        assert_eq!(cfg.timeout_factor, None);
        std::fs::write(none.path().join("fermut.toml"), "timeout_factor = 0\n").unwrap();
        let cfg = call_with_filter(none.path().to_path_buf(), None, empty_filter()).unwrap();
        assert_eq!(cfg.timeout_factor, None);

        let mut args = run_args(empty_filter());
        args.baseline_secs = Some(1.5);
        let cfg = build_config(tmp.path().to_path_buf(), args).unwrap();
        assert_eq!(cfg.baseline_secs, Some(1.5));
        let mut args = run_args(empty_filter());
        args.baseline_secs = Some(-1.0);
        assert!(build_config(tmp.path().to_path_buf(), args).is_err());
    }

    /// What `f` logged on this thread.
    fn logged(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let w = buf.clone();
        let sub = tracing_subscriber::fmt()
            .with_writer(move || w.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn timeout_factor_warns_only_when_no_baseline_can_size_it() {
        let tmp = tempfile::tempdir().unwrap();
        let warns = |no_verify: bool, baseline_secs: Option<f64>| {
            let mut args = run_args(empty_filter());
            args.timeout_factor = Some(3.0);
            args.no_verify_baseline = no_verify;
            args.baseline_secs = baseline_secs;
            logged(|| {
                build_config(tmp.path().to_path_buf(), args).unwrap();
            })
            .contains("--timeout-factor needs the baseline run")
        };
        assert!(warns(true, None));
        assert!(!warns(false, None), "baseline run sizes it");
        assert!(!warns(true, Some(2.0)), "--baseline-secs sizes it");
        let mut args = run_args(empty_filter());
        args.no_verify_baseline = true;
        let default_warns = logged(|| {
            build_config(tmp.path().to_path_buf(), args).unwrap();
        })
        .contains("--timeout-factor needs the baseline run");
        assert!(!default_warns, "the default factor stays quiet");
    }

    #[test]
    fn smart_order_config_false_disables() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "smart_order = false\n").unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert!(!cfg.smart_order);
    }

    #[test]
    fn cli_no_smart_order_overrides_config_on() {
        // Config says on; the CLI flag must win and turn it off.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "smart_order = true\n").unwrap();
        let mut args = run_args(empty_filter());
        args.no_smart_order = true;
        let cfg = build_config(tmp.path().to_path_buf(), args).unwrap();
        assert!(!cfg.smart_order, "--no-smart-order wins over config on");
    }

    #[test]
    fn pytest_plugin_defaults_on_and_both_switches_disable_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = build_config(tmp.path().to_path_buf(), run_args(empty_filter())).unwrap();
        assert!(cfg.pytest_plugin, "the reporter plugin is on by default");

        let mut args = run_args(empty_filter());
        args.no_pytest_plugin = true;
        let cfg = build_config(tmp.path().to_path_buf(), args).unwrap();
        assert!(!cfg.pytest_plugin, "--no-pytest-plugin disables it");

        std::fs::write(tmp.path().join("fermut.toml"), "pytest_plugin = false\n").unwrap();
        let cfg = build_config(tmp.path().to_path_buf(), run_args(empty_filter())).unwrap();
        assert!(!cfg.pytest_plugin, "pytest_plugin = false disables it");
    }

    #[test]
    fn explicit_config_timeout_keeps_default_smart_order_on() {
        // Finding #1 regression guard: a per-mutant timeout does NOT disable
        // smart ordering. Ordering only flips killed↔timed_out (both detected in
        // the score), never producing or removing a survivor, so the score and
        // the regression gate stay order-invariant — and killer-first ordering
        // helps *most* under a timeout. An explicit `timeout` in config must
        // leave the default-on ordering on.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "timeout = 30\n").unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert!(
            cfg.smart_order,
            "an explicit timeout must NOT disable default-on smart ordering"
        );
    }

    #[test]
    fn explicit_smart_order_true_survives_timeout() {
        // Config `smart_order = true` alongside a timeout stays on (as does the
        // default — see the sibling test; a timeout never disables ordering).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "smart_order = true\ntimeout = 30\n",
        )
        .unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert!(
            cfg.smart_order,
            "explicit smart_order = true must survive an explicit timeout"
        );
    }

    #[test]
    fn explicit_cli_timeout_keeps_smart_order_on() {
        // Finding #1 guard via the CLI `--timeout`, no config at all: a timeout
        // must not disable default-on ordering (order-invariant score).
        let tmp = tempfile::tempdir().unwrap();
        let mut args = run_args(empty_filter());
        args.timeout = Some(5); // explicit per-mutant timeout
        let cfg = build_config(tmp.path().to_path_buf(), args).unwrap();
        assert!(
            cfg.smart_order,
            "--timeout must NOT disable default-on smart ordering"
        );
    }

    #[test]
    fn cli_smart_order_forces_on_over_config_off_with_timeout() {
        // `--smart-order` forces ordering on over a config `smart_order = false`,
        // timeout present or not. (The timeout itself never disables ordering.)
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "smart_order = false\n").unwrap();
        let mut args = run_args(empty_filter());
        args.timeout = Some(5);
        args.smart_order = true; // forces on over config-off
        let cfg = build_config(tmp.path().to_path_buf(), args).unwrap();
        assert!(
            cfg.smart_order,
            "--smart-order must force ordering on over config smart_order = false"
        );
    }

    #[test]
    fn ops_empty_string_clears_allowlist() {
        // Under value_delimiter = ',', `--ops ""` becomes vec![""], not vec![].
        // Empty strings must short-circuit the same way an empty Vec does.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "ops = [\"arith-op-swap\"]\n",
        )
        .unwrap();
        let mut f = empty_filter();
        f.ops = Some(vec![String::new()]);
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, f).unwrap();
        assert!(
            cfg.ops_allow.is_none(),
            "expected cleared allowlist, got {:?}",
            cfg.ops_allow
        );
    }

    #[test]
    fn fail_under_accepts_boundary_values() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            call(tmp.path().to_path_buf(), Some(0.0))
                .unwrap()
                .fail_under,
            Some(0.0)
        );
        assert_eq!(
            call(tmp.path().to_path_buf(), Some(100.0))
                .unwrap()
                .fail_under,
            Some(100.0)
        );
    }
}
