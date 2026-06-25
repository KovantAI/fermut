//! Top-level orchestration.
//!
//! Pipeline: **collect mutations → build filter chain → hash source files
//! once → parallel evaluate (filter chain → cache lookup → runner → cache
//! insert) → save cache → return `Report`**. Parallelism is rayon-driven; the
//! cache is shared via `Mutex<Cache>` (cheap because reads dominate).

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;
use tracing::{info, warn};

use crate::ast_hash::{self, ScopeMap};
use crate::cache::Cache;
use crate::config::{CacheScope, Config, RunnerKind};
use crate::equiv::{EquivPipeline, EquivVerdict};
use crate::filter::{self, Filter};
use crate::history::HistoryEntry;
use crate::mutator::{self, Mutant};
use crate::report::{MutantOutcome, Report};
use crate::runner::{self, Runner};
use crate::sync::lock_recover;

/// Top-level orchestration: parse → mutate → filter chain → pytest → report.
///
/// Returns the final `Report` plus the `HistoryEntry` that was built for
/// this run (when `cfg.history` is on). The entry is returned regardless
/// of whether the on-disk append succeeded so callers — the regression
/// gate, the markdown trend block — can reason about *this* run without
/// reloading the history file. If append fails the file still lacks the
/// entry, but the in-memory `Some(entry)` keeps gate decisions correct
/// in the exact CI environments (read-only FS, full disk) where
/// `--fail-on-regression` is most likely the hard gate.
pub fn run(cfg: &Config) -> Result<(Report, Option<HistoryEntry>)> {
    let started = Instant::now();
    info!(path = %cfg.source_root.display(), "collecting mutations");
    let mutants: Vec<Mutant> = mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)
        .with_context(|| format!("collecting mutations under {}", cfg.source_root.display()))?;
    info!(count = mutants.len(), "mutations generated");

    let filters = filter::build_chain(cfg)?;
    let runner = runner::build(cfg);

    // Pre-flight: confirm the unmutated suite is green before spending time
    // mutating. A red suite makes every covered mutant exit non-zero, which
    // the runner counts as "killed" — yielding a confidently-wrong score near
    // 100%. Skip when there's nothing to mutate or the user opted out.
    if cfg.verify_baseline && !mutants.is_empty() {
        info!("verifying baseline: running the unmutated test suite");
        match runner.baseline()? {
            runner::BaselineStatus::Passed => info!("baseline suite is green"),
            runner::BaselineStatus::Failed { output } => {
                return Err(anyhow::anyhow!(
                    "baseline test suite is not green — aborting before mutation.\n\n\
                     A failing or erroring suite makes every covered mutant exit \
                     non-zero, which fermut counts as \"killed\", producing a \
                     falsely high mutation score. Fix the suite (run your tests \
                     until they pass) or, if you accept the risk, re-run with \
                     --no-verify-baseline.\n\n\
                     --- test output (tail) ---\n{output}"
                ));
            }
        }
    }

    let file_hashes = hash_unique_files(&mutants);
    let scope_maps = if matches!(cfg.cache_scope, CacheScope::Scope) {
        build_scope_maps(&mutants)
    } else {
        HashMap::new()
    };
    let file_sources = if cfg.equiv_detect {
        load_unique_files(&mutants)
    } else {
        HashMap::new()
    };
    let equiv = if cfg.equiv_detect {
        Some(EquivPipeline::default_pipeline())
    } else {
        None
    };
    let scope_prefix = compute_scope_prefix(cfg);
    let cache = if cfg.cache {
        Mutex::new(Cache::load(&cfg.cache_path))
    } else {
        Mutex::new(Cache::default())
    };

    let progress = make_progress_bar(mutants.len() as u64);
    let pool = build_pool(cfg.jobs)?;
    let outcomes: Vec<MutantOutcome> = pool.install(|| {
        mutants
            .par_iter()
            .map(|m| {
                let outcome = evaluate(
                    m,
                    cfg,
                    &filters,
                    runner.as_ref(),
                    &cache,
                    &file_hashes,
                    &scope_maps,
                    &scope_prefix,
                    equiv.as_ref(),
                    &file_sources,
                );
                if let Some(pb) = &progress {
                    pb.inc(1);
                }
                outcome
            })
            .collect()
    });
    if let Some(pb) = &progress {
        pb.finish_and_clear();
    }

    if cfg.cache {
        let guard = lock_recover(&cache);
        let dropped = guard.dropped_on_load();
        let entries = guard.len();
        if let Err(e) = guard.save(&cfg.cache_path) {
            warn!(path = %cfg.cache_path.display(), error = %e, "failed to save cache");
        }
        // One-line end-of-run summary so users (and CI log scrapers) see
        // the integrity outcome without grepping every `warn!` line. A
        // non-zero `dropped` here is the signal that someone fed us a
        // tampered or stale cache.
        info!(
            entries,
            dropped_on_load = dropped,
            "cache: post-run summary"
        );
    }

    let report = Report::new(outcomes);

    let entry = if cfg.history {
        let duration_ms = u64::try_from(started.elapsed().as_millis()).ok();
        let hash = crate::history::config_hash(cfg);
        let entry = HistoryEntry::from_report(&report, &cfg.source_root, duration_ms, Some(hash));
        if let Err(e) = crate::history::append(&cfg.history_path, &entry) {
            // Never let history-log IO sink the run — `fermut run` already
            // has a final report; trend tracking is observational. The
            // entry still flows back to the caller so the regression gate
            // and trend block compare against the run that actually
            // happened, not against whatever the file happens to hold.
            warn!(path = %cfg.history_path.display(), error = %e, "failed to append history entry");
        }
        Some(entry)
    } else {
        None
    };

    Ok((report, entry))
}

