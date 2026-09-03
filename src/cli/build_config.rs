//! Merge CLI arguments with the loaded config file into a runtime `Config`.
//!
//! Precedence: **CLI explicit value > config-file value > built-in default**.
//! Each list (`ops`, `skip_ops`) merges independently — a CLI `--ops`
//! overrides the file's `ops` but leaves the file's `skip_ops` intact, and
//! vice versa.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use super::FilterArgs;
use crate::config::{CacheScope, Config, ConfigSource, IsolationMode, LoadedConfig, RunnerKind};
use crate::filter;
use crate::mutator::Operator;

use super::merge::parse_shard_spec;

pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default wall-clock cap for the baseline (full-suite) run. Generous because
/// the whole suite runs once; raise with `--baseline-timeout` for big suites.
pub(super) const DEFAULT_BASELINE_TIMEOUT_SECS: u64 = 300;

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_config(
    cli_path: PathBuf,
    cli_tests: Option<PathBuf>,
    cli_jobs: Option<usize>,
    cli_timeout: Option<u64>,
    cli_no_ty_filter: bool,
    cli_ruff_filter: bool,
    cli_hypothesis_seed: Option<u64>,
    cli_pytest_args: Vec<String>,
    cli_no_cache: bool,
    cli_cache_path: Option<PathBuf>,
    cli_no_history: bool,
    cli_history_path: Option<PathBuf>,
    cli_sample: Option<f64>,
    cli_sample_seed: Option<u64>,
    cli_shard: Option<(u32, u32)>,
    cli_runner: Option<RunnerKind>,
    cli_python: Option<PathBuf>,
    cli_isolation: Option<IsolationMode>,
    cli_no_equiv_detect: bool,
    cli_cache_scope: Option<CacheScope>,
    cli_fail_under: Option<f64>,
    cli_no_verify_baseline: bool,
    cli_baseline_timeout: Option<u64>,
    cli_no_smart_order: bool,
    f: FilterArgs,
) -> Result<Config> {
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
    let experimental = f.experimental || file.experimental.unwrap_or(false);
    let parity = f.parity || file.parity.unwrap_or(false);

    let ops_allow = merge_op_list(f.ops, &file.ops)?;
    let ops_deny = merge_op_list(f.skip_ops, &file.skip_ops)?.unwrap_or_default();

    let (diff_base, since) = if f.no_diff_only {
        (None, None)
    } else {
        let diff_base = f.diff_only.or_else(|| file.diff_only.clone());
        let since = f.since.or_else(|| file.since.clone());
        if diff_base.is_some() && since.is_some() {
            return Err(anyhow::anyhow!(
                "`--diff-only` and `--since` are mutually exclusive"
            ));
        }
        (diff_base, since)
    };

    let coverage_path = if f.no_coverage {
        None
    } else {
        f.coverage
            .or_else(|| file.coverage.clone().map(|p| loaded.resolve_path(p)))
    };
    let coverage = match &coverage_path {
        Some(p) => {
            let project_root = crate::runner::find_project_root(&source_root)
                .unwrap_or_else(|| source_root.clone());
            Some(crate::filter::coverage::CoverageContexts::from_path(
                p,
                &source_root,
                &project_root,
            )?)
        }
        None => None,
    };

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

    let history = if cli_no_history {
        false
    } else {
        file.history.unwrap_or(true)
    };
    let history_path = cli_history_path
        .or_else(|| file.history_path.clone().map(|p| loaded.resolve_path(p)))
        .unwrap_or_else(|| crate::history::default_history_path(&artifact_root));

    // Smart test ordering: default on; `--no-smart-order` (CLI) wins, else the
    // config value, else on. The sidecar lives beside cache/history.
    let smart_order = if cli_no_smart_order {
        false
    } else {
        file.smart_order.unwrap_or(true)
    };
    let kill_order_path = artifact_root.join(".fermut").join("kill-order.json");

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

    Ok(Config {
        source_root,
        tests,
        jobs,
        timeout_secs,
        ty_filter,
        ruff_filter,
        experimental,
        parity,
        ops_allow,
        ops_deny,
        diff_base,
        since,
        coverage_path,
        coverage,
        hypothesis_seed,
        pytest_args,
        cache,
        cache_path,
        smart_order,
        kill_order_path,
        history,
        history_path,
        sample_ratio,
        sample_seed,
        shard,
        runner,
        python,
        isolation,
        equiv_detect,
        cache_scope,
        fail_under,
        exclude,
        verify_baseline,
        baseline_timeout_secs,
    })
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

    fn empty_filter() -> FilterArgs {
        FilterArgs {
            ops: None,
            skip_ops: None,
            diff_only: None,
            since: None,
            no_diff_only: false,
            coverage: None,
            no_coverage: false,
            experimental: false,
            parity: false,
            exclude: Vec::new(),
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
        build_config(
            cli_path,
            None,
            None,
            None,
            false,
            false,
            None,
            Vec::new(),
            false,
            None,
            false,
            None,
            None,
            None,
            None,
            None, // cli_runner
            None, // cli_python
            None, // cli_isolation
            false,
            None,
            cli_fail_under,
            false,
            None,
            false, // cli_no_smart_order
            filter,
        )
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
    fn smart_order_defaults_on() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = call_with_filter(tmp.path().to_path_buf(), None, empty_filter()).unwrap();
        assert!(cfg.smart_order, "smart ordering is on by default");
        assert!(cfg.kill_order_path.ends_with(".fermut/kill-order.json"));
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
        let cfg = build_config(
            tmp.path().to_path_buf(),
            None,
            None,
            None,
            false,
            false,
            None,
            Vec::new(),
            false,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
            None,
            false,
            None,
            true, // cli_no_smart_order
            empty_filter(),
        )
        .unwrap();
        assert!(!cfg.smart_order, "--no-smart-order wins over config on");
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
