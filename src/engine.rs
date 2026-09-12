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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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

/// Filter name recorded on mutants left untested when `--max-time` runs out.
/// Callers (the PR gate, `skipped_by_filter`) match on this to tell a
/// budget-truncated run apart from a coverage/shard skip.
pub const TIME_BUDGET_FILTER: &str = "time-budget";

/// Reorder `mutants` in place, highest-value first, for a budgeted run.
///
/// v1 value signal: a mutant is "covered" when the coverage context maps it to
/// at least one test. Covered mutants sort ahead of uncovered ones so the
/// budget is spent on mutants that can actually be reached — an uncovered
/// mutant would be coverage-skipped regardless. `sort_by_cached_key` is stable,
/// so generation order is preserved within each tier and the ordering is fully
/// deterministic. With no coverage context every mutant tiers equal and the
/// order is unchanged.
///
/// Note: the worker pool (`par_iter`) splits the slice across threads, so this
/// biases *start* order rather than strictly serializing covered-before-
/// uncovered. In practice uncovered mutants are cheap coverage-skips that drain
/// fast, so the expensive budget still lands on covered mutants; the ordering
/// makes that the common case, not a hard guarantee.
fn order_by_value(mutants: &mut [Arc<Mutant>], cfg: &Config) {
    let Some(cov) = cfg.coverage.as_ref() else {
        return;
    };
    // key 0 = covered (run first), 1 = uncovered. `sort_by_cached_key` computes
    // the coverage lookup once per mutant (O(n)) rather than the O(n log n)
    // lookups a bare `sort_by_key` comparator would run.
    mutants.sort_by_cached_key(|m| u8::from(cov.tests_for_mutant(m).is_none_or(<[_]>::is_empty)));
}

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
    // Wrap each mutant in an `Arc` once, here, so the per-mutant testing loop
    // (and the outcomes it embeds them in) shares them by refcount instead of
    // deep-copying the four heap fields on every outcome construction, cache
    // insert, and cache-hit lookup. Nothing mutates a `Mutant` after collection
    // — only the Vec is reordered — so the shared `Arc` needs no interior mut.
    let mut mutants: Vec<Arc<Mutant>> = mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)
        .with_context(|| format!("collecting mutations under {}", cfg.source_root.display()))?
        .into_iter()
        .map(Arc::new)
        .collect();
    info!(count = mutants.len(), "mutations generated");

    // `--sample` scales only the TESTING phase: every mutant above was still
    // generated, and the `ty` pre-filter (below) still type-checks the whole
    // universe. That work is a fixed floor under every run, so wall-clock does
    // NOT shrink by the sample ratio — sizing a shard by `1/ratio` overshoots.
    if let Some(ratio) = cfg.sample_ratio {
        info!(
            ratio,
            generated = mutants.len(),
            "sampling the testing phase only — generation and the ty pre-filter still \
             cover all {} mutants, a fixed cost the ratio does not reduce",
            mutants.len()
        );
    }

    // With a testing-phase budget, evaluate the highest-value mutants first so
    // the ones that survive the deadline are the least informative. v1 signal:
    // covered mutants (a coverage-selected test can actually reach them) ahead
    // of uncovered ones, which the coverage filter would skip anyway. Stable so
    // generation order is preserved within each tier — and left untouched
    // entirely when no budget is set, so non-budgeted runs are byte-identical.
    if cfg.max_time_secs.is_some() {
        order_by_value(&mut mutants, cfg);
    }

    // Everything read-only for the testing phase: filter chain, runner, cache,
    // per-file artifacts, equiv pipeline, deadline — plus the baseline check.
    let state = prepare(cfg, &mutants)?;

    let ctx = EvalCtx {
        cfg,
        filters: &state.filters,
        runner: state.runner.as_ref(),
        cache: &state.cache,
        file_hashes: &state.file_hashes,
        scope_maps: &state.scope_maps,
        scope_prefix: &state.scope_prefix,
        equiv: state.equiv.as_ref(),
        file_sources: &state.file_sources,
        deadline: state.deadline,
    };
    let outcomes: Vec<MutantOutcome> = evaluate_all(cfg, &ctx, &mutants)?;

    Ok(persist(
        cfg,
        started,
        state.runner.as_ref(),
        &state.cache,
        state.deadline,
        outcomes,
    ))
}

