//! Mutation filters. A `Filter` rejects a mutant before it reaches the test
//! runner. Filters are composable — `build_chain` returns a `Vec<Box<dyn Filter>>`
//! ordered cheap-to-expensive so rejections short-circuit before the slow ones run.

pub mod coverage;
pub mod diff;
pub mod experimental;
pub mod operator;
pub mod parity;
pub mod ruff;
pub mod sample;
pub mod shard;
pub mod tce;
pub mod ty;
pub mod ty_embedded;

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;
use tracing::warn;
use wait_timeout::ChildExt;

use crate::runner::process_group::{kill_group, with_new_process_group};

use crate::config::Config;
use crate::mutator::{Mutant, Operator};

pub trait Filter: Send + Sync {
    fn name(&self) -> &'static str;
    /// Returns Ok(true) if the mutant should be tested, Ok(false) to drop it.
    fn admits(&self, mutant: &Mutant) -> Result<bool>;
}

/// Write `patched` to a temp `.py` file for a subprocess linter/type-checker.
///
/// Two properties matter for baseline-vs-mutant symmetry:
///
/// 1. **Location.** The temp file sits *next to* `original` so the nearest
///    `pyproject`/tool config, sibling modules, and package `__init__` resolve
///    the same way they did for the baseline. A `/tmp` file would resolve a
///    different (or no) config and leave every intra-project import unresolved,
///    inflating the mutant's diagnostic count asymmetrically.
/// 2. **Name.** The filename stem is derived from `original`'s stem plus an
///    underscore separator, so it stays a *valid Python module name*. A hyphen
///    (the old `fermut-` prefix) makes the module name invalid and trips
///    module-name lints (ruff `N999`, ty) on the mutant but not the baseline —
///    the exact asymmetry this whole approach exists to avoid.
///
/// If the source directory is not writable (installed package, read-only mount,
/// CI cache), fall back to the system temp dir: degrading config fidelity beats
/// aborting the entire filter phase.
pub(crate) fn patched_tempfile(original: &Path, patched: &str) -> Result<NamedTempFile> {
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("mutant");
    let prefix = format!("{stem}_fermut_");
    // `Path::new("foo.py").parent()` is `Some("")`, not `None`, so guard the
    // empty case explicitly rather than relying on the `None` branch.
    let parent = match original.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let tmp = match tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".py")
        .tempfile_in(parent)
    {
        Ok(f) => f,
        Err(e) => {
            // Fallback to /tmp restores the config-asymmetry the in-dir temp
            // exists to avoid, so warn once — otherwise a read-only source tree
            // silently drops valid mutants with no signal.
            static WARNED: Once = Once::new();
            WARNED.call_once(|| {
                warn!(
                    dir = %parent.display(),
                    error = %e,
                    "source dir not writable; writing mutant temp files to the system \
                     temp dir instead — tool config may resolve differently and skew \
                     diagnostic counts"
                );
            });
            tempfile::Builder::new()
                .prefix(&prefix)
                .suffix(".py")
                .tempfile()
                .context("creating temp file")?
        }
    };
    std::fs::write(tmp.path(), patched).context("writing patched source")?;
    Ok(tmp)
}

