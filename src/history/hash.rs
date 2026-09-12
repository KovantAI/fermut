//! Stable hashing of the run-shape config — see [`HistoryEntry::config_hash`].
//!
//! [`HistoryEntry::config_hash`]: super::HistoryEntry::config_hash

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::config::Config;

/// Feed a length-delimited byte field. Prefixing each variable-length,
/// user-controlled value with its length stops adjacent fields from
/// stream-colliding — otherwise a crafted git ref, glob, or pytest arg that
/// happened to contain a later field marker (`|shard=`, …) could hash
/// identically to a structurally different config.
fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Render `p` relative to `root` when it sits under it, else the path as-is.
/// Keeps the hash stable across checkouts (absolute paths vary per machine)
/// while still distinguishing a run pointed at a *different* suite/file.
fn rel_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// Stable hex digest of the run-shape config — see `HistoryEntry::config_hash`.
/// Format-versioned (`v2|…`) so we can rev the shape without colliding with
/// older entries; bump the prefix if the inputs ever change.
pub fn config_hash(cfg: &Config) -> String {
    let mut h = Sha256::new();
    // v2: folds in the mutant-universe / comparability inputs the score
    // actually depends on but v1 ignored — parity operators, diff scope
    // (`--since`/`--diff-only`), `--sample`, `--exclude`, `--shard`, the
    // ruff/ty filters, equivalent-mutant detection, and the test-suite /
    // coverage-file selection. These are exactly the
    // filter-chain selectors in `filter::build_chain` plus the engine's equiv
    // pass; each changes which mutants are tested or counted. Two runs with
    // different values here are NOT comparable, so a diff-scoped or sampled run
    // must no longer hash identically to a full run (which is what forced the
    // PR gate onto `--no-history` and the trend merge onto a manual `ci_scope`).
    h.update(b"v2|runner=");
    h.update(format!("{:?}", cfg.runner).as_bytes());
    h.update(b"|python=");
    match &cfg.python {
        Some(p) => feed(&mut h, p.as_os_str().as_encoded_bytes()),
        None => h.update(b"none"),
    }
    h.update(b"|timeout=");
    h.update(cfg.timeout_secs.to_le_bytes());
    h.update(b"|hseed=");
    if let Some(s) = cfg.hypothesis_seed {
        h.update(s.to_le_bytes());
    } else {
        h.update(b"none");
    }
    h.update(b"|args=");
    h.update((cfg.pytest_args.len() as u64).to_le_bytes());
    for a in &cfg.pytest_args {
        feed(&mut h, a.as_bytes());
    }
    h.update(b"|cov=");
    h.update(if cfg.coverage.is_some() {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    h.update(b"|exp=");
    h.update(if cfg.experimental {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    h.update(b"|ops=");
    if let Some(allow) = &cfg.ops_allow {
        let mut sorted: Vec<String> = allow.iter().map(|o| format!("{o:?}")).collect();
        sorted.sort();
        for o in sorted {
            h.update(b"|");
            h.update(o.as_bytes());
        }
    } else {
        h.update(b"all");
    }
    h.update(b"|deny=");
    let mut sorted: Vec<String> = cfg.ops_deny.iter().map(|o| format!("{o:?}")).collect();
    sorted.sort();
    for o in sorted {
        h.update(b"|");
        h.update(o.as_bytes());
    }
    // Parity operators expand the mutant set, so a parity run's score is not
    // comparable to a non-parity one.
    h.update(b"|parity=");
    h.update(if cfg.parity { &b"on"[..] } else { &b"off"[..] });
    // Diff scope. A run restricted to changed lines scores over a different
    // universe than a full run; `--since` and `--diff-only` are mutually
    // exclusive at the parser. The spec/base string is included so a full run
    // and a `--since origin/main` run get distinct hashes.
    h.update(b"|scope=");
    if let Some(spec) = &cfg.since {
        h.update(b"since:");
        feed(&mut h, spec.as_bytes());
    } else if let Some(base) = &cfg.diff_base {
        h.update(b"diff:");
        feed(&mut h, base.as_bytes());
    } else {
        h.update(b"full");
    }
    // Sampling tests only a fraction of mutants, so a sampled score is a noisy
    // estimate, not comparable to a full run. Ratio + seed both matter: a
    // different seed selects a different subset.
    h.update(b"|sample=");
    match cfg.sample_ratio {
        Some(r) => {
            h.update(b"ratio:");
            h.update(r.to_bits().to_le_bytes());
            h.update(b":seed:");
            h.update(cfg.sample_seed.unwrap_or(0).to_le_bytes());
        }
        None => h.update(b"none"),
    }
    // Excluded paths change which files produce mutants at all. Sorted so the
    // hash is order-independent.
    h.update(b"|exclude=");
    if cfg.exclude.is_empty() {
        h.update(b"none");
    } else {
        let mut ex = cfg.exclude.clone();
        ex.sort();
        h.update((ex.len() as u64).to_le_bytes());
        for g in ex {
            feed(&mut h, g.as_bytes());
        }
    }
    // Sharding partitions the mutant set (`ShardFilter`, a hash modulo): a
    // `--shard i/n` run scores over 1/n of the universe, not comparable to a
    // full run. Index + total both matter — different shards test different
    // mutants.
    h.update(b"|shard=");
    match cfg.shard {
        Some((index, total)) => {
            h.update(index.to_le_bytes());
            h.update(b"/");
            h.update(total.to_le_bytes());
        }
        None => h.update(b"none"),
    }
    // The ruff/ty filters drop mutants those tools reject before they are ever
    // run, shrinking the denominator. A `--ruff`/`--ty` run is not comparable
    // to one without.
    h.update(b"|ruff=");
    h.update(if cfg.ruff_filter {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    h.update(b"|ty=");
    h.update(if cfg.ty_filter {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    // The TCE pre-filter drops provably-equivalent mutants before they run,
    // shrinking the denominator just like ruff/ty. A `--tce` run scores over a
    // different mutant universe than one without it.
    h.update(b"|tce=");
    h.update(if cfg.tce { &b"on"[..] } else { &b"off"[..] });
    // Equivalent-mutant detection flips otherwise-`Survived` mutants to
    // `Equivalent`, removing them from the killable denominator and moving the
    // score. On by default; `--no-equiv-detect` changes the universe.
    h.update(b"|equiv=");
    h.update(if cfg.equiv_detect {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    // Test-suite / coverage-file selection is run shape: pointing `--tests` at
    // a different suite (or `--coverage` at a different file) scores over a
    // different mutant universe, so those runs are not comparable. Hashed
    // relative to `source_root` so the digest stays stable across checkouts
    // whose absolute paths differ; `source_root` itself is deliberately NOT
    // hashed for the same reason (it's project/checkout identity, not shape).
    h.update(b"|tests=");
    feed(
        &mut h,
        rel_to(&cfg.source_root, &cfg.tests_path()).as_bytes(),
    );
    h.update(b"|covpath=");
    match &cfg.coverage_path {
        Some(p) => feed(&mut h, rel_to(&cfg.source_root, p).as_bytes()),
        None => h.update(b"none"),
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Minimal `Config` for exercising `config_hash`. Only the fields the hash
    /// reads are meaningful; the rest are inert defaults.
    fn cfg() -> Config {
        use crate::config::{CacheScope, IsolationMode, RunnerKind};
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
            ops_deny: Default::default(),
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
            unittest_pattern: None,
            isolation: IsolationMode::Auto,
            equiv_detect: false,
            cache_scope: CacheScope::File,
            fail_under: None,
            exclude: Vec::new(),
            verify_baseline: false,
            baseline_timeout_secs: 300,
            max_time_secs: None,
            record_kill_sets: None,
        }
    }

    #[test]
    fn config_hash_is_stable_for_identical_config() {
        assert_eq!(config_hash(&cfg()), config_hash(&cfg()));
    }

    #[test]
    fn config_hash_distinguishes_scope_sample_exclude_parity() {
        let base = config_hash(&cfg());

        // Diff scope: full vs --since vs --diff-only, and two different --since
        // specs, must all differ — a diff-scoped run is no longer mistaken for
        // a full run in the trend store.
        let since = config_hash(&Config {
            since: Some("origin/main".into()),
            ..cfg()
        });
        let since2 = config_hash(&Config {
            since: Some("v1.2.0".into()),
            ..cfg()
        });
        let diff = config_hash(&Config {
            diff_base: Some("origin/main".into()),
            ..cfg()
        });
        assert_ne!(base, since, "full vs --since must differ");
        assert_ne!(since, since2, "different --since specs must differ");
        assert_ne!(base, diff, "full vs --diff-only must differ");
        assert_ne!(since, diff, "--since vs --diff-only must differ");

        // Sampling: presence and seed both change comparability.
        let s1 = config_hash(&Config {
            sample_ratio: Some(0.5),
            sample_seed: Some(0),
            ..cfg()
        });
        let s2 = config_hash(&Config {
            sample_ratio: Some(0.5),
            sample_seed: Some(1),
            ..cfg()
        });
        assert_ne!(base, s1, "sampled run must differ from full");
        assert_ne!(s1, s2, "different sample seeds must differ");

        // Exclude: presence and content, order-independent.
        let e1 = config_hash(&Config {
            exclude: vec!["a/**".into()],
            ..cfg()
        });
        let e_ab = config_hash(&Config {
            exclude: vec!["a/**".into(), "b/**".into()],
            ..cfg()
        });
        let e_ba = config_hash(&Config {
            exclude: vec!["b/**".into(), "a/**".into()],
            ..cfg()
        });
        assert_ne!(base, e1, "an exclude must differ from none");
        assert_ne!(e1, e_ab, "different exclude sets must differ");
        assert_eq!(e_ab, e_ba, "exclude order must not matter");

        // Parity operators expand the mutant set.
        let parity = config_hash(&Config {
            parity: true,
            ..cfg()
        });
        assert_ne!(base, parity, "parity run must differ from non-parity");

        // Sharding partitions the mutant set; index and total both matter.
        let sh1 = config_hash(&Config {
            shard: Some((1, 4)),
            ..cfg()
        });
        let sh2 = config_hash(&Config {
            shard: Some((2, 4)),
            ..cfg()
        });
        let sh_total = config_hash(&Config {
            shard: Some((1, 2)),
            ..cfg()
        });
        assert_ne!(base, sh1, "sharded run must differ from full");
        assert_ne!(sh1, sh2, "different shard indices must differ");
        assert_ne!(sh1, sh_total, "different shard totals must differ");

        // ruff/ty filters and equiv detection each change the denominator.
        let ruff = config_hash(&Config {
            ruff_filter: true,
            ..cfg()
        });
        let ty = config_hash(&Config {
            ty_filter: true,
            ..cfg()
        });
        let equiv = config_hash(&Config {
            equiv_detect: true,
            ..cfg()
        });
        let tce = config_hash(&Config { tce: true, ..cfg() });
        assert_ne!(base, ruff, "ruff-filtered run must differ");
        assert_ne!(base, ty, "ty-filtered run must differ");
        assert_ne!(ruff, ty, "ruff vs ty must differ");
        assert_ne!(base, equiv, "equiv-detect toggle must differ");
        assert_ne!(base, tce, "tce-filtered run must differ");

        // Test-suite / coverage-file selection: pointing at a different suite
        // or coverage file is a shape change, so the hash must differ.
        let tests = config_hash(&Config {
            tests: Some(PathBuf::from("other_tests")),
            ..cfg()
        });
        let covpath = config_hash(&Config {
            coverage_path: Some(PathBuf::from("cov.json")),
            ..cfg()
        });
        assert_ne!(base, tests, "different --tests must differ");
        assert_ne!(base, covpath, "a --coverage file must differ from none");
    }

    #[test]
    fn config_hash_ignores_sample_seed_without_ratio() {
        // `sample_seed` is only read inside the `Some(ratio)` arm; with no
        // ratio the seed selects nothing, so it must not move the hash.
        let no_seed = config_hash(&cfg());
        let with_seed = config_hash(&Config {
            sample_seed: Some(42),
            ..cfg()
        });
        assert_eq!(no_seed, with_seed, "seed without a ratio must be inert");
    }

    #[test]
    fn config_hash_tests_path_is_relative_to_source_root() {
        // Absolute paths vary per checkout; the hash must depend only on the
        // suite's location relative to source_root, not the checkout prefix.
        let a = config_hash(&Config {
            source_root: PathBuf::from("/checkout-a"),
            tests: Some(PathBuf::from("/checkout-a/tests")),
            ..cfg()
        });
        let b = config_hash(&Config {
            source_root: PathBuf::from("/checkout-b"),
            tests: Some(PathBuf::from("/checkout-b/tests")),
            ..cfg()
        });
        assert_eq!(a, b, "same relative suite across checkouts must match");
    }
}