#[allow(clippy::too_many_arguments)]
fn evaluate(
    mutant: &Mutant,
    cfg: &Config,
    filters: &[Box<dyn Filter>],
    runner: &dyn Runner,
    cache: &Mutex<Cache>,
    file_hashes: &HashMap<PathBuf, String>,
    scope_maps: &HashMap<PathBuf, ScopeMap>,
    scope_prefix: &str,
    equiv: Option<&EquivPipeline>,
    file_sources: &HashMap<PathBuf, String>,
) -> MutantOutcome {
    // Filter chain first — skip decisions depend on current filter config,
    // so we can't satisfy them from cache.
    for f in filters {
        match f.admits(mutant) {
            Ok(true) => continue,
            Ok(false) => return MutantOutcome::skipped(mutant.clone(), f.name()),
            Err(e) => {
                warn!(filter = f.name(), error = %e, "filter errored; admitting mutant");
            }
        }
    }

    let scope = compute_mutant_scope(scope_prefix, cfg, mutant);
    let identity_hash = mutant_identity_hash(cfg, mutant, file_hashes, scope_maps);

    if let Some(hash) = &identity_hash {
        if let Some(cached) = lock_recover(cache).lookup(&mutant.id, hash, &scope) {
            // Translate the cached verdict against the current detector
            // setting before returning. The cache stores the post-equiv
            // outcome, but `maybe_remap_equivalent` is invertible:
            // Survived↔Equivalent flip based on whether `equiv` is Some/None.
            // Lets `--no-equiv-detect` toggle take effect without busting
            // the cache. Detector rule changes are *not* covered — those
            // need a scope-prefix bump.
            return maybe_remap_equivalent(cached, equiv, file_sources);
        }
    }

    let outcome = runner
        .run(mutant)
        .unwrap_or_else(|e| MutantOutcome::error(mutant.clone(), e.to_string()));

    let outcome = maybe_remap_equivalent(outcome, equiv, file_sources);

    if let Some(hash) = identity_hash {
        lock_recover(cache).insert(mutant.id.clone(), hash, scope, outcome.clone());
    }

    outcome
}

