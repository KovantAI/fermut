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
pub mod ty;
pub mod ty_embedded;

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::mutator::{Mutant, Operator};

pub trait Filter: Send + Sync {
    fn name(&self) -> &'static str;
    /// Returns Ok(true) if the mutant should be tested, Ok(false) to drop it.
    fn admits(&self, mutant: &Mutant) -> Result<bool>;
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
        chain.push(Box::new(ruff::RuffFilter::new()?));
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

    Ok(chain)
}

/// Same as `build_chain` but skips filters that need a project root / external
/// binary, for use in the `list` subcommand. Currently identical — kept as a
/// hook so we can later opt out of expensive filters here without code churn.
pub fn build_chain_for_list(cfg: &Config) -> Result<Vec<Box<dyn Filter>>> {
    build_chain(cfg)
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

/// Re-export for callers that need a default working directory.
pub fn default_cwd() -> &'static Path {
    Path::new(".")
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
            cache_path: PathBuf::from(".fermut/cache.json"),
            smart_order: false,
            kill_order_path: PathBuf::from(".fermut/kill-order.json"),
            history: false,
            history_path: PathBuf::from(".fermut/history.jsonl"),
            sample_ratio: None,
            sample_seed: None,
            shard: None,
            runner: RunnerKind::Pytest,
            python: None,
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
        // since, coverage), then external-process (ruff, ty). If someone
        // edits CANONICAL_ORDER, this asserts they kept the buckets in
        // the right order.
        fn bucket(name: &str) -> u8 {
            match name {
                "experimental" | "parity" | "operator" | "shard" | "sample" => 0,
                "diff-only" | "since" | "coverage" => 1,
                "ruff" | "ty" => 2,
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