/// Wall-clock cap for a single filter subprocess (one `ty`/`ruff check` on one
/// file). Type-checking or linting a single file is sub-second in practice; this
/// bound only fires when the tool wedges (a bad plugin, a stuck filesystem, a
/// runaway inference loop). Without it a hung filter stalls the *entire* run
/// forever — the test runners already cap themselves with `wait_timeout`, but
/// the filters used a plain blocking `.output()` with no escape hatch.
pub(crate) const FILTER_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `cmd`, capturing stdout/stderr, but kill it (and its whole process
/// group) if it outruns [`FILTER_SUBPROCESS_TIMEOUT`]. Returns `Ok(None)` on
/// timeout so the caller can *bypass* the filter (admit the mutant) instead of
/// hanging the run.
///
/// Output is buffered to temp files rather than OS pipes so a chatty tool can't
/// deadlock on a full pipe buffer while we sit in `wait_timeout` — same guard
/// the baseline runner uses.
pub(crate) fn run_filter_with_timeout(mut cmd: Command) -> Result<Option<Output>> {
    let out_file = NamedTempFile::new().context("creating filter stdout buffer")?;
    let err_file = NamedTempFile::new().context("creating filter stderr buffer")?;
    cmd.stdout(
        out_file
            .reopen()
            .context("reopening filter stdout buffer")?,
    );
    cmd.stderr(
        err_file
            .reopen()
            .context("reopening filter stderr buffer")?,
    );
    // Own process group so the kill reaches any children the tool spawned.
    with_new_process_group(&mut cmd);

    let mut child = cmd.spawn().context("spawning filter subprocess")?;
    match child
        .wait_timeout(FILTER_SUBPROCESS_TIMEOUT)
        .context("waiting on filter subprocess")?
    {
        Some(status) => {
            let stdout = std::fs::read(out_file.path()).unwrap_or_default();
            let stderr = std::fs::read(err_file.path()).unwrap_or_default();
            Ok(Some(Output {
                status,
                stdout,
                stderr,
            }))
        }
        None => {
            kill_group(&mut child);
            let _ = child.wait();
            Ok(None)
        }
    }
}

/// Build the filter chain implied by the config, cheap-to-expensive.
pub fn build_chain(cfg: &Config) -> Result<Vec<Box<dyn Filter>>> {
    let mut chain: Vec<Box<dyn Filter>> = Vec::new();

    chain.push(Box::new(experimental::ExperimentalFilter::new(
        cfg.experimental,
    )));
    chain.push(Box::new(parity::ParityFilter::new(cfg.parity)));

    if cfg.ops_allow.is_some() || !cfg.ops_deny.is_empty() {
        chain.push(Box::new(operator::OperatorFilter::new(
            cfg.ops_allow.clone(),
            cfg.ops_deny.clone(),
        )));
    }

    if let Some((index, total)) = cfg.shard {
        chain.push(Box::new(shard::ShardFilter::new(index, total)));
    }

    if let Some(ratio) = cfg.sample_ratio {
        chain.push(Box::new(sample::SampleFilter::new(
            ratio,
            cfg.sample_seed.unwrap_or(0),
        )));
    }

    if let Some(base) = &cfg.diff_base {
        chain.push(Box::new(diff::DiffFilter::from_git(
            base,
            &cfg.source_root,
        )?));
    }

    if let Some(spec) = &cfg.since {
        chain.push(Box::new(diff::DiffFilter::from_git_since(
            spec,
            &cfg.source_root,
        )?));
    }

    if let Some(ctx) = &cfg.coverage {
        chain.push(Box::new(coverage::CoverageFilter::new(ctx.clone())));
    }

    if cfg.ruff_filter {
        chain.push(Box::new(ruff::RuffFilter::new(&cfg.source_root)?));
    }

    if cfg.ty_filter {
        // Anchor the ty cache at the project root, same as the result cache and
        // history — otherwise `source_root = src/` scatters a third
        // `src/.fermut/ty-cache.json` apart from the project-root `.fermut/`.
        let ty_cache_path =
            ty::TyFilter::default_cache_path(&crate::history::resolve_root(&cfg.source_root));
        let pool_size = cfg.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        });
        chain.push(Box::new(ty::TyFilter::with_cache_and_project(
            Some(ty_cache_path),
            &cfg.source_root,
            pool_size,
        )?));
    }

    // TCE (bytecode-equivalence) last: it spawns a `compile()` subprocess per
    // mutant, so run it after ty's type-error cull has already dropped the
    // cheap-to-reject mutants — fewer survivors reach the per-mutant subprocess.
    if cfg.tce {
        chain.push(Box::new(tce::TceFilter::new()));
    }

    Ok(chain)
}

/// Convenience: parse a comma-separated operator name list into a HashSet.
pub fn parse_op_names(names: &[String]) -> Result<HashSet<Operator>> {
    names
        .iter()
        .map(|s| Operator::from_name(s).ok_or_else(|| anyhow::anyhow!("unknown operator: {}", s)))
        .collect()
}

/// Run `admits` for every filter in order. First reject wins. Returns
/// `Ok(None)` if all admit, `Ok(Some(filter_name))` if rejected.
pub fn first_rejector<'a>(
    filters: &'a [Box<dyn Filter>],
    mutant: &Mutant,
) -> Result<Option<&'a str>> {
    for f in filters {
        if !f.admits(mutant)? {
            return Ok(Some(f.name()));
        }
    }
    Ok(None)
}

/// Canonical filter order, cheap-to-expensive. `build_chain` may include any
/// subset of these depending on config flags, but their relative order must
/// match this list — otherwise an expensive filter can run before a cheap one
/// that would have rejected the mutant for free. Refactors of `build_chain`
/// should keep filters in this order; tests below enforce it.
#[allow(dead_code)] // used only by chain_order_tests; kept as docs/refactor guide.
pub(crate) const CANONICAL_ORDER: &[&str] = &[
    "experimental",
    "parity",
    "operator",
    "shard",
    "sample",
    "diff-only",
    "since",
    "coverage",
    "ruff",
    "ty",
    "tce",
];