/// The cache-key file-identity hash for `mutant`. In `CacheScope::Scope`
/// mode this is the per-enclosing-scope composite; otherwise it's the
/// file-AST hash. Returns `None` when no hash is available for the file
/// (e.g., I/O error during the eager hash pass) so the caller skips the
/// cache for that mutant.
fn mutant_identity_hash(
    cfg: &Config,
    mutant: &Mutant,
    file_hashes: &HashMap<PathBuf, String>,
    scope_maps: &HashMap<PathBuf, ScopeMap>,
) -> Option<String> {
    if matches!(cfg.cache_scope, CacheScope::Scope) {
        if let Some(map) = scope_maps.get(&mutant.file) {
            return Some(map.scope_hash_for(mutant.range));
        }
    }
    file_hashes.get(&mutant.file).cloned()
}

/// Translate cached outcomes against the current detector setting.
///
/// - Detector enabled + cached `Survived` proved equivalent → `Equivalent`.
/// - Detector disabled + cached `Equivalent` → demoted back to `Survived`.
///   Required so toggling `--no-equiv-detect` mid-cache reflects the user's
///   intent; the original verdict was a derivation, not a runner observation.
/// - All other shapes pass through.
///
/// `LikelyEquivalent` verdicts are *not* auto-applied — they need a human
/// to inspect and add an inline-ignore marker.
fn maybe_remap_equivalent(
    outcome: MutantOutcome,
    equiv: Option<&EquivPipeline>,
    file_sources: &HashMap<PathBuf, String>,
) -> MutantOutcome {
    let Some(pipeline) = equiv else {
        if let MutantOutcome::Equivalent { mutant, .. } = outcome {
            return MutantOutcome::Survived { mutant };
        }
        return outcome;
    };
    let MutantOutcome::Survived { mutant } = outcome else {
        return outcome;
    };
    let Some(source) = file_sources.get(&mutant.file) else {
        return MutantOutcome::Survived { mutant };
    };
    match pipeline.classify(&mutant, source) {
        EquivVerdict::ProvablyEquivalent {
            reason,
            source: src,
        } => MutantOutcome::equivalent(mutant, reason, src),
        _ => MutantOutcome::Survived { mutant },
    }
}