/// Read-only state assembled before the testing phase and borrowed by every
/// mutant's [`evaluate`]. Owned here (not in [`run`]) so [`prepare`] can build
/// it in one place; [`EvalCtx`] holds borrows into it.
struct RunState {
    filters: Vec<Box<dyn Filter>>,
    runner: Box<dyn Runner>,
    cache: Mutex<Cache>,
    file_hashes: HashMap<PathBuf, String>,
    scope_maps: HashMap<PathBuf, ScopeMap>,
    file_sources: HashMap<PathBuf, String>,
    equiv: Option<EquivPipeline>,
    scope_prefix: String,
    deadline: Option<Instant>,
}

/// Build the filter chain and runner, verify the baseline suite is green, then
/// derive the per-file artifacts, cache, equiv pipeline, and testing-phase
/// deadline. Everything the per-mutant loop reads, in one place.
fn prepare(cfg: &Config, mutants: &[Arc<Mutant>]) -> Result<RunState> {
    let filters = filter::build_chain(cfg)?;
    let runner = runner::build(cfg);

    preflight_baseline(cfg, runner.as_ref(), mutants)?;

    // One read + at most one parse per unique file, deriving the cache-key
    // hash (always), the scope map (only in `scope` cache mode), and the source
    // for the equiv detector (only when it's on).
    let FileArtifacts {
        hashes: file_hashes,
        scope_maps,
        sources: file_sources,
    } = analyze_unique_files(
        mutants,
        matches!(cfg.cache_scope, CacheScope::Scope),
        cfg.equiv_detect,
    );
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

    // Testing-phase wall-clock ceiling. Computed here — after baseline
    // verification and the source-hash pass — so the budget covers the
    // per-mutant testing phase only, matching the flag's documented scope.
    let deadline = compute_test_deadline(cfg);

    Ok(RunState {
        filters,
        runner,
        cache,
        file_hashes,
        scope_maps,
        file_sources,
        equiv,
        scope_prefix,
        deadline,
    })
}