#[cfg(test)]
mod chain_order_tests {
    //! `build_chain` must emit filters cheap-to-expensive. The order is the
    //! whole point of the filter chain — running `ty` (slow, spawns a
    //! subprocess per mutant) before `shard` (a constant-time hash modulo)
    //! would scale terribly. These tests freeze that invariant so a
    //! refactor or accidental insertion can't silently reorder.
    //!
    //! Filters that need a binary on PATH (`ruff`, `ty`) or a git repo
    //! (`diff-only`, `since`) aren't easy to construct in a unit test, so
    //! we cover the subset that builds without system deps and separately
    //! assert that the canonical order itself is correctly documented.

    use std::sync::Arc;

    use super::{build_chain, CANONICAL_ORDER};
    use crate::config::{CacheScope, Config, IsolationMode, RunnerKind};
    use crate::filter::coverage::CoverageContexts;
    use crate::mutator::Operator;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn empty_config() -> Config {
        Config {
            source_root: PathBuf::from("."),
            tests: None,
            jobs: None,
            timeout_secs: 30,
            ty_filter: false,
            ruff_filter: false,
            tce: false,
            experimental: false,
            parity: false,
            ops_allow: None,
            ops_deny: HashSet::new(),
            diff_base: None,
            since: None,
            coverage_path: None,
            coverage: None,
            hypothesis_seed: None,
            pytest_args: Vec::new(),
            cache: false,
            cache_audit_rate: 0.0,
            cache_path: PathBuf::from(".fermut/cache.json"),
            smart_order: false,
            pytest_plugin: false,
            kill_order_path: PathBuf::from(".fermut/kill-order.json"),
            history: false,
            history_path: PathBuf::from(".fermut/history.jsonl"),
            sample_ratio: None,
            sample_seed: None,
            shard: None,
            runner: RunnerKind::Pytest,
            python: None,
            unittest_pattern: None,
            isolation: IsolationMode::Auto,
            equiv_detect: false,
            cache_scope: CacheScope::File,
            fail_under: None,
            exclude: Vec::new(),
            verify_baseline: false,
            baseline_timeout_secs: 300,
            max_time_secs: None,
        }
    }

    fn names(cfg: &Config) -> Vec<&'static str> {
        build_chain(cfg)
            .expect("build_chain succeeds for system-dep-free config")
            .iter()
            .map(|f| f.name())
            .collect()
    }

    fn is_subsequence(short: &[&str], long: &[&str]) -> bool {
        let mut li = 0;
        for s in short {
            while li < long.len() && long[li] != *s {
                li += 1;
            }
            if li == long.len() {
                return false;
            }
            li += 1;
        }
        true
    }

    #[test]
    fn empty_config_only_includes_operator_gates() {
        // The experimental and parity gates are unconditionally first in the
        // chain — that's how the build distinguishes stable from gated ops
        // regardless of any flags. The rest of the chain is opt-in. If this
        // test fails because another filter became unconditional, the design
        // changed and both the chain semantics and CANONICAL_ORDER should be
        // reviewed together.
        assert_eq!(names(&empty_config()), vec!["experimental", "parity"]);
    }

    #[test]
    fn all_system_dep_free_filters_in_canonical_order() {
        // Enable every filter that builds without needing git, ruff, or
        // ty on PATH. Asserts the exact emitted order, which catches both
        // misordering and an accidental skip.
        let mut cfg = empty_config();
        cfg.experimental = true;
        cfg.ops_allow = Some(HashSet::from([Operator::ArithOpSwap]));
        cfg.shard = Some((1, 4));
        cfg.sample_ratio = Some(0.5);
        cfg.coverage = Some(Arc::new(CoverageContexts::default()));

        assert_eq!(
            names(&cfg),
            vec![
                "experimental",
                "parity",
                "operator",
                "shard",
                "sample",
                "coverage"
            ],
        );
    }

    #[test]
    fn enabled_subset_is_a_canonical_subsequence() {
        // Property-style: for any partial-enable config, the emitted
        // names appear as a subsequence of CANONICAL_ORDER. Exhaustively
        // tries each combination of the opt-in flags (the experimental
        // gate is always-on so it's not in the toggle list).
        let flag_setters: Vec<fn(&mut Config)> = vec![
            |c| c.ops_allow = Some(HashSet::from([Operator::ArithOpSwap])),
            |c| c.shard = Some((1, 2)),
            |c| c.sample_ratio = Some(0.5),
            |c| c.coverage = Some(Arc::new(CoverageContexts::default())),
        ];

        let n = flag_setters.len();
        for mask in 0..(1u32 << n) {
            let mut cfg = empty_config();
            for (i, setter) in flag_setters.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    setter(&mut cfg);
                }
            }
            let got = names(&cfg);
            assert!(
                is_subsequence(&got, CANONICAL_ORDER),
                "mask={mask:#b}: emitted {got:?} is not a subsequence of \
                 canonical order {CANONICAL_ORDER:?}"
            );
        }
    }

    #[test]
    fn canonical_order_is_strictly_decreasing_in_cost_category() {
        // Documentation invariant: the canonical list itself must group
        // filters by cost category — cheap selection (experimental,
        // operator, shard, sample), then git/coverage-driven (diff,
        // since, coverage), then external-process (ruff, ty), then tce
        // (a `compile()` subprocess per mutant — the most expensive, so it
        // runs after ty's cull). If someone edits CANONICAL_ORDER, this
        // asserts they kept the buckets in the right order.
        fn bucket(name: &str) -> u8 {
            match name {
                "experimental" | "parity" | "operator" | "shard" | "sample" => 0,
                "diff-only" | "since" | "coverage" => 1,
                "ruff" | "ty" => 2,
                "tce" => 3,
                _ => panic!("unknown filter name in canonical order: {name}"),
            }
        }
        let mut last = 0u8;
        for name in CANONICAL_ORDER {
            let b = bucket(name);
            assert!(
                b >= last,
                "canonical order regresses at `{name}`: bucket {b} after bucket {last}"
            );
            last = b;
        }
    }

    #[test]
    fn canonical_order_has_no_duplicates() {
        let mut seen = HashSet::new();
        for name in CANONICAL_ORDER {
            assert!(
                seen.insert(name),
                "duplicate filter name in CANONICAL_ORDER: {name}"
            );
        }
    }
}