/// Hash run-wide settings that affect outcomes without changing source bytes.
/// Mixed into every cached entry's scope so cache hits require the same shape.
fn compute_scope_prefix(cfg: &Config) -> String {
    let mut h = Sha256::new();
    // v4: the scope prefix now folds in a fingerprint of the test-suite tree
    // (see `|tests=` below). Bumping the tag isn't strictly required — adding
    // a new field already shifts the hash — but it documents the composition
    // change and keeps the intent legible.
    //
    // v3: cache-scope mode now affects how the file-identity hash is
    // computed. Mix the mode into the scope prefix so a `file`-mode entry
    // can't satisfy a `scope`-mode lookup (and vice versa) — the two modes
    // produce different identity hashes for the same source.
    // v5: folds in the resolved Python interpreter (`|python=` below) — a
    // different interpreter (or its venv's package set) can flip a mutant's
    // verdict, so a `--python` change must not reuse another interpreter's
    // cached outcomes.
    h.update(b"v5");
    h.update(b"|cache_scope=");
    h.update(format!("{:?}", cfg.cache_scope).as_bytes());
    h.update(b"|runner=");
    h.update(runner_cache_tag(cfg.runner).as_bytes());
    h.update(b"|python=");
    match &cfg.python {
        Some(p) => h.update(p.as_os_str().as_encoded_bytes()),
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
    for a in &cfg.pytest_args {
        h.update(b"|");
        h.update(a.as_bytes());
    }
    h.update(b"|cov=");
    h.update(if cfg.coverage.is_some() {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
    // Test-suite content. A mutant's outcome depends on the tests as much as
    // on the source: editing a test (without touching the source AST) can turn
    // a survivor into a kill, but the source `file_hash` and the rest of the
    // scope are unchanged. Without this, an agent's "add the killing test,
    // re-run" loop keeps reading the stale "survived" verdict from the cache
    // and never sees its fix land. Folding the tree fingerprint in means any
    // test edit invalidates every cached outcome — coarse, but correct: a new
    // test can kill more than one survivor, so re-evaluating all of them is the
    // right semantics, not just the safe one.
    h.update(b"|tests=");
    h.update(test_suite_fingerprint(&cfg.tests_path()).as_bytes());
    hex::encode(h.finalize())
}

/// Content fingerprint of the test-suite tree rooted at `tests_path`.
///
/// Walks the tree the same way the worker mirror does — `.gitignore` honored,
/// hidden directories and `__pycache__` pruned, compiled bytecode skipped —
/// and folds each surviving file's relative path plus a content hash into one
/// stable digest. Path-sorted so the result is independent of directory
/// iteration order. A missing or empty tree hashes to a constant (the empty
/// digest), which is fine: it still differs from any populated tree.
fn test_suite_fingerprint(tests_path: &Path) -> String {
    let mut files: Vec<(PathBuf, [u8; 32])> = Vec::new();
    let walker = ignore::WalkBuilder::new(tests_path)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .require_git(false)
        .parents(false)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let name = entry.file_name().to_str();
            !(is_dir && name.is_some_and(|n| n.starts_with('.') || n == "__pycache__"))
        })
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // Stale compiled bytecode outside `__pycache__` never affects the run.
        if path.extension().is_some_and(|e| e == "pyc" || e == "pyo") {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            // Unreadable file: skip it rather than abort. Its absence from the
            // fingerprint is conservative — at worst a cache entry lives one
            // run too long, never the inverse.
            continue;
        };
        let mut fh = Sha256::new();
        fh.update(&bytes);
        let rel = path.strip_prefix(tests_path).unwrap_or(path).to_path_buf();
        files.push((rel, fh.finalize().into()));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (rel, fh) in &files {
        h.update(rel.to_string_lossy().as_bytes());
        h.update(b"\0");
        h.update(fh);
    }
    hex::encode(h.finalize())
}

/// Stable cache tag for `RunnerKind`. Hard-coded so renaming a variant
/// is a deliberate cache-busting change, not a silent one.
fn runner_cache_tag(kind: RunnerKind) -> &'static str {
    match kind {
        RunnerKind::Pytest => "pytest",
        RunnerKind::Unittest => "unittest",
    }
}

/// Per-mutant final scope: prefix + sorted list of selected test ids when
/// coverage-driven selection is on. Without coverage selection, every mutant
/// shares the prefix.
fn compute_mutant_scope(prefix: &str, cfg: &Config, mutant: &Mutant) -> String {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    if let Some(ctx) = &cfg.coverage {
        if let Some(tests) = ctx.tests_for(&mutant.file, mutant.line) {
            let mut sorted: Vec<&String> = tests.iter().collect();
            sorted.sort();
            for t in sorted {
                h.update(b"|");
                h.update(t.as_bytes());
            }
        }
    }
    hex::encode(h.finalize())
}

/// Read each unique source file once. Used by the equiv detector so
/// classifications don't re-read the file per mutant.
fn load_unique_files(mutants: &[Mutant]) -> HashMap<PathBuf, String> {
    let mut out = HashMap::new();
    for m in mutants {
        if out.contains_key(&m.file) {
            continue;
        }
        match std::fs::read_to_string(&m.file) {
            Ok(s) => {
                out.insert(m.file.clone(), s);
            }
            Err(e) => {
                warn!(file = %m.file.display(), error = %e, "source read failed; equiv detector disabled for this file");
            }
        }
    }
    out
}