/// Post-testing wrap-up: warn on a budget-truncated run, report cache
/// integrity, fold learned kills into the kill-order sidecar, then build the
/// [`Report`] and (optionally) append the history entry the trend/gate read.
fn persist(
    cfg: &Config,
    started: Instant,
    runner: &dyn Runner,
    cache: &Mutex<Cache>,
    deadline: Option<Instant>,
    outcomes: Vec<MutantOutcome>,
) -> (Report, Option<HistoryEntry>) {
    // Surface the budget cutoff so a partial run reads as partial, not as a
    // clean pass over the whole catalogue. Mirrors the `--sample` floor note:
    // the skipped mutants are excluded from the score denominator, so without
    // this line a time-boxed run looks indistinguishable from a full one.
    if deadline.is_some() {
        let budget_skipped = outcomes
            .iter()
            .filter(|o| matches!(o, MutantOutcome::Skipped { filter, .. } if filter == TIME_BUDGET_FILTER))
            .count();
        if budget_skipped > 0 {
            warn!(
                budget_skipped,
                max_time_secs = cfg.max_time_secs,
                "--max-time budget exhausted: {budget_skipped} mutant(s) not tested \
                 (recorded as skipped/`{TIME_BUDGET_FILTER}`, excluded from the score)"
            );
        }
    }

    summarize_integrity(cfg, cache);

    // Smart ordering: fold the kills learned this run into the kill-order
    // sidecar so the next run puts proven killers first. Advisory — a write
    // failure just forfeits the speedup, never the run.
    if cfg.smart_order {
        let records = runner.take_kill_records();
        if !records.is_empty() {
            let mut ko = crate::kill_order::KillOrder::load(&cfg.kill_order_path);
            ko.apply(&records);
            if let Err(e) = ko.save(&cfg.kill_order_path) {
                warn!(path = %cfg.kill_order_path.display(), error = %e, "failed to save kill-order");
            }
        }
    }

    // Kill-set recording (higher-order-mutant experiment): drain the per-mutant
    // kill-sets the runner captured and write them as JSONL. Observational — a
    // write failure warns but never sinks the run.
    if let Some(path) = &cfg.record_kill_sets {
        let records = runner.take_kill_sets();
        match crate::runner::kill_sets::write_jsonl(path, &records) {
            Ok(()) => info!(
                path = %path.display(),
                count = records.len(),
                "wrote per-mutant kill-sets"
            ),
            Err(e) => warn!(path = %path.display(), error = %e, "failed to write kill-sets"),
        }
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

    (report, entry)
}

/// Pre-flight: confirm the unmutated suite is green before spending time
/// mutating. A red suite makes every covered mutant exit non-zero, which the
/// runner counts as "killed" — yielding a confidently-wrong score near 100%.
/// No-op when there's nothing to mutate or the user opted out.
fn preflight_baseline(cfg: &Config, runner: &dyn Runner, mutants: &[Arc<Mutant>]) -> Result<()> {
    if !cfg.verify_baseline || mutants.is_empty() {
        return Ok(());
    }
    info!("verifying baseline: running the unmutated test suite");
    match runner.baseline()? {
        runner::BaselineStatus::Passed => {
            info!("baseline suite is green");
            Ok(())
        }
        runner::BaselineStatus::Failed { output } => Err(anyhow::anyhow!(
            "baseline test suite is not green — aborting before mutation.\n\n\
             A failing or erroring suite makes every covered mutant exit \
             non-zero, which fermut counts as \"killed\", producing a \
             falsely high mutation score. Fix the suite (run your tests \
             until they pass) or, if you accept the risk, re-run with \
             --no-verify-baseline.\n\n\
             --- test output (tail) ---\n{output}"
        )),
    }
}

/// Testing-phase wall-clock ceiling from `--max-time`. `None` when unset.
/// Called after baseline verification and the source-hash pass so the budget
/// covers the per-mutant testing phase only, matching the flag's scope.
fn compute_test_deadline(cfg: &Config) -> Option<Instant> {
    cfg.max_time_secs
        .map(|secs| Instant::now() + Duration::from_secs(secs))
}

/// Parallel evaluate: build the worker pool and progress bar, then fan out
/// `evaluate` over every mutant. `ctx` carries the read-only per-run state.
fn evaluate_all(
    cfg: &Config,
    ctx: &EvalCtx,
    mutants: &[Arc<Mutant>],
) -> Result<Vec<MutantOutcome>> {
    let progress = make_progress_bar(mutants.len() as u64);
    let pool = build_pool(cfg.jobs)?;
    let outcomes: Vec<MutantOutcome> = pool.install(|| {
        mutants
            .par_iter()
            .map(|m| {
                let outcome = evaluate(ctx, m);
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
    Ok(outcomes)
}

/// Persist the cache and log a one-line end-of-run integrity summary so users
/// (and CI log scrapers) see the outcome without grepping every `warn!` line.
/// A non-zero `dropped_on_load` is the signal that someone fed us a tampered
/// or stale cache. No-op when caching is disabled.
fn summarize_integrity(cfg: &Config, cache: &Mutex<Cache>) {
    if !cfg.cache {
        return;
    }
    let guard = lock_recover(cache);
    let dropped = guard.dropped_on_load();
    let entries = guard.len();
    if let Err(e) = guard.save(&cfg.cache_path) {
        warn!(path = %cfg.cache_path.display(), error = %e, "failed to save cache");
    }
    info!(
        entries,
        dropped_on_load = dropped,
        "cache: post-run summary"
    );
}

/// Read-only, per-run context shared by every `evaluate` call. Built once
/// before the per-mutant fan-out (only `mutant` varies across calls), so the
/// hot loop passes one reference instead of ten identical arguments.
struct EvalCtx<'a> {
    cfg: &'a Config,
    filters: &'a [Box<dyn Filter>],
    runner: &'a dyn Runner,
    cache: &'a Mutex<Cache>,
    file_hashes: &'a HashMap<PathBuf, String>,
    scope_maps: &'a HashMap<PathBuf, ScopeMap>,
    scope_prefix: &'a str,
    equiv: Option<&'a EquivPipeline>,
    file_sources: &'a HashMap<PathBuf, String>,
    /// Testing-phase wall-clock ceiling. Identical for every mutant in the
    /// run; a mutant reaching `evaluate` past it is a cheap `time-budget` skip.
    deadline: Option<Instant>,
}

fn evaluate(ctx: &EvalCtx, mutant: &Arc<Mutant>) -> MutantOutcome {
    // Testing-phase budget check first, before the filter chain or the runner.
    // Past the deadline every remaining mutant is a cheap skip — including the
    // ty pre-filter, the dominant per-mutant cost — so the wall-clock ceiling
    // actually holds. In-flight mutants that passed this check already run to
    // completion; only not-yet-started ones short-circuit here.
    if let Some(deadline) = ctx.deadline {
        if Instant::now() >= deadline {
            return MutantOutcome::skipped(mutant.clone(), TIME_BUDGET_FILTER);
        }
    }

    // Filter chain first — skip decisions depend on current filter config,
    // so we can't satisfy them from cache.
    for f in ctx.filters {
        match f.admits(mutant) {
            Ok(true) => continue,
            Ok(false) => return MutantOutcome::skipped(mutant.clone(), f.name()),
            Err(e) => {
                warn!(filter = f.name(), error = %e, "filter errored; admitting mutant");
            }
        }
    }

    let scope = compute_mutant_scope(ctx.scope_prefix, ctx.cfg, mutant);
    let identity_hash = mutant_identity_hash(ctx.cfg, mutant, ctx.file_hashes, ctx.scope_maps);

    if let Some(hash) = &identity_hash {
        if let Some((cached, checked)) =
            lock_recover(ctx.cache).lookup_entry(&mutant.id, hash, &scope)
        {
            // Translate the cached verdict against the current detector
            // setting before returning. The cache stores the post-equiv
            // outcome and an `equiv_checked` marker, so the common warm path
            // is a pure Survived↔Equivalent flip (no pipeline, no CPython
            // subprocess). `remap_cached_outcome` only pays the classify cost
            // once — for a `Survived` cached by a detector-off run — and
            // rewrites the entry so the next warm run skips it. Lets
            // `--no-equiv-detect` toggle take effect without busting the
            // cache. Detector rule changes are *not* covered — those need a
            // scope-prefix bump.
            let (outcome, rewrite) =
                remap_cached_outcome(cached, checked, ctx.equiv, ctx.file_sources);
            if let Some(new_checked) = rewrite {
                lock_recover(ctx.cache).insert_checked(
                    mutant.id.clone(),
                    hash.clone(),
                    scope.clone(),
                    outcome.clone(),
                    new_checked,
                );
            }
            return outcome;
        }
    }

    let outcome = ctx
        .runner
        .run(mutant)
        .unwrap_or_else(|e| MutantOutcome::error(mutant.clone(), e.to_string()));

    let outcome = maybe_remap_equivalent(outcome, ctx.equiv, ctx.file_sources);

    if let Some(hash) = identity_hash {
        // A detector-on run classified any surviving mutant just above, so
        // the stored outcome is already post-equiv — mark it checked so warm
        // runs never re-classify it. A detector-off run leaves it unchecked.
        lock_recover(ctx.cache).insert_checked(
            mutant.id.clone(),
            hash,
            scope,
            outcome.clone(),
            ctx.equiv.is_some(),
        );
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
    classify_survivor(pipeline, outcome, file_sources)
}

/// Run the equivalence pipeline on a `Survived` outcome, promoting it to
/// `Equivalent` only on a `ProvablyEquivalent` verdict. Non-`Survived`
/// outcomes pass through untouched. This is the one place that calls
/// `pipeline.classify` — and thus the only place that can spawn the CPython
/// bytecode probe — so keeping its callers on the cold path (fresh run, or a
/// one-time reclassify of a detector-off cache entry) is what makes the warm
/// cache free of subprocess churn.
fn classify_survivor(
    pipeline: &EquivPipeline,
    outcome: MutantOutcome,
    file_sources: &HashMap<PathBuf, String>,
) -> MutantOutcome {
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

/// Translate a *cached* outcome against the current detector setting, using
/// the entry's `equiv_checked` marker to avoid re-running the pipeline.
///
/// Returns the outcome to report, plus `Some(new_equiv_checked)` when the
/// cache entry should be rewritten (and `None` when it should be left as-is).
///
/// - Detector off: demote a cached `Equivalent` back to `Survived`, pass
///   everything else. Never rewrites — the stored verdict stays valid for the
///   next detector-on run.
/// - Detector on, entry already `equiv_checked`: pure flip. A cached
///   `Survived` stays `Survived`, a cached `Equivalent` stays `Equivalent`.
///   No `classify` call, no subprocess — this is the hot warm-cache path.
/// - Detector on, cached `Survived` *not* yet checked (written by a
///   detector-off run, or a pre-marker fermut version): classify once, then
///   rewrite the entry marked checked so subsequent warm runs skip the probe.
fn remap_cached_outcome(
    outcome: MutantOutcome,
    equiv_checked: bool,
    equiv: Option<&EquivPipeline>,
    file_sources: &HashMap<PathBuf, String>,
) -> (MutantOutcome, Option<bool>) {
    let Some(pipeline) = equiv else {
        if let MutantOutcome::Equivalent { mutant, .. } = outcome {
            return (MutantOutcome::Survived { mutant }, None);
        }
        return (outcome, None);
    };
    if equiv_checked || !matches!(outcome, MutantOutcome::Survived { .. }) {
        return (outcome, None);
    }
    (
        classify_survivor(pipeline, outcome, file_sources),
        Some(true),
    )
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
        RunnerKind::Rstest => "rstest",
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
        if let Some(tests) = ctx.tests_for_mutant(mutant) {
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

/// Per-file artifacts for a run, keyed by source path. Built in one pass by
/// [`analyze_unique_files`].
struct FileArtifacts {
    /// Cache-key AST hash for every unique file (byte fallback on parse/utf-8
    /// failure). Always populated.
    hashes: HashMap<PathBuf, String>,
    /// Scope maps for the `scope` cache mode. Empty otherwise; files that fail
    /// to parse are absent (consumers fall back to the file hash).
    scope_maps: HashMap<PathBuf, ScopeMap>,
    /// Source text for the equiv detector, so classification doesn't re-read
    /// the file per mutant. Empty when equiv detection is off.
    sources: HashMap<PathBuf, String>,
}

/// Read + parse each unique mutated file exactly once, deriving every per-file
/// artifact the run needs. Replaces three separate passes (`hash_unique_files`,
/// `build_scope_maps`, `load_unique_files`) that each re-read and re-parsed the
/// same files — 2–3× the IO and parse work. `want_scope`/`want_source` gate the
/// optional artifacts so a run that needs neither pays only for the hash.
fn analyze_unique_files(
    mutants: &[Arc<Mutant>],
    want_scope: bool,
    want_source: bool,
) -> FileArtifacts {
    let mut hashes = HashMap::new();
    let mut scope_maps = HashMap::new();
    let mut sources = HashMap::new();
    for m in mutants {
        if hashes.contains_key(&m.file) {
            continue;
        }
        let analysis = match ast_hash::analyze_file(&m.file, want_scope, want_source) {
            Ok(a) => a,
            Err(e) => {
                warn!(file = %m.file.display(), error = %e, "source read failed; cache disabled for this file");
                continue;
            }
        };
        hashes.insert(m.file.clone(), analysis.ast_hash);
        if want_scope {
            match analysis.scope_map {
                Some(map) => {
                    scope_maps.insert(m.file.clone(), map);
                }
                None => {
                    warn!(file = %m.file.display(), "scope-map parse failed; falling back to file hash for this file");
                }
            }
        }
        if want_source {
            match analysis.source {
                Some(s) => {
                    sources.insert(m.file.clone(), s);
                }
                None => {
                    warn!(file = %m.file.display(), "source not utf-8; equiv detector disabled for this file");
                }
            }
        }
    }
    FileArtifacts {
        hashes,
        scope_maps,
        sources,
    }
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
            stmt_line: 1,
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
    fn cached_checked_survivor_is_pure_flip_no_reclassify() {
        // The warm-cache fix: a `Survived` cached by a detector-on run
        // (equiv_checked = true) is returned as-is with NO rewrite — proving
        // the pipeline (and its CPython bytecode subprocess) is never touched.
        // The source map is empty on purpose: if `classify` ran it would have
        // to consult it; skipping classification is the whole point.
        let m = arith_mutant("return x + 0", "+", "-");
        let cached = MutantOutcome::survived(m.clone());
        let pipeline = EquivPipeline::default_pipeline();
        let (result, rewrite) =
            remap_cached_outcome(cached, true, Some(&pipeline), &HashMap::new());
        assert!(matches!(result, MutantOutcome::Survived { .. }));
        assert_eq!(rewrite, None, "checked entry must not be rewritten");
    }

    #[test]
    fn cached_checked_equivalent_stays_equivalent_no_rewrite() {
        // A cached `Equivalent` (inherently classified) under detector-on is a
        // pure pass-through: no reclassify, no rewrite.
        let m = arith_mutant("return x + 0", "+", "-");
        let cached = MutantOutcome::equivalent(m, "reason", "bytecode-identity");
        let pipeline = EquivPipeline::default_pipeline();
        let (result, rewrite) =
            remap_cached_outcome(cached, true, Some(&pipeline), &HashMap::new());
        assert!(matches!(result, MutantOutcome::Equivalent { .. }));
        assert_eq!(rewrite, None);
    }

    #[test]
    fn cached_unchecked_survivor_reclassifies_once_and_requests_rewrite() {
        // A `Survived` cached by a detector-off run (equiv_checked = false):
        // detector-on now classifies it and asks the caller to rewrite the
        // entry `checked = true`, so the probe runs at most once across warm
        // runs. `return None` → `return` is bytecode-identical under CPython,
        // so classification promotes to Equivalent. Skips without python3 so
        // the test isn't environment-fragile.
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
            stmt_line: 2,
        };
        let cached = MutantOutcome::survived(m.clone());
        let pipeline = EquivPipeline::default_pipeline();
        let mut sources = HashMap::new();
        sources.insert(m.file.clone(), source.to_string());
        let (result, rewrite) = remap_cached_outcome(cached, false, Some(&pipeline), &sources);
        assert_eq!(rewrite, Some(true), "must rewrite entry as checked");
        match result {
            MutantOutcome::Equivalent { source: src, .. } => assert_eq!(src, "bytecode-identity"),
            other => panic!("expected Equivalent, got {other:?}"),
        }
    }

    #[test]
    fn cached_unchecked_non_survivor_is_untouched_no_rewrite() {
        // Detector-on, unchecked, but the cached outcome isn't a `Survived`
        // candidate — pass through with no classify and no rewrite.
        let m = arith_mutant("return x + 0", "+", "-");
        let cached = MutantOutcome::killed(m);
        let pipeline = EquivPipeline::default_pipeline();
        let (result, rewrite) =
            remap_cached_outcome(cached, false, Some(&pipeline), &HashMap::new());
        assert!(matches!(result, MutantOutcome::Killed { .. }));
        assert_eq!(rewrite, None);
    }

    #[test]
    fn cached_detector_off_demotes_equivalent_no_rewrite() {
        // Detector off: a cached `Equivalent` demotes to `Survived` without a
        // rewrite (the stored verdict stays valid for the next detector-on run).
        let m = arith_mutant("return x + 0", "+", "-");
        let cached = MutantOutcome::equivalent(m, "reason", "bytecode-identity");
        let (result, rewrite) = remap_cached_outcome(cached, true, None, &HashMap::new());
        assert!(matches!(result, MutantOutcome::Survived { .. }));
        assert_eq!(rewrite, None);
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

    /// Runner that must never be invoked — proves the deadline short-circuits
    /// before the runner in `evaluate`.
    struct PanicRunner;
    impl Runner for PanicRunner {
        fn run(&self, _m: &Arc<Mutant>) -> Result<MutantOutcome> {
            panic!("runner invoked past the --max-time deadline");
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Passed)
        }
    }

    /// Runner that reports every mutant killed. Proves a not-yet-expired
    /// deadline still lets the runner execute.
    struct KillRunner;
    impl Runner for KillRunner {
        fn run(&self, m: &Arc<Mutant>) -> Result<MutantOutcome> {
            Ok(MutantOutcome::killed(m.clone()))
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Passed)
        }
    }

    fn eval_with_deadline(runner: &dyn Runner, deadline: Option<Instant>) -> MutantOutcome {
        let cfg = base_test_config();
        let m = Arc::new(arith_mutant("return x + 0", "+", "-"));
        let cache = Mutex::new(Cache::default());
        let ctx = EvalCtx {
            cfg: &cfg,
            filters: &[],
            runner,
            cache: &cache,
            file_hashes: &HashMap::new(),
            scope_maps: &HashMap::new(),
            scope_prefix: "",
            equiv: None,
            file_sources: &HashMap::new(),
            deadline,
        };
        evaluate(&ctx, &m)
    }

    #[test]
    fn time_budget_filter_is_stable() {
        // Callers (PR gate, skipped_by_filter) match this literal.
        assert_eq!(TIME_BUDGET_FILTER, "time-budget");
    }

    #[test]
    fn past_deadline_skips_before_running() {
        // Deadline already elapsed: must skip with the budget filter and never
        // touch the runner (PanicRunner would blow up if it did).
        let past = Instant::now() - Duration::from_secs(1);
        let out = eval_with_deadline(&PanicRunner, Some(past));
        match out {
            MutantOutcome::Skipped { filter, .. } => assert_eq!(filter, TIME_BUDGET_FILTER),
            other => panic!("expected time-budget skip, got {other:?}"),
        }
    }

    #[test]
    fn future_deadline_still_runs() {
        // Plenty of budget left: the runner must execute normally.
        let future = Instant::now() + Duration::from_secs(3600);
        let out = eval_with_deadline(&KillRunner, Some(future));
        assert!(matches!(out, MutantOutcome::Killed { .. }));
    }

    #[test]
    fn no_deadline_runs() {
        // `None` deadline (no --max-time) never short-circuits.
        let out = eval_with_deadline(&KillRunner, None);
        assert!(matches!(out, MutantOutcome::Killed { .. }));
    }

    #[test]
    fn order_by_value_is_noop_without_coverage() {
        // No coverage context → no value signal → order preserved exactly.
        let cfg = base_test_config();
        let a = arith_mutant("a + 0", "+", "-");
        let mut b = arith_mutant("b + 0", "+", "-");
        b.id = "b".into();
        let mut mutants = vec![Arc::new(a.clone()), Arc::new(b.clone())];
        order_by_value(&mut mutants, &cfg);
        assert_eq!(mutants[0].id, a.id);
        assert_eq!(mutants[1].id, b.id);
    }

    #[test]
    fn order_by_value_puts_covered_mutants_first() {
        use crate::filter::coverage::CoverageContexts;
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\ny = 2\nz = 3\n").unwrap();
        // Only line 1 is covered by a test.
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let cov_path = tmp.path().join("coverage.json");
        std::fs::write(&cov_path, doc).unwrap();
        let cov = CoverageContexts::from_json(&cov_path, tmp.path(), tmp.path()).unwrap();

        let mut covered = arith_mutant("x + 0", "+", "-");
        covered.id = "covered".into();
        covered.file = py.clone();
        covered.line = 1;
        covered.stmt_line = 1;
        let mut uncovered = arith_mutant("z + 0", "+", "-");
        uncovered.id = "uncovered".into();
        uncovered.file = py.clone();
        uncovered.line = 3;
        uncovered.stmt_line = 3;

        // Input order is uncovered-first; ordering must swap it.
        let mut mutants = vec![Arc::new(uncovered.clone()), Arc::new(covered.clone())];
        let cfg = Config {
            coverage: Some(cov),
            ..base_test_config()
        };
        order_by_value(&mut mutants, &cfg);
        assert_eq!(mutants[0].id, "covered", "covered mutant must sort first");
        assert_eq!(mutants[1].id, "uncovered");
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
            stmt_line: 2,
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