#[cfg(test)]
mod filter_timeout_tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn fast_command_returns_captured_output() {
        // A command that finishes well under the timeout returns Some(Output)
        // with its stdout captured — the happy path the filters depend on.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf hello"]);
        let out = run_filter_with_timeout(cmd)
            .expect("helper must not error")
            .expect("fast command must not time out");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }

    #[test]
    #[cfg(unix)]
    fn nonzero_exit_is_still_some() {
        // ty/ruff exit non-zero when they find diagnostics; the helper must
        // return Some (not None), since None is reserved for a timeout bypass.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 1"]);
        let out = run_filter_with_timeout(cmd).unwrap();
        assert!(
            out.is_some(),
            "non-timeout exit must yield Some, not a bypass"
        );
        assert!(!out.unwrap().status.success());
    }
}

#[cfg(test)]
mod patched_tempfile_tests {
    use super::*;

    /// True iff `stem` is a valid Python module name: first char a letter or
    /// underscore, rest alphanumeric or underscore. A hyphen fails this — which
    /// is the whole point of the check (ruff N999 / ty module-name lints).
    fn is_valid_module_name(stem: &str) -> bool {
        let mut chars = stem.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    #[test]
    fn temp_lands_next_to_original() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("mymod.py");
        let tmp = patched_tempfile(&original, "x = 1\n").unwrap();
        assert_eq!(
            tmp.path().parent().unwrap(),
            dir.path(),
            "temp file must sit in the original's directory for config/import symmetry"
        );
        assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), "x = 1\n");
    }

    #[test]
    fn temp_filename_is_a_valid_python_module_name() {
        // Regression guard: a hyphenated temp name trips module-name lints on
        // the mutant but not the baseline, asymmetrically inflating the count.
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("mymod.py");
        let tmp = patched_tempfile(&original, "x = 1\n").unwrap();
        let stem = tmp.path().file_stem().unwrap().to_str().unwrap();
        assert!(
            is_valid_module_name(stem),
            "temp module name `{stem}` is not a valid Python identifier"
        );
        assert!(
            stem.starts_with("mymod_fermut_"),
            "temp stem `{stem}` should derive from the original's stem"
        );
    }

    #[test]
    fn falls_back_when_source_dir_unwritable() {
        // Regression guard: a non-existent parent dir stands in for a
        // read-only source tree (installed package, ro mount, CI cache).
        // The old `.tempfile_in(parent)?` aborted the whole filter phase;
        // now we degrade to the system temp dir instead.
        let original = Path::new("/no/such/fermut/dir/mymod.py");
        let tmp = patched_tempfile(original, "y = 2\n")
            .expect("must fall back to system temp dir, not error");
        assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), "y = 2\n");
        let stem = tmp.path().file_stem().unwrap().to_str().unwrap();
        assert!(is_valid_module_name(stem), "fallback name `{stem}` invalid");
    }

    #[test]
    fn handles_bare_relative_filename() {
        // `Path::new("bare.py").parent()` is `Some("")` — the empty-parent
        // guard must route this to `.` (or fallback) without panicking.
        let tmp = patched_tempfile(Path::new("bare.py"), "z = 3\n").unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path()).unwrap(), "z = 3\n");
    }
}