/// Parse every unique source file once and return its [`ScopeMap`]. Files
/// that fail to parse or read are simply absent from the map; consumers fall
/// back to the file-AST hash for those.
fn build_scope_maps(mutants: &[Mutant]) -> HashMap<PathBuf, ScopeMap> {
    let mut out = HashMap::new();
    for m in mutants {
        if out.contains_key(&m.file) {
            continue;
        }
        match ast_hash::scope_map_from_file(&m.file) {
            Ok(Some(map)) => {
                out.insert(m.file.clone(), map);
            }
            Ok(None) => {
                warn!(file = %m.file.display(), "scope-map parse failed; falling back to file hash for this file");
            }
            Err(e) => {
                warn!(file = %m.file.display(), error = %e, "scope-map read failed; falling back to file hash for this file");
            }
        }
    }
    out
}

fn hash_unique_files(mutants: &[Mutant]) -> HashMap<PathBuf, String> {
    let mut out = HashMap::new();
    for m in mutants {
        if !out.contains_key(&m.file) {
            match ast_hash::hash_file_ast(&m.file) {
                Ok(h) => {
                    out.insert(m.file.clone(), h);
                }
                Err(e) => {
                    warn!(file = %m.file.display(), error = %e, "hash failed; cache disabled for this file");
                }
            }
        }
    }
    out
}

fn make_progress_bar(total: u64) -> Option<ProgressBar> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:30.cyan/blue}] {pos}/{len} mutants",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    pb.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    Some(pb)
}

/// Worker-thread stack size. rayon workers default to 2 MiB (vs the main
/// thread's 8 MiB), so recursive AST work — ruff's source-order walk and the
/// equivalent-mutant detector re-parsing a deeply-nested expression — can
/// overflow a worker stack on real-world files (e.g. more-itertools) while the
/// main-thread collection pass succeeds. Match the main thread at 8 MiB.
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn build_pool(jobs: Option<usize>) -> Result<rayon::ThreadPool> {
    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(j) = jobs {
        builder = builder.num_threads(j);
    }
    builder
        .stack_size(WORKER_STACK_BYTES)
        .thread_name(|i| format!("fermut-{i}"))
        .build()
        .context("building rayon pool")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::{TextRange, TextSize};

    fn arith_mutant(source: &str, original: &str, replacement: &str) -> Mutant {
        let start = source.find(original).unwrap();
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + original.len()) as u32),
            ),
            original: original.into(),
            replacement: replacement.into(),
            line: 1,
        }
    }

    #[test]
    fn remap_no_detector_passes_through() {
        let m = arith_mutant("return x + 0", "+", "-");
        let out = MutantOutcome::survived(m.clone());
        let result = maybe_remap_equivalent(out, None, &HashMap::new());
        assert!(matches!(result, MutantOutcome::Survived { .. }));
    }

    #[test]
    fn remap_no_detector_demotes_cached_equivalent_to_survived() {
        // Cache may hold an `Equivalent` outcome from a prior run with
        // detection enabled. When the current run has detection disabled
        // (equiv=None), the cached verdict no longer reflects user intent
        // and must be downgraded back to a plain survivor.
        let m = arith_mutant("return x + 0", "+", "-");
        let cached = MutantOutcome::equivalent(m.clone(), "stale reason", "stale-source");
        let result = maybe_remap_equivalent(cached, None, &HashMap::new());
        match result {
            MutantOutcome::Survived { mutant } => assert_eq!(mutant.id, m.id),
            other => panic!("expected Survived, got {other:?}"),
        }
    }

    #[test]
    fn remap_non_survived_is_untouched() {
        let m = arith_mutant("return x + 0", "+", "-");
        let killed = MutantOutcome::killed(m.clone());
        let pipeline = EquivPipeline::default_pipeline();
        let mut sources = HashMap::new();
        sources.insert(m.file.clone(), "return x + 0".to_string());
        let result = maybe_remap_equivalent(killed, Some(&pipeline), &sources);
        assert!(matches!(result, MutantOutcome::Killed { .. }));
    }

    #[test]
    fn remap_missing_source_keeps_survived() {
        let m = arith_mutant("return x + 0", "+", "-");
        let out = MutantOutcome::survived(m);
        let pipeline = EquivPipeline::default_pipeline();
        let result = maybe_remap_equivalent(out, Some(&pipeline), &HashMap::new());
        assert!(matches!(result, MutantOutcome::Survived { .. }));
    }

    #[test]
    fn remap_provable_arith_zero_via_pattern_rule_does_not_promote_to_equivalent() {
        // Layer 2 returns `LikelyEquivalent`, not `Provably`. The engine only
        // auto-promotes provable verdicts; likely ones stay Survived so a
        // human reviews them. Locks in that contract.
        let m = arith_mutant("def f():\n    return x + 0\n", "+", "-");
        let out = MutantOutcome::survived(m.clone());
        let pipeline = EquivPipeline::default_pipeline();
        let mut sources = HashMap::new();
        sources.insert(m.file.clone(), "def f():\n    return x + 0\n".to_string());
        let result = maybe_remap_equivalent(out, Some(&pipeline), &sources);
        assert!(matches!(result, MutantOutcome::Survived { .. }));
    }

    #[test]
    fn mutant_identity_hash_uses_scope_map_in_scope_mode() {
        // `bar`-side mutant: scope mode must produce a hash that survives a
        // body edit to `foo` (sibling top-level fn). File mode must not.
        use crate::config::CacheScope;
        let path = PathBuf::from("t.py");
        let src_a = "def foo():\n    return 1\n\ndef bar():\n    return 99\n";
        let src_b = "def foo():\n    return 42\n\ndef bar():\n    return 99\n";

        let map_a = ast_hash::compute_scope_map(src_a).unwrap();
        let map_b = ast_hash::compute_scope_map(src_b).unwrap();
        let mut maps_a = HashMap::new();
        maps_a.insert(path.clone(), map_a);
        let mut maps_b = HashMap::new();
        maps_b.insert(path.clone(), map_b);

        let mut files_a = HashMap::new();
        files_a.insert(path.clone(), ast_hash::hash_ast_source(src_a).unwrap());
        let mut files_b = HashMap::new();
        files_b.insert(path.clone(), ast_hash::hash_ast_source(src_b).unwrap());

        // Build a mutant pointing at the `99` literal in bar's body.
        let start = src_a.find("99").unwrap();
        let mut m = arith_mutant(src_a, "99", "100");
        m.file = path.clone();
        m.range = TextRange::new(
            TextSize::from(start as u32),
            TextSize::from((start + 2) as u32),
        );

        let scope_cfg = Config {
            cache_scope: CacheScope::Scope,
            ..base_test_config()
        };
        let file_cfg = Config {
            cache_scope: CacheScope::File,
            ..base_test_config()
        };

        // Scope mode: bar-side hash stable across edits to foo's body.
        let scope_a = mutant_identity_hash(&scope_cfg, &m, &files_a, &maps_a).unwrap();
        let scope_b = mutant_identity_hash(&scope_cfg, &m, &files_b, &maps_b).unwrap();
        assert_eq!(
            scope_a, scope_b,
            "scope-mode bar hash must survive foo edit"
        );

        // File mode: any AST change anywhere invalidates the hash.
        let file_a = mutant_identity_hash(&file_cfg, &m, &files_a, &maps_a).unwrap();
        let file_b = mutant_identity_hash(&file_cfg, &m, &files_b, &maps_b).unwrap();
        assert_ne!(file_a, file_b, "file-mode bar hash must change on foo edit");
    }

    #[test]
    fn test_suite_fingerprint_is_stable_and_tracks_content() {
        let tmp = tempfile::tempdir().unwrap();
        let tests = tmp.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(tests.join("test_a.py"), b"def test_a():\n    assert True\n").unwrap();

        let h1 = test_suite_fingerprint(&tests);
        let h2 = test_suite_fingerprint(&tests);
        assert_eq!(h1, h2, "fingerprint must be deterministic");

        // Editing a test body — same file, same path — must move the hash.
        std::fs::write(
            tests.join("test_a.py"),
            b"def test_a():\n    assert 1 == 1\n",
        )
        .unwrap();
        let h3 = test_suite_fingerprint(&tests);
        assert_ne!(h1, h3, "editing a test must change the fingerprint");

        // Adding a test file must also move it.
        std::fs::write(tests.join("test_b.py"), b"def test_b():\n    assert True\n").unwrap();
        let h4 = test_suite_fingerprint(&tests);
        assert_ne!(h3, h4, "adding a test must change the fingerprint");
    }

    #[test]
    fn test_suite_fingerprint_ignores_pycache_and_bytecode() {
        let tmp = tempfile::tempdir().unwrap();
        let tests = tmp.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(tests.join("test_a.py"), b"def test_a():\n    assert True\n").unwrap();
        let baseline = test_suite_fingerprint(&tests);

        // Stale bytecode under __pycache__ and a stray .pyc must not count —
        // they never affect what pytest runs.
        let pycache = tests.join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        std::fs::write(pycache.join("test_a.cpython-312.pyc"), b"\x00\x01junk").unwrap();
        std::fs::write(tests.join("test_a.pyc"), b"\x00\x01junk").unwrap();

        assert_eq!(
            baseline,
            test_suite_fingerprint(&tests),
            "compiled bytecode must not alter the fingerprint"
        );
    }

    #[test]
    fn scope_prefix_changes_when_a_test_is_edited() {
        // The core regression: editing a test (no source-AST change) must
        // invalidate cached per-mutant outcomes, or an agent's add-test/re-run
        // loop reads the stale pre-edit verdict forever.
        let tmp = tempfile::tempdir().unwrap();
        let tests = tmp.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(
            tests.join("test_calc.py"),
            b"def test_add():\n    assert add(1, 2) == 3\n",
        )
        .unwrap();

        let cfg = Config {
            source_root: tmp.path().to_path_buf(),
            tests: Some(tests.clone()),
            ..base_test_config()
        };
        let before = compute_scope_prefix(&cfg);
        assert_eq!(before, compute_scope_prefix(&cfg), "prefix must be stable");

        std::fs::write(
            tests.join("test_calc.py"),
            b"def test_add():\n    assert add(1, 2) == 3\n\ndef test_add_neg():\n    assert add(-1, 1) == 0\n",
        )
        .unwrap();
        let after = compute_scope_prefix(&cfg);
        assert_ne!(before, after, "editing tests must change the scope prefix");
    }

    /// Minimal `Config` for tests that exercise hash/scope logic without
    /// touching the runner, filters, or cache file. Only the fields read by
    /// the path under test are meaningful; the rest get safe defaults.
    fn base_test_config() -> Config {
        use crate::config::{CacheScope, IsolationMode, RunnerKind};
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
            ops_deny: Default::default(),
            diff_base: None,
            since: None,
            coverage_path: None,
            coverage: None,
            hypothesis_seed: None,
            pytest_args: Vec::new(),
            cache: false,
            cache_path: PathBuf::from(".fermut/cache.json"),
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
        }
    }

    #[test]
    fn remap_bytecode_identical_becomes_equivalent() {
        // `return None` → `return` is bytecode-identical under CPython, so
        // the bytecode-identity layer proves equivalence. Skips when no
        // python3 on PATH so the test isn't environment-fragile.
        if std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: python3 not available");
            return;
        }
        let source = "def f():\n    return None\n";
        let start = source.find("return None").unwrap();
        let m = Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ReturnValueToNone,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + "return None".len()) as u32),
            ),
            original: "return None".into(),
            replacement: "return".into(),
            line: 2,
        };
        let out = MutantOutcome::survived(m.clone());
        let pipeline = EquivPipeline::default_pipeline();
        let mut sources = HashMap::new();
        sources.insert(m.file.clone(), source.to_string());
        let result = maybe_remap_equivalent(out, Some(&pipeline), &sources);
        match result {
            MutantOutcome::Equivalent { source: src, .. } => {
                assert_eq!(src, "bytecode-identity");
            }
            other => panic!("expected Equivalent, got {other:?}"),
        }
    }
}
