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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::ast_hash::{self, Anchors, ScopeMap};
use crate::cache::{Cache, SurvivorBasis};
use crate::config::{CacheScope, Config, MemoryCap, RunnerKind};
use crate::cov_identity::CovIdentity;
use crate::equiv::{EquivPipeline, EquivVerdict};
use crate::filter::{self, Filter};
use crate::history::HistoryEntry;
use crate::mutator::{self, Mutant};
use crate::report::{MutantOutcome, Report};
use crate::runner::{self, Runner};
use crate::sync::lock_recover;
use crate::test_tree::TestTree;

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
    let mut mutants: Vec<Arc<Mutant>> =
        mutator::collect_from_tree(&cfg.source_root, &cfg.exclude, &cfg.ignore_patterns)
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
    let cfg = state.narrowed.as_ref().unwrap_or(cfg);

    let ctx = EvalCtx {
        cfg,
        filters: &state.filters,
        runner: state.runner.as_ref(),
        cache: &state.cache,
        file_hashes: &state.files.hashes,
        scope_maps: &state.files.scope_maps,
        cov_identity: state.files.cov.as_ref(),
        anchors: &state.files.anchors,
        scope_prefix: &state.scope_prefix,
        test_tree: &state.test_tree,
        equiv: state.equiv.as_ref(),
        file_sources: &state.files.sources,
        deadline: state.deadline,
        stats: &state.stats,
        killer_hits: &state.killer_hits,
    };
    let (mut outcomes, inferred) = if cfg.only_dominators {
        evaluate_with_dominators(cfg, &ctx, &mutants)?
    } else {
        (evaluate_all(cfg, &ctx, &mutants)?, Vec::new())
    };
    audit_killer_hits(cfg, &ctx, &mutants, &mut outcomes)?;
    let audit_failures = audit_inferred(cfg, &ctx, &mutants, &mut outcomes, inferred)?;

    let (mut report, entry) = persist(
        cfg,
        started,
        state.runner.as_ref(),
        &state.cache,
        &state.stats,
        state.deadline,
        outcomes,
    );
    report.inferred_audit_failures = audit_failures;
    Ok((report, entry))
}

/// `--only-dominators` (see [`crate::subsume::DominatorPlan`]): run the
/// mutants the dominator store can't vouch for, infer the kills a dominator
/// kill implies, then run whatever couldn't be inferred. Returns the outcomes
/// in `mutants` order plus the inferred kills, for [`audit_inferred`].
fn evaluate_with_dominators(
    cfg: &Config,
    ctx: &EvalCtx,
    mutants: &[Arc<Mutant>],
) -> Result<(Vec<MutantOutcome>, Vec<KillerHit>)> {
    let files: std::collections::BTreeSet<&Path> =
        mutants.iter().map(|m| m.file.as_path()).collect();
    let plan = crate::subsume::DominatorPlan::load(&cfg.dominators_path, files)?;
    let (first, deferred): (Vec<usize>, Vec<usize>) =
        (0..mutants.len()).partition(|&i| plan.runs_first(&mutants[i]));
    info!(
        run_first = first.len(),
        deferred = deferred.len(),
        fresh_files = plan.fresh_files(),
        "--only-dominators: running dominators and unvouched mutants first"
    );

    let mut slots: Vec<Option<MutantOutcome>> = vec![None; mutants.len()];
    let pick =
        |idx: &[usize]| -> Vec<Arc<Mutant>> { idx.iter().map(|&i| mutants[i].clone()).collect() };
    for (&i, o) in first.iter().zip(evaluate_all(cfg, ctx, &pick(&first))?) {
        slots[i] = Some(o);
    }

    // Observed kills only: an inferred kill never seeds another inference.
    let killed: HashMap<String, String> = slots
        .iter()
        .flatten()
        .filter_map(|o| match o {
            MutantOutcome::Killed {
                mutant,
                killer: Some(t),
                inferred_from: None,
                ..
            } => Some((mutant.id.clone(), t.clone())),
            _ => None,
        })
        .collect();
    let killed: HashMap<&str, &str> = killed
        .iter()
        .map(|(m, t)| (m.as_str(), t.as_str()))
        .collect();

    // Deferred mutants still pass through the filter chain: a mutant the
    // chain would skip is skipped, never inferred.
    let pool = build_pool(cfg.jobs)?;
    let prechecked: Vec<Option<MutantOutcome>> = pool.install(|| {
        deferred
            .par_iter()
            .map(|&i| precheck(ctx, &mutants[i]))
            .collect()
    });
    let mut inferred = Vec::new();
    let mut second = Vec::new();
    for (&i, skip) in deferred.iter().zip(prechecked) {
        let m = &mutants[i];
        if let Some(skip) = skip {
            slots[i] = Some(skip);
        } else if let Some((rep, t)) = plan.infer(m, &killed) {
            slots[i] = Some(MutantOutcome::killed_inferred(m.clone(), t.clone(), rep));
            inferred.push(KillerHit {
                mutant: m.clone(),
                killer: t,
            });
        } else {
            second.push(i);
        }
    }
    info!(
        inferred = inferred.len(),
        rerun = second.len(),
        "--only-dominators: inferred kills; running the rest"
    );
    for (&i, o) in second
        .iter()
        .zip(evaluate_all_with(cfg, &pick(&second), |m| {
            evaluate_admitted(ctx, m)
        })?)
    {
        slots[i] = Some(o);
    }
    Ok((
        slots
            .into_iter()
            .map(|o| o.expect("every mutant gets an outcome"))
            .collect(),
        inferred,
    ))
}

/// Re-run a sample of inferred kills against their killing test alone. A kill
/// that doesn't reproduce means the recorded kill-sets no longer describe the
/// suite (an edited test, a flaky one): the mutant is re-run fully, its real
/// outcome replaces the inferred one, and the failure is counted so `fermut
/// run` exits non-zero. Returns the number of failures.
fn audit_inferred(
    cfg: &Config,
    ctx: &EvalCtx,
    mutants: &[Arc<Mutant>],
    outcomes: &mut [MutantOutcome],
    inferred: Vec<KillerHit>,
) -> Result<usize> {
    let picked = sample_for_audit(inferred, cfg.inferred_audit_rate);
    if picked.is_empty() {
        return Ok(0);
    }
    if ctx.deadline.is_some_and(|d| Instant::now() >= d) {
        warn!(
            pending = picked.len(),
            "inferred-kill audit skipped: --max-time budget exhausted"
        );
        return Ok(0);
    }
    let pool = build_pool(cfg.jobs)?;
    let failed: Vec<MutantOutcome> = pool.install(|| {
        picked
            .par_iter()
            .filter_map(|hit| {
                let verdict = match ctx.runner.audit(&hit.mutant, &hit.killer) {
                    // The runner can't audit (unittest): nothing to compare.
                    Ok(None) => return None,
                    Ok(Some(v)) => v,
                    Err(e) => MutantOutcome::error(hit.mutant.clone(), e.to_string()),
                };
                if matches!(verdict, MutantOutcome::Killed { .. }) {
                    return None;
                }
                warn!(
                    mutant = %hit.mutant.id,
                    file = %hit.mutant.file.display(),
                    line = hit.mutant.line,
                    killer = %hit.killer,
                    audit = verdict.status_label(),
                    "inferred kill did not reproduce against its killing test; \
                     re-running the mutant (re-record kill-sets and re-run `fermut subsume`)"
                );
                Some(evaluate_admitted(ctx, &hit.mutant))
            })
            .collect()
    });
    let n = failed.len();
    for outcome in failed {
        if let Some(i) = mutants.iter().position(|m| m.id == outcome.mutant().id) {
            outcomes[i] = outcome;
        }
    }
    info!(audited = picked.len(), failed = n, "inferred-kill audit");
    Ok(n)
}

/// Read-only state assembled before the testing phase and borrowed by every
/// mutant's [`evaluate`]. Owned here (not in [`run`]) so [`prepare`] can build
/// it in one place; [`EvalCtx`] holds borrows into it.
struct RunState {
    filters: Vec<Box<dyn Filter>>,
    runner: Box<dyn Runner>,
    cache: Mutex<Cache>,
    files: FileArtifacts,
    equiv: Option<EquivPipeline>,
    scope_prefix: String,
    test_tree: TestTree,
    deadline: Option<Instant>,
    stats: CacheStats,
    killer_hits: Mutex<Vec<KillerHit>>,
    /// The run's config narrowed to the tests the baseline's pytest kept
    /// ([`narrow_to_baseline`]); the rest of the run reads it in place of the
    /// caller's.
    narrowed: Option<Config>,
}

/// Per-run cache counters, reported in the post-run summary. Atomics so the
/// parallel `evaluate` workers bump them without a lock.
#[derive(Default)]
struct CacheStats {
    /// Lookups served by the full `(file_hash, scope)` key.
    full_hits: AtomicUsize,
    /// Lookups served by the killer-scoped fallback.
    killer_hits: AtomicUsize,
    /// Lookups that found nothing usable and ran the mutant.
    misses: AtomicUsize,
    /// Killer hits re-run against their killer by the audit.
    audited: AtomicUsize,
    /// Audited killer hits whose kill didn't reproduce (entry dropped).
    dropped: AtomicUsize,
    /// Cached survivors re-checked against only their new or changed tests.
    delta_hits: AtomicUsize,
    /// Tests those re-checks ran, and the covering tests a full run would have.
    delta_tests: AtomicUsize,
    delta_full_tests: AtomicUsize,
    /// Delta survivors also run against every covering test, and those where
    /// that full run detected the mutant.
    delta_audited: AtomicUsize,
    delta_disagreed: AtomicUsize,
    /// The first delta survivor of a run is always audited.
    delta_first_audit_done: std::sync::atomic::AtomicBool,
}

/// A mutant served from the cache by its killer scope, queued for the
/// post-evaluation sampled audit.
struct KillerHit {
    mutant: Arc<Mutant>,
    killer: String,
}

/// Build the filter chain and runner, verify the baseline suite is green, then
/// derive the per-file artifacts, cache, equiv pipeline, and testing-phase
/// deadline. Everything the per-mutant loop reads, in one place.
fn prepare(cfg: &Config, mutants: &[Arc<Mutant>]) -> Result<RunState> {
    let mut filters = filter::build_chain(cfg)?;
    let mut runner = runner::build(cfg);

    // One read + at most one parse per unique file, deriving the cache-key
    // hash (always), the scope map (only in `scope` cache mode), and the source
    // for the equiv detector (only when it's on). Narrowing (below) changes
    // none of these, nor the scope prefix.
    let mut files = analyze_unique_files(
        mutants,
        matches!(cfg.cache_scope, CacheScope::Scope),
        cfg.equiv_detect,
    );
    if let (true, CacheScope::Coverage, Some(ctx)) = (cfg.cache, cfg.cache_scope, &cfg.coverage) {
        files.cov = Some(CovIdentity::build(
            ctx,
            &cfg.tests_path(),
            files.hashes.keys().map(PathBuf::as_path),
        ));
    }
    let scope_prefix = compute_scope_prefix(cfg);
    // Walked and hashed once; each mutant's test fingerprint folds from it.
    let test_tree = TestTree::walk(&cfg.tests_path());
    let cache = if cfg.cache {
        Mutex::new(Cache::load(&cfg.cache_path))
    } else {
        Mutex::new(Cache::default())
    };
    // Only the mutants that will actually run: a warm watch iteration with a
    // handful of misses then spawns them instead of starting a template.
    let uncached = |cfg: &Config, filters: &[Box<dyn Filter>]| -> Vec<Arc<Mutant>> {
        let cache = lock_recover(&cache);
        prepare_candidates(filters, mutants)
            .into_iter()
            .filter(|m| !is_cached(cfg, &cache, &files, &scope_prefix, &test_tree, m))
            .collect()
    };
    // The last baseline's deselection, when nothing that decides it changed:
    // the narrowing the baseline would reproduce, without running it.
    let dkey = deselection_key(cfg, &scope_prefix, &test_tree);
    let saved = lock_recover(&cache)
        .deselection(&dkey)
        .and_then(|d| narrow_to_baseline(cfg, d));
    let mut to_run = match &saved {
        Some(n) => uncached(n, &filter::build_chain(n)?),
        None => uncached(cfg, &filters),
    };

    // Nothing will run: the baseline would only guard runs that don't happen.
    let own_baseline = if to_run.is_empty() {
        if cfg.verify_baseline && !mutants.is_empty() {
            info!("every mutant is cached or filtered: skipping the baseline");
        }
        None
    } else {
        preflight_baseline(cfg, runner.as_ref(), mutants)?
    };
    let narrowed = if own_baseline.is_some() {
        let d = runner.baseline_deselection();
        if let Some(d) = d.as_ref().filter(|d| cfg.cache && !d.deselected.is_empty()) {
            lock_recover(&cache).remember_deselection(dkey, d.clone());
        }
        let narrowed = d.and_then(|d| narrow_to_baseline(cfg, &d));
        if let Some(n) = &narrowed {
            to_run = uncached(n, &filter::build_chain(n)?);
        }
        narrowed
    } else {
        saved
    };
    if let Some(n) = &narrowed {
        filters = filter::build_chain(n)?;
        runner = runner::build(n);
    }
    let cfg = narrowed.as_ref().unwrap_or(cfg);
    if let Some(bytes) = memory_cap_bytes(
        cfg.max_memory,
        own_baseline.and_then(|_| runner::memory::children_peak_rss()),
    ) {
        info!(
            cap_mib = bytes >> 20,
            "per-mutant memory cap (--max-memory)"
        );
        runner.set_memory_cap(bytes);
    }
    let baseline_wall = own_baseline.or_else(|| cfg.baseline_secs.map(Duration::from_secs_f64));
    // Only the runner sees the budget. The cache scope and history hash keep
    // the configured `timeout_secs` + factor: the budget moves a little every
    // run and would otherwise miss the cache every time.
    if let Some(secs) = budgeted_timeout_secs(cfg, baseline_wall) {
        runner.set_timeout(Duration::from_secs(secs));
    }
    runner.prepare(&to_run)?;
    let equiv = if cfg.equiv_detect {
        Some(EquivPipeline::default_pipeline())
    } else {
        None
    };

    // Testing-phase wall-clock ceiling. Computed here — after baseline
    // verification and the source-hash pass — so the budget covers the
    // per-mutant testing phase only, matching the flag's documented scope.
    let deadline = compute_test_deadline(cfg);

    Ok(RunState {
        filters,
        runner,
        cache,
        files,
        equiv,
        scope_prefix,
        test_tree,
        deadline,
        stats: CacheStats::default(),
        killer_hits: Mutex::new(Vec::new()),
        narrowed,
    })
}

/// Fingerprint of what decides the baseline's deselection: the pytest args
/// and run shape (`scope_prefix`), the tests tree, `PYTEST_ADDOPTS`, and every
/// pytest config file and `conftest.py` in the tests root's ancestors (where
/// pytest looks for `addopts` and root conftests).
fn deselection_key(cfg: &Config, scope_prefix: &str, test_tree: &TestTree) -> String {
    const CONFIGS: &[&str] = &[
        "pyproject.toml",
        "pytest.ini",
        ".pytest.ini",
        "setup.cfg",
        "tox.ini",
        "conftest.py",
    ];
    let mut h = Sha256::new();
    h.update(scope_prefix.as_bytes());
    h.update(b"|tests=");
    h.update(test_tree.whole().as_bytes());
    h.update(b"|addopts=");
    h.update(
        std::env::var("PYTEST_ADDOPTS")
            .unwrap_or_default()
            .as_bytes(),
    );
    let tests = cfg.tests_path();
    let tests = tests.canonicalize().unwrap_or(tests);
    for dir in tests.ancestors().skip(1) {
        for name in CONFIGS {
            let path = dir.join(name);
            if let Ok(body) = std::fs::read(&path) {
                h.update(b"|");
                h.update(path.as_os_str().as_encoded_bytes());
                h.update(b"=");
                h.update(Sha256::digest(&body));
            }
        }
    }
    hex::encode(h.finalize())
}

/// `cfg` narrowed the way `--select-tests` narrows it, to the tests a green
/// baseline ran, when its pytest deselected some (`--deselect`, `-k`, `-m` in
/// the pytest args or `addopts`). A mutant only deselected tests cover then
/// skips as `selection` instead of exiting 5 ("no tests collected"). `None`
/// when nothing was deselected, there's no coverage to narrow, or
/// `--select-tests` already narrowed it.
fn narrow_to_baseline(cfg: &Config, d: &runner::Deselection) -> Option<Config> {
    if d.deselected.is_empty() || cfg.coverage_unselected.is_some() {
        return None;
    }
    let full = cfg.coverage.clone()?;
    info!(
        deselected = d.deselected.len(),
        "pytest deselected tests in the baseline; mutants only they cover are skipped \
         as `selection`"
    );
    Some(Config {
        coverage: Some(full.deselect(&d.deselected, &d.ran)),
        coverage_unselected: Some(full),
        ..cfg.clone()
    })
}

/// The mutants worth handing `Runner::prepare`: those the cheap filters
/// admit (sample, coverage, …) and no costly filter already remembers
/// rejecting. `--schemata` builds every prepared mutant into its schema, and
/// most never run. [`prepare`] also drops cache hits; the costly filters drop
/// a few more later, and those just sit in the schema unused. A filter error
/// admits, as in [`precheck`].
fn prepare_candidates(filters: &[Box<dyn Filter>], mutants: &[Arc<Mutant>]) -> Vec<Arc<Mutant>> {
    mutants
        .iter()
        .filter(|m| {
            filters.iter().all(|f| {
                if f.costly() {
                    f.cached_verdict(m) != Some(false)
                } else {
                    f.admits(m).unwrap_or(true)
                }
            })
        })
        .cloned()
        .collect()
}

/// Whether [`evaluate_admitted`] would serve `mutant` from the cache (a full
/// or killer hit) without running it.
fn is_cached(
    cfg: &Config,
    cache: &Cache,
    files: &FileArtifacts,
    scope_prefix: &str,
    test_tree: &TestTree,
    mutant: &Mutant,
) -> bool {
    let Some(hash) = mutant_identity_hash(
        cfg,
        mutant,
        &files.hashes,
        &files.scope_maps,
        files.cov.as_ref(),
    ) else {
        return false;
    };
    let scope = compute_mutant_scope(scope_prefix, cfg, test_tree, mutant);
    let key = cache_key(mutant, &files.anchors);
    cache.lookup_entry(&key, &hash, &scope).is_some()
        || cache
            .lookup_killer(&key, &hash, |t| {
                compute_killer_scope(scope_prefix, cfg, test_tree, mutant, t)
            })
            .is_some()
}

/// Post-testing wrap-up: warn on a budget-truncated run, report cache
/// integrity, fold learned kills into the kill-order sidecar, then build the
/// [`Report`] and (optionally) append the history entry the trend/gate read.
fn persist(
    cfg: &Config,
    started: Instant,
    runner: &dyn Runner,
    cache: &Mutex<Cache>,
    stats: &CacheStats,
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

    summarize_integrity(cfg, cache, stats);

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

    let mut report = Report::new(outcomes);
    report.shard = cfg
        .shard
        .map(|(index, total)| crate::report::Shard { index, total });
    report.dominator_score = dominator_score(cfg, &report.outcomes);
    report.config_hash = Some(crate::history::universe_hash(cfg));
    report.duration_ms = u64::try_from(started.elapsed().as_millis()).ok();

    let entry = if cfg.history {
        let hash = crate::history::config_hash(cfg);
        let entry =
            HistoryEntry::from_report(&report, &cfg.source_root, report.duration_ms, Some(hash));
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

/// Dominator score from `.fermut/dominators.json`, when one exists and still
/// describes this run. Advisory: a missing store is silent, a stale or broken
/// one logs why at debug/warn level and leaves the score out.
fn dominator_score(cfg: &Config, outcomes: &[MutantOutcome]) -> Option<f64> {
    let store = match crate::subsume::DominatorStore::load(&cfg.dominators_path) {
        Ok(Some(store)) => store,
        Ok(None) => return None,
        Err(e) => {
            warn!(path = %cfg.dominators_path.display(), error = %e, "ignoring dominator store");
            return None;
        }
    };
    match crate::subsume::report_dominator_score(&store, outcomes) {
        Ok(score) => score,
        Err(reason) => {
            debug!(path = %cfg.dominators_path.display(), %reason, "dominator store is stale; no dominator_score");
            None
        }
    }
}

/// Pre-flight: confirm the unmutated suite is green before spending time
/// mutating. A red suite makes every covered mutant exit non-zero, which the
/// runner counts as "killed" — yielding a confidently-wrong score near 100%.
/// No-op when there's nothing to mutate or the user opted out.
/// Returns how long the green suite's test process ran (`None` when the
/// baseline didn't run).
fn preflight_baseline(
    cfg: &Config,
    runner: &dyn Runner,
    mutants: &[Arc<Mutant>],
) -> Result<Option<Duration>> {
    if !cfg.verify_baseline || mutants.is_empty() {
        return Ok(None);
    }
    info!("verifying baseline: running the unmutated test suite");
    match runner.baseline()? {
        runner::BaselineStatus::Passed { wall } => {
            info!("baseline suite is green");
            Ok(Some(wall))
        }
        runner::BaselineStatus::Failed { output } => Err(anyhow::anyhow!(
            "baseline test suite is not green — aborting before mutation.\n\n\
             A failing or erroring suite makes every covered mutant exit \
             non-zero, which fermut counts as \"killed\", producing a \
             falsely high mutation score. Fix the suite (run your tests \
             until they pass) or, if you accept the risk, re-run with \
             --no-verify-baseline.\n\n\
             If the same tests pass when you run them yourself, fermut's \
             copy of the project broke them: please report it at {}. The \
             copy leaves out gitignored directories and gitignored files \
             outside Python packages, so a test that reads one of those \
             fails only here.{}\n\n\
             Common causes: {}\n\n\
             --- test output (tail) ---\n{output}",
            crate::ISSUES_URL,
            if cfg.private_home {
                "\n\nfermut runs tests with a private, empty HOME, so a suite \
                 that reads ~/.gitconfig, credentials or caches under your \
                 home fails here; --no-private-home (private_home = false) \
                 keeps your real home."
            } else {
                ""
            },
            crate::TROUBLESHOOTING_URL,
        )),
    }
}

/// Fixed part of the `--timeout-factor` budget: interpreter start, imports and
/// collection that a tiny suite's wall time barely reflects, plus slack for
/// running beside other workers. ponytail: a constant (PIT and Stryker use
/// 4–5 s too); make it configurable if a project needs more.
const BUDGET_FLOOR_SECS: f64 = 5.0;

/// Per-mutant timeout under `--timeout-factor`: `factor × baseline + floor`,
/// rounded up to whole seconds and never above the configured `--timeout`.
/// The baseline ran the whole suite alone; a mutant may rerun all of it beside
/// other workers, hence the factor floor of 2 in `build_config`.
fn budget_timeout_secs(timeout_secs: u64, factor: f64, baseline: Duration) -> u64 {
    let budget = (factor * baseline.as_secs_f64() + BUDGET_FLOOR_SECS).ceil() as u64;
    budget.min(timeout_secs)
}

/// The `--timeout-factor` budget for the runner. `None` without a factor or a
/// baseline wall time to scale.
fn budgeted_timeout_secs(cfg: &Config, baseline_wall: Option<Duration>) -> Option<u64> {
    let (Some(factor), Some(wall)) = (cfg.timeout_factor, baseline_wall) else {
        return None;
    };
    let secs = budget_timeout_secs(cfg.timeout_secs, factor, wall);
    info!(
        baseline_secs = format!("{:.1}", wall.as_secs_f64()),
        timeout_secs = secs,
        "per-mutant timeout sized from the baseline (--timeout-factor)"
    );
    Some(secs)
}

/// The per-mutant memory cap, if any. Auto needs `peak`, the baseline's peak
/// RSS — only measured on Linux, after a baseline fermut ran itself.
fn memory_cap_bytes(cap: MemoryCap, peak: Option<u64>) -> Option<u64> {
    match cap {
        MemoryCap::Off => None,
        MemoryCap::Bytes(b) => Some(b),
        MemoryCap::Auto => peak.map(runner::memory::auto_cap),
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
    evaluate_all_with(cfg, mutants, |m| evaluate(ctx, m))
}

/// [`evaluate_all`] with a custom per-mutant step: parallel, in input order,
/// with the progress bar.
fn evaluate_all_with(
    cfg: &Config,
    mutants: &[Arc<Mutant>],
    step: impl Fn(&Arc<Mutant>) -> MutantOutcome + Sync,
) -> Result<Vec<MutantOutcome>> {
    let progress = make_progress_bar(mutants.len() as u64);
    let pool = build_pool(cfg.jobs)?;
    let outcomes: Vec<MutantOutcome> = pool.install(|| {
        // One mutant per rayon task: a default split hands each thread a
        // contiguous range it runs to the end, unstealable. Mutant cost is
        // clumpy (skips are free, a file's survivors slow), so one heavy
        // range left a single worker grinding alone — `--select-tests` on
        // pyjwt spent 78 of 128 s with one pytest running. Per-item tasks
        // cost nothing next to a ~300 ms pytest spawn.
        mutants
            .par_iter()
            .with_max_len(1)
            .map(|m| {
                let outcome = step(m);
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
fn summarize_integrity(cfg: &Config, cache: &Mutex<Cache>, stats: &CacheStats) {
    if !cfg.cache {
        return;
    }
    let guard = lock_recover(cache);
    let dropped = guard.dropped_on_load();
    let entries = guard.len();
    if let Err(e) = guard.save(&cfg.cache_path) {
        warn!(path = %cfg.cache_path.display(), error = %e, "failed to save cache");
    }
    let full = stats.full_hits.load(Ordering::Relaxed);
    let killer = stats.killer_hits.load(Ordering::Relaxed);
    let audited = stats.audited.load(Ordering::Relaxed);
    let audit_dropped = stats.dropped.load(Ordering::Relaxed);
    let misses = stats.misses.load(Ordering::Relaxed);
    let deltas = stats.delta_hits.load(Ordering::Relaxed);
    let delta_tests = stats.delta_tests.load(Ordering::Relaxed);
    let delta_full = stats.delta_full_tests.load(Ordering::Relaxed);
    let delta_audited = stats.delta_audited.load(Ordering::Relaxed);
    let delta_disagreed = stats.delta_disagreed.load(Ordering::Relaxed);
    info!(
        entries,
        dropped_on_load = dropped,
        full_hits = full,
        killer_hits = killer,
        audited,
        audit_dropped,
        survivor_deltas = deltas,
        delta_tests,
        delta_full_tests = delta_full,
        delta_audited,
        delta_disagreed,
        misses,
        "cache: {full} full hits, {killer} killer hits, {audited} audited \
         ({audit_dropped} dropped), {deltas} survivor deltas ({delta_tests} of \
         {delta_full} tests run, {delta_audited} audited, {delta_disagreed} \
         disagreed), {misses} misses"
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
    /// Per-test executed-code digests for the `coverage` cache mode.
    cov_identity: Option<&'a CovIdentity>,
    /// Top-level def/class positions per file, for [`cache_key`].
    anchors: &'a HashMap<PathBuf, Anchors>,
    scope_prefix: &'a str,
    test_tree: &'a TestTree,
    equiv: Option<&'a EquivPipeline>,
    file_sources: &'a HashMap<PathBuf, String>,
    /// Testing-phase wall-clock ceiling. Identical for every mutant in the
    /// run; a mutant reaching `evaluate` past it is a cheap `time-budget` skip.
    deadline: Option<Instant>,
    stats: &'a CacheStats,
    /// Killer hits served this run, for [`audit_killer_hits`].
    killer_hits: &'a Mutex<Vec<KillerHit>>,
}

fn evaluate(ctx: &EvalCtx, mutant: &Arc<Mutant>) -> MutantOutcome {
    precheck(ctx, mutant).unwrap_or_else(|| evaluate_admitted(ctx, mutant))
}

/// The time budget and the filter chain: `Some(skipped)` when `mutant` must
/// not be tested, `None` when it is admitted.
fn precheck(ctx: &EvalCtx, mutant: &Arc<Mutant>) -> Option<MutantOutcome> {
    // Testing-phase budget check first, before the filter chain or the runner.
    // Past the deadline every remaining mutant is a cheap skip — including the
    // ty pre-filter, the dominant per-mutant cost — so the wall-clock ceiling
    // actually holds. In-flight mutants that passed this check already run to
    // completion; only not-yet-started ones short-circuit here.
    if let Some(deadline) = ctx.deadline {
        if Instant::now() >= deadline {
            return Some(MutantOutcome::skipped(mutant.clone(), TIME_BUDGET_FILTER));
        }
    }

    // Filter chain first — skip decisions depend on current filter config,
    // so we can't satisfy them from cache.
    for f in ctx.filters {
        match f.admits(mutant) {
            Ok(true) => continue,
            Ok(false) => return Some(MutantOutcome::skipped(mutant.clone(), f.name())),
            Err(e) => {
                warn!(filter = f.name(), error = %e, "filter errored; admitting mutant");
            }
        }
    }
    None
}

/// Cache lookup, then a real run, for a mutant the filter chain admitted.
fn evaluate_admitted(ctx: &EvalCtx, mutant: &Arc<Mutant>) -> MutantOutcome {
    let scope = compute_mutant_scope(ctx.scope_prefix, ctx.cfg, ctx.test_tree, mutant);
    let identity_hash = mutant_identity_hash(
        ctx.cfg,
        mutant,
        ctx.file_hashes,
        ctx.scope_maps,
        ctx.cov_identity,
    );
    let key = cache_key(mutant, ctx.anchors);

    if let Some(hash) = &identity_hash {
        let cached = lock_recover(ctx.cache).lookup_entry(&key, hash, &scope);
        if let Some((cached, checked)) = cached {
            // The key survives edits that move the mutant (see `cache_key`),
            // so the stored outcome may carry an old line and range: report
            // the mutant as it is now, and classify against today's source.
            let cached = cached.rebind(mutant.clone());
            ctx.stats.full_hits.fetch_add(1, Ordering::Relaxed);
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
                // Only a `Survived` is ever rewritten, so no killer scope.
                lock_recover(ctx.cache).insert_checked(
                    key.clone(),
                    hash.clone(),
                    scope.clone(),
                    outcome.clone(),
                    new_checked,
                    None,
                    survivor_basis(ctx.scope_prefix, ctx.cfg, ctx.test_tree, mutant),
                );
            }
            return outcome;
        }
        // Full key missed (typically: some covering test file changed). A
        // cached kill survives if the test that killed it is unchanged and
        // still covers the mutant — an edit elsewhere can't un-kill it. The
        // entry keeps its old `scope`, so the next run takes this path again
        // until the mutant is re-run.
        let killer_hit = lock_recover(ctx.cache).lookup_killer(&key, hash, |t| {
            compute_killer_scope(ctx.scope_prefix, ctx.cfg, ctx.test_tree, mutant, t)
        });
        if let Some((outcome, killer)) = killer_hit {
            ctx.stats.killer_hits.fetch_add(1, Ordering::Relaxed);
            lock_recover(ctx.killer_hits).push(KillerHit {
                mutant: mutant.clone(),
                killer,
            });
            return outcome.rebind(mutant.clone());
        }
        // A cached survivor whose covering tests changed: every test it
        // passed before still passes, so run only the new or changed ones.
        if let Some(outcome) = survivor_delta(ctx, mutant, &key, hash, &scope) {
            return outcome;
        }
        ctx.stats.misses.fetch_add(1, Ordering::Relaxed);
    }

    run_and_cache(ctx, mutant, key, scope, identity_hash)
}

/// Run `mutant` for real, remap it through the equiv detector, and cache the
/// verdict under `(identity_hash, scope)` — plus the killer scope when the
/// runner named the killing test.
fn run_and_cache(
    ctx: &EvalCtx,
    mutant: &Arc<Mutant>,
    key: String,
    scope: String,
    identity_hash: Option<String>,
) -> MutantOutcome {
    let outcome = ctx
        .runner
        .run(mutant)
        .unwrap_or_else(|e| MutantOutcome::error(mutant.clone(), e.to_string()));
    record(ctx, mutant, key, scope, identity_hash, outcome)
}

/// Remap a fresh `outcome` through the equiv detector and cache it under
/// `(key, identity_hash, scope)`, with the killer scope for a kill and the
/// survivor basis for a survivor.
fn record(
    ctx: &EvalCtx,
    mutant: &Arc<Mutant>,
    key: String,
    scope: String,
    identity_hash: Option<String>,
    outcome: MutantOutcome,
) -> MutantOutcome {
    let outcome = maybe_remap_equivalent(outcome, ctx.equiv, ctx.file_sources);

    if let Some(hash) = identity_hash {
        let killer_scope = outcome.killer().and_then(|t| {
            compute_killer_scope(ctx.scope_prefix, ctx.cfg, ctx.test_tree, mutant, t)
        });
        let basis = matches!(
            outcome,
            MutantOutcome::Survived { .. } | MutantOutcome::Equivalent { .. }
        )
        .then(|| survivor_basis(ctx.scope_prefix, ctx.cfg, ctx.test_tree, mutant))
        .flatten();
        // A detector-on run classified any surviving mutant just above, so
        // the stored outcome is already post-equiv — mark it checked so warm
        // runs never re-classify it. A detector-off run leaves it unchecked.
        lock_recover(ctx.cache).insert_checked(
            key,
            hash,
            scope,
            outcome.clone(),
            ctx.equiv.is_some(),
            killer_scope,
            basis,
        );
    }

    outcome
}

/// The covering tests a survivor's verdict rests on (see
/// [`SurvivorBasis`]), when they are known precisely: coverage contexts name
/// them and every one's file can be fingerprinted (the same conditions as
/// [`scoped_test_fingerprint`]). `None` otherwise — no delta then.
fn survivor_basis(
    prefix: &str,
    cfg: &Config,
    tree: &TestTree,
    mutant: &Mutant,
) -> Option<SurvivorBasis> {
    let cov = cfg.coverage.as_ref()?;
    let ids = cov.tests_for_mutant(mutant)?;
    let mut files = std::collections::BTreeMap::new();
    for id in ids {
        let unit = crate::cache::test_unit(id);
        if !files.contains_key(unit) {
            let fp = scoped_test_fingerprint(
                cfg,
                tree,
                cov.node_root(),
                std::slice::from_ref(id),
                true,
            )?;
            files.insert(unit.to_string(), fp);
        }
    }
    Some(SurvivorBasis {
        prefix: prefix.to_string(),
        tests: ids.iter().cloned().collect(),
        files,
    })
}

/// Seed for the survivor-delta audit sample (see [`survivor_delta`]).
const DELTA_AUDIT_SEED: u64 = 0xde17_a5ed;

/// Re-check a cached survivor against only the covering tests that are new or
/// changed since its verdict. `None` when there is no usable cached survivor,
/// the run shape changed, or the runner can't select tests: the caller then
/// runs the mutant fully.
///
/// A survivor passed every covering test; an unchanged test still passes and a
/// removed one can't kill it. The delta runs new tests without the old ones
/// before them, so an order-dependent kill can be missed: a deterministic
/// sample (`--cache-audit-rate`, and the first delta of each run) also runs
/// against every covering test, and that verdict stands.
fn survivor_delta(
    ctx: &EvalCtx,
    mutant: &Arc<Mutant>,
    key: &str,
    hash: &str,
    scope: &str,
) -> Option<MutantOutcome> {
    // Recording needs every covering test's verdict, not a subset.
    if ctx.cfg.record_kill_sets.is_some() {
        return None;
    }
    let (cached, checked, old) = lock_recover(ctx.cache).lookup_survivor(key, hash)?;
    let current = survivor_basis(ctx.scope_prefix, ctx.cfg, ctx.test_tree, mutant)?;
    if current.prefix != old.prefix {
        return None;
    }
    let delta = current.delta_from(&old);
    let full = current.tests.len();

    if delta.is_empty() {
        // Covering tests only went away: everything left already passed.
        let cached = cached.rebind(mutant.clone());
        let (outcome, rewrite) = remap_cached_outcome(cached, checked, ctx.equiv, ctx.file_sources);
        ctx.stats.delta_hits.fetch_add(1, Ordering::Relaxed);
        ctx.stats
            .delta_full_tests
            .fetch_add(full, Ordering::Relaxed);
        lock_recover(ctx.cache).insert_checked(
            key.to_string(),
            hash.to_string(),
            scope.to_string(),
            outcome.clone(),
            rewrite.unwrap_or(checked),
            None,
            Some(current),
        );
        return Some(outcome);
    }

    let ran = match ctx.runner.run_tests(mutant, &delta) {
        Ok(Some(o)) if !matches!(o, MutantOutcome::Error { .. }) => o,
        _ => return None,
    };
    ctx.stats.delta_hits.fetch_add(1, Ordering::Relaxed);
    ctx.stats
        .delta_tests
        .fetch_add(delta.len(), Ordering::Relaxed);
    ctx.stats
        .delta_full_tests
        .fetch_add(full, Ordering::Relaxed);

    let rate = ctx.cfg.cache_audit_rate;
    let audit = matches!(ran, MutantOutcome::Survived { .. })
        && rate > 0.0
        && (!ctx
            .stats
            .delta_first_audit_done
            .swap(true, Ordering::Relaxed)
            || crate::filter::sample::score(DELTA_AUDIT_SEED, key) < rate);
    let ran = if audit {
        ctx.stats.delta_audited.fetch_add(1, Ordering::Relaxed);
        match ctx.runner.run(mutant) {
            Ok(full_run) if !matches!(full_run, MutantOutcome::Error { .. }) => {
                if matches!(
                    full_run,
                    MutantOutcome::Killed { .. } | MutantOutcome::TimedOut { .. }
                ) {
                    ctx.stats.delta_disagreed.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        mutant = %mutant.id,
                        new_tests = ?delta,
                        "cache audit: a survivor re-checked against only its new tests \
                         survived, but the full covering set detects it (order-dependent \
                         tests?); using the full run"
                    );
                }
                full_run
            }
            _ => ran,
        }
    } else {
        ran
    };
    Some(record(
        ctx,
        mutant,
        key.to_string(),
        scope.to_string(),
        Some(hash.to_string()),
        ran,
    ))
}

/// Re-verify a random sample of this run's killer hits by re-running each
/// mutant against only its recorded killer. Trusting a killer hit skips the
/// run entirely, so this is the safety net for kills that won't reproduce on
/// their own — a flaky test, or one that failed only because of state an
/// earlier test in the same `-x` run left behind. Samples
/// `ceil(rate × hits)` (at least one) hits; each disagreement drops the
/// cache entry, warns with the test and mutant, and re-runs the mutant fully,
/// replacing its outcome. Skipped when the rate is 0, no hits occurred, or
/// the `--max-time` budget is already spent.
fn audit_killer_hits(
    cfg: &Config,
    ctx: &EvalCtx,
    mutants: &[Arc<Mutant>],
    outcomes: &mut [MutantOutcome],
) -> Result<()> {
    let hits = std::mem::take(&mut *lock_recover(ctx.killer_hits));
    let picked = sample_for_audit(hits, cfg.cache_audit_rate);
    if picked.is_empty() {
        return Ok(());
    }
    if ctx.deadline.is_some_and(|d| Instant::now() >= d) {
        info!(
            pending = picked.len(),
            "cache audit skipped: --max-time budget exhausted"
        );
        return Ok(());
    }
    let pool = build_pool(cfg.jobs)?;
    let reruns: Vec<(usize, MutantOutcome)> = pool.install(|| {
        picked
            .par_iter()
            .filter_map(|hit| audit_one(ctx, hit))
            .filter_map(|outcome| {
                let id = &outcome.mutant().id;
                mutants
                    .iter()
                    .position(|m| &m.id == id)
                    .map(|i| (i, outcome))
            })
            .collect()
    });
    for (i, outcome) in reruns {
        outcomes[i] = outcome;
    }
    Ok(())
}

/// Audit one killer hit. `None` when the kill reproduced (or the runner can't
/// audit); otherwise the mutant's fresh full-run outcome.
fn audit_one(ctx: &EvalCtx, hit: &KillerHit) -> Option<MutantOutcome> {
    let verdict = match ctx.runner.audit(&hit.mutant, &hit.killer) {
        Ok(None) => return None,
        Ok(Some(v)) => v,
        Err(e) => MutantOutcome::error(hit.mutant.clone(), e.to_string()),
    };
    ctx.stats.audited.fetch_add(1, Ordering::Relaxed);
    if matches!(verdict, MutantOutcome::Killed { .. }) {
        return None;
    }
    ctx.stats.dropped.fetch_add(1, Ordering::Relaxed);
    warn!(
        mutant = %hit.mutant.id,
        file = %hit.mutant.file.display(),
        line = hit.mutant.line,
        killer = %hit.killer,
        audit = verdict.status_label(),
        "cache audit: cached kill did not reproduce against its killer alone \
         (flaky or order-dependent test?); re-running the mutant"
    );
    let key = cache_key(&hit.mutant, ctx.anchors);
    lock_recover(ctx.cache).remove(&key);
    let scope = compute_mutant_scope(ctx.scope_prefix, ctx.cfg, ctx.test_tree, &hit.mutant);
    let identity_hash = mutant_identity_hash(
        ctx.cfg,
        &hit.mutant,
        ctx.file_hashes,
        ctx.scope_maps,
        ctx.cov_identity,
    );
    Some(run_and_cache(ctx, &hit.mutant, key, scope, identity_hash))
}

/// Pick `ceil(rate × n)` of `hits` (at least one when `rate > 0`) uniformly at
/// random. Uses the process-random `RandomState` hasher as the shuffle key, so
/// each run audits a different sample without a `rand` dependency.
fn sample_for_audit(mut hits: Vec<KillerHit>, rate: f64) -> Vec<KillerHit> {
    if rate <= 0.0 || hits.is_empty() {
        return Vec::new();
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let k = ((rate * hits.len() as f64).ceil() as usize).clamp(1, hits.len());
    let seed = std::collections::hash_map::RandomState::new();
    hits.sort_by_cached_key(|h| std::hash::BuildHasher::hash_one(&seed, &h.mutant.id));
    hits.truncate(k);
    hits
}

/// The cache-key file-identity hash for `mutant`. In `CacheScope::Scope`
/// mode this is the per-enclosing-scope composite; otherwise it's the
/// file-AST hash. Returns `None` when no hash is available for the file
/// (e.g., I/O error during the eager hash pass) so the caller skips the
/// cache for that mutant.
/// The key a mutant's verdict is cached under.
///
/// `mutant.id` embeds the absolute byte offset, so any edit above a mutant —
/// a line added to an earlier function, a comment above its `def` — gives it
/// a new id and its cached verdict is never found again. Inside a top-level
/// def/class the key uses the offset from that def's start instead:
/// `file::bar@12:boundary-shift:>=->>`. Whether the verdict still holds is
/// the identity hash's and scope's job, not the key's. Module-level mutants
/// (and files that didn't parse) keep `mutant.id`, so their entries from
/// before this key existed still hit.
fn cache_key(mutant: &Mutant, anchors: &HashMap<PathBuf, Anchors>) -> String {
    match anchors
        .get(&mutant.file)
        .and_then(|a| a.locate(mutant.range))
    {
        Some((def, offset)) => format!(
            "{}::{def}@{offset}:{}:{}->{}",
            mutant.file.display(),
            mutant.operator.name(),
            mutant.original,
            mutant.replacement
        ),
        None => mutant.id.clone(),
    }
}

fn mutant_identity_hash(
    cfg: &Config,
    mutant: &Mutant,
    file_hashes: &HashMap<PathBuf, String>,
    scope_maps: &HashMap<PathBuf, ScopeMap>,
    cov: Option<&CovIdentity>,
) -> Option<String> {
    if let (Some(cov), Some(ctx)) = (cov, &cfg.coverage) {
        if let Some(h) = cov.hash_for(ctx, mutant) {
            return Some(h);
        }
    }
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
            return MutantOutcome::survived(mutant);
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
    let MutantOutcome::Survived { mutant, .. } = &outcome else {
        return outcome;
    };
    let Some(source) = file_sources.get(&mutant.file) else {
        return outcome;
    };
    match pipeline.classify(mutant, source) {
        EquivVerdict::ProvablyEquivalent {
            reason,
            source: src,
        } => MutantOutcome::equivalent(mutant.clone(), reason, src),
        _ => outcome,
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
            return (MutantOutcome::survived(mutant), None);
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
    // v8 also folds in the private-home setting (backlog #57) and the pinned
    // `PYTHONHASHSEED` (`runner::hash_seed`):
    // entries from random-seed runs carry order-dependent verdicts.
    //
    // v8: `|python=` hashes the interpreter actually used, by identity
    // (`interpreter_identity`), not the configured path's spelling — the same
    // venv reached via `$VIRTUAL_ENV`, `PATH` or `x/../.venv` keyed three
    // different caches that evicted each other.
    //
    // v7: `Killed` entries also carry a killer scope (see
    // `compute_killer_scope`) and the runner names the killing test.
    //
    // v6: the test-suite fingerprint moved out of the prefix and into each
    // mutant's scope (see `compute_mutant_scope`), scoped to the tests that
    // cover the mutant, so an edit to an unrelated test keeps its verdict.
    //
    // v5: folds in the resolved Python interpreter (`|python=` below) — a
    // different interpreter (or its venv's package set) can flip a mutant's
    // verdict, so a `--python` change must not reuse another interpreter's
    // cached outcomes.
    //
    // v3: cache-scope mode now affects how the file-identity hash is
    // computed. Mix the mode into the scope prefix so a `file`-mode entry
    // can't satisfy a `scope`-mode lookup (and vice versa) — the two modes
    // produce different identity hashes for the same source.
    h.update(b"v8");
    h.update(b"|cache_scope=");
    h.update(format!("{:?}", cfg.cache_scope).as_bytes());
    h.update(b"|runner=");
    // Schemata shares the pytest tag: its verdicts match a normal run's, and
    // a cache built either way serves the other.
    h.update(runner_cache_tag(cfg.runner).as_bytes());
    h.update(b"|python=");
    let used = match cfg.runner {
        RunnerKind::Pytest => crate::runner::python::pytest_interpreter(cfg.python.as_deref()),
        RunnerKind::Rstest => crate::runner::python::rstest_interpreter(cfg.python.as_deref())
            .unwrap_or_else(|| crate::runner::python::interpreter(None)),
        RunnerKind::Unittest => crate::runner::python::interpreter(cfg.python.as_deref()),
    };
    h.update(crate::runner::python::interpreter_identity(&used).as_bytes());
    h.update(b"|timeout=");
    h.update(cfg.timeout_secs.to_le_bytes());
    // A budgeted timeout can turn a slow survivor into a timeout, so the
    // factor (not the per-run budget, which moves with the baseline) scopes
    // the verdicts. Fed only when set: existing entries keep their scope.
    if let Some(f) = cfg.timeout_factor {
        h.update(b"|timeout_factor=");
        h.update(f.to_le_bytes());
    }
    // An explicit cap can turn a survivor into a kill; the auto cap is sized
    // so it only turns a timeout into one (both detected), so it isn't fed.
    if let MemoryCap::Bytes(b) = cfg.max_memory {
        h.update(b"|max_memory=");
        h.update(b.to_le_bytes());
    }
    h.update(b"|hseed=");
    if let Some(s) = cfg.hypothesis_seed {
        h.update(s.to_le_bytes());
    } else {
        h.update(b"none");
    }
    h.update(b"|pyhashseed=");
    h.update(crate::runner::hash_seed().as_bytes());
    // A shared real home let tests race across workers (false kills).
    h.update(if cfg.private_home {
        &b"|home=private"[..]
    } else {
        &b"|home=real"[..]
    });
    h.update(b"|args=");
    for a in &cfg.pytest_args {
        h.update(b"|");
        h.update(a.as_bytes());
    }
    // Which tests unittest discovers and where it runs them. Fed only when
    // set, so existing entries keep their scope.
    if let Some(p) = &cfg.unittest_pattern {
        h.update(b"|ut_pattern=");
        h.update(p.as_bytes());
    }
    for (tag, dir) in [
        (&b"|ut_top="[..], &cfg.unittest_top_level),
        (&b"|ut_cwd="[..], &cfg.unittest_cwd),
    ] {
        if let Some(d) = dir {
            h.update(tag);
            h.update(d.as_os_str().as_encoded_bytes());
        }
    }
    h.update(b"|cov=");
    h.update(if cfg.coverage.is_some() {
        &b"on"[..]
    } else {
        &b"off"[..]
    });
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

/// Per-mutant final scope: prefix + sorted selected test ids (when
/// coverage-driven selection is on) + a fingerprint of the tests that run
/// against the mutant.
///
/// A mutant's outcome depends on the tests as much as on its source: editing a
/// test (without touching the source AST) can turn a survivor into a kill, and
/// without the fingerprint an agent's "add the killing test, re-run" loop would
/// keep reading the stale verdict. When the covering tests are known precisely,
/// only their files (plus conftest chain and shared support files, see
/// [`TestTree::scoped`]) are fingerprinted, so editing an unrelated test keeps
/// the verdict. Otherwise the whole tree is — any test edit invalidates. The
/// covering id set itself is folded in too, so a new test that starts covering
/// the line changes the key even when no existing file changed.
fn compute_mutant_scope(prefix: &str, cfg: &Config, tree: &TestTree, mutant: &Mutant) -> String {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    let covering = cfg
        .coverage
        .as_ref()
        .and_then(|ctx| ctx.tests_for_mutant(mutant).map(|ids| (ctx, ids)));
    if let Some((_, tests)) = covering {
        let mut sorted: Vec<&String> = tests.iter().collect();
        sorted.sort();
        for t in sorted {
            h.update(b"|");
            h.update(t.as_bytes());
        }
    }
    let scoped = covering
        .and_then(|(ctx, ids)| scoped_test_fingerprint(cfg, tree, ctx.node_root(), ids, false));
    match scoped {
        Some(fp) => {
            h.update(b"|tests:scoped=");
            h.update(fp.as_bytes());
        }
        None => {
            h.update(b"|tests:tree=");
            h.update(tree.whole().as_bytes());
        }
    }
    hex::encode(h.finalize())
}

/// Killer scope for `mutant` killed by test `killer`: the run-shape prefix,
/// the killer's node id, and the scoped fingerprint of just the killer's test
/// (its source, the non-test statements around it, its conftest chain and
/// shared support files; see [`TestTree::scoped`]). A cached kill stays valid
/// while this is unchanged, whatever happens to the other covering tests,
/// including ones in the killer's file. `None` — no killer-keyed reuse — when
/// `killer` no longer covers the mutant (deleted, renamed, or a refreshed
/// coverage DB that doesn't reach the line) or its file can't be fingerprinted
/// precisely (see [`scoped_test_fingerprint`]).
fn compute_killer_scope(
    prefix: &str,
    cfg: &Config,
    tree: &TestTree,
    mutant: &Mutant,
    killer: &str,
) -> Option<String> {
    let ctx = cfg.coverage.as_ref()?;
    let ids = ctx.tests_for_mutant(mutant)?;
    let killer_id = ids.iter().find(|t| *t == killer)?;
    let fp = scoped_test_fingerprint(
        cfg,
        tree,
        ctx.node_root(),
        std::slice::from_ref(killer_id),
        true,
    )?;
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    // v2: the fingerprint narrows the killer's file to the killer's test.
    h.update(b"|killer:v2=");
    h.update(killer.as_bytes());
    h.update(b"|tests:scoped=");
    h.update(fp.as_bytes());
    Some(hex::encode(h.finalize()))
}

/// Fingerprint of only the test files behind `ids`, or `None` when the set of
/// tests that run against the mutant isn't known precisely and the caller must
/// fall back to the whole tree:
///
/// - `unittest` runner — it ignores coverage selection and runs the full suite;
/// - empty selection — the runner then sweeps the whole tests dir;
/// - a covering file outside the tests tree, or one the walk didn't hash.
///
/// `by_test` narrows each file to the tests in `ids` (see [`TestTree::scoped`]).
fn scoped_test_fingerprint(
    cfg: &Config,
    tree: &TestTree,
    node_root: &Path,
    ids: &[String],
    by_test: bool,
) -> Option<String> {
    if matches!(cfg.runner, RunnerKind::Unittest) || ids.is_empty() {
        return None;
    }
    let abs: Vec<PathBuf> = ids
        .iter()
        .map(|id| node_root.join(crate::cache::test_file(id)))
        .collect();
    let rel: Vec<(&Path, Option<&str>)> = abs
        .iter()
        .zip(ids)
        .map(|(p, id)| {
            let test = by_test
                .then(|| crate::cache::test_unit(id).split_once("::"))
                .flatten()
                .map(|(_, t)| t);
            tree.relative(p).map(|r| (r, test))
        })
        .collect::<Option<_>>()?;
    Some(tree.scoped(rel))
}

/// Per-file artifacts for a run, keyed by source path. Built in one pass by
/// [`analyze_unique_files`].
#[derive(Default)]
struct FileArtifacts {
    /// Cache-key AST hash for every unique file (byte fallback on parse/utf-8
    /// failure). Always populated.
    hashes: HashMap<PathBuf, String>,
    /// Scope maps for the `scope` cache mode. Empty otherwise; files that fail
    /// to parse are absent (consumers fall back to the file hash).
    scope_maps: HashMap<PathBuf, ScopeMap>,
    /// Top-level def/class positions, for [`cache_key`]. Files that failed to
    /// parse are absent (their mutants keep `mutant.id` as the key).
    anchors: HashMap<PathBuf, Anchors>,
    /// Source text for the equiv detector, so classification doesn't re-read
    /// the file per mutant. Empty when equiv detection is off.
    sources: HashMap<PathBuf, String>,
    /// `coverage` cache mode with a coverage DB; `None` otherwise.
    cov: Option<CovIdentity>,
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
    let mut anchors = HashMap::new();
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
        if let Some(a) = analysis.anchors {
            anchors.insert(m.file.clone(), a);
        }
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
        anchors,
        sources,
        cov: None,
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

    #[test]
    fn schemata_shares_the_normal_runs_cache() {
        // Same verdicts by design, so turning schemata on or off keeps the
        // cache warm instead of re-running everything.
        let on = Config {
            schemata: true,
            ..base_test_config()
        };
        let off = Config {
            schemata: false,
            ..base_test_config()
        };
        assert_eq!(compute_scope_prefix(&on), compute_scope_prefix(&off));
        assert_eq!(runner_cache_tag(RunnerKind::Pytest), "pytest");
    }

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
            site: None,
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
            MutantOutcome::Survived { mutant, .. } => assert_eq!(mutant.id, m.id),
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
            site: None,
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
        let scope_a = mutant_identity_hash(&scope_cfg, &m, &files_a, &maps_a, None).unwrap();
        let scope_b = mutant_identity_hash(&scope_cfg, &m, &files_b, &maps_b, None).unwrap();
        assert_eq!(
            scope_a, scope_b,
            "scope-mode bar hash must survive foo edit"
        );

        // File mode: any AST change anywhere invalidates the hash.
        let file_a = mutant_identity_hash(&file_cfg, &m, &files_a, &maps_a, None).unwrap();
        let file_b = mutant_identity_hash(&file_cfg, &m, &files_b, &maps_b, None).unwrap();
        assert_ne!(file_a, file_b, "file-mode bar hash must change on foo edit");
    }

    /// Project fixture for per-mutant scope tests:
    ///
    /// ```text
    /// pyproject.toml
    /// src/calc.py                      (mutated; line 1 covered by test_a, line 2 by test_b)
    /// tests/conftest.py
    /// tests/helpers.py
    /// tests/a/conftest.py
    /// tests/a/test_a.py
    /// tests/b/conftest.py
    /// tests/b/test_b.py
    /// ```
    struct ScopeFixture {
        tmp: tempfile::TempDir,
    }

    impl ScopeFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let fx = Self { tmp };
            fx.write("pyproject.toml", "");
            fx.write("src/calc.py", "x = 1 + 1\ny = 2 + 2\nz = 3 + 3\n");
            fx.write("tests/conftest.py", "root = 1\n");
            fx.write("tests/helpers.py", "def h(): pass\n");
            fx.write("tests/a/conftest.py", "a = 1\n");
            fx.write("tests/a/test_a.py", "def test_a(): pass\n");
            fx.write("tests/b/conftest.py", "b = 1\n");
            fx.write("tests/b/test_b.py", "def test_b(): pass\n");
            fx.write_coverage(&[
                (1, &["tests/a/test_a.py::test_a"]),
                (2, &["tests/b/test_b.py::test_b"]),
            ]);
            fx
        }

        fn root(&self) -> &Path {
            self.tmp.path()
        }

        fn write(&self, rel: &str, body: &str) {
            let p = self.root().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }

        fn write_coverage(&self, lines: &[(u32, &[&str])]) {
            let contexts: serde_json::Map<String, serde_json::Value> = lines
                .iter()
                .map(|(line, ids)| {
                    let ids: Vec<String> = ids.iter().map(|id| format!("{id}|run")).collect();
                    (line.to_string(), serde_json::json!(ids))
                })
                .collect();
            let doc = serde_json::json!({
                "files": { "src/calc.py": { "contexts": contexts } }
            });
            self.write("coverage.json", &doc.to_string());
        }

        fn config(&self, runner: RunnerKind, with_coverage: bool) -> Config {
            use crate::filter::coverage::CoverageContexts;
            let coverage = with_coverage.then(|| {
                CoverageContexts::from_json(
                    &self.root().join("coverage.json"),
                    &self.root().join("src"),
                    self.root(),
                )
                .unwrap()
            });
            Config {
                source_root: self.root().join("src"),
                tests: Some(self.root().join("tests")),
                coverage,
                runner,
                ..base_test_config()
            }
        }

        fn mutant(&self, line: u32) -> Mutant {
            let mut m = arith_mutant("x = 1 + 1", "+", "-");
            m.file = self.root().join("src/calc.py");
            m.line = line;
            m.stmt_line = line;
            m
        }

        /// Scope of the mutant on `line` against the tree as it is now.
        fn scope(&self, cfg: &Config, line: u32) -> String {
            let tree = TestTree::walk(&cfg.tests_path());
            compute_mutant_scope(&compute_scope_prefix(cfg), cfg, &tree, &self.mutant(line))
        }
    }

    #[test]
    fn prepare_candidates_applies_only_cheap_filters() {
        struct F(&'static str, bool, fn(&Mutant) -> Result<bool>);
        impl Filter for F {
            fn name(&self) -> &'static str {
                self.0
            }
            fn admits(&self, m: &Mutant) -> Result<bool> {
                (self.2)(m)
            }
            fn costly(&self) -> bool {
                self.1
            }
            // The costly filter remembers rejecting line 4, nothing else.
            fn cached_verdict(&self, m: &Mutant) -> Option<bool> {
                (self.1 && m.line == 4).then_some(false)
            }
        }
        let mutants: Vec<Arc<Mutant>> = (1..=4)
            .map(|line| {
                let mut m = arith_mutant("x = 1 + 1", "+", "-");
                m.line = line;
                Arc::new(m)
            })
            .collect();
        let filters: Vec<Box<dyn Filter>> = vec![
            Box::new(F("cheap", false, |m| Ok(m.line != 2))),
            Box::new(F("erroring", false, |m| {
                if m.line == 3 {
                    anyhow::bail!("boom")
                } else {
                    Ok(true)
                }
            })),
            Box::new(F("costly", true, |_| Ok(false))),
        ];
        let lines: Vec<u32> = prepare_candidates(&filters, &mutants)
            .iter()
            .map(|m| m.line)
            .collect();
        // Line 2: cheap reject. Line 3: an error admits. The costly filter
        // (which rejects everything) isn't run, only its cached reject of
        // line 4 counts.
        assert_eq!(lines, [1, 3]);
    }

    #[test]
    fn scope_prefix_ignores_test_edits() {
        // The test fingerprint lives in the per-mutant scope now, not in the
        // prefix every entry shares.
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Pytest, true);
        let before = compute_scope_prefix(&cfg);
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        assert_eq!(before, compute_scope_prefix(&cfg));
    }

    #[test]
    fn scope_prefix_tracks_the_private_home() {
        let on = base_test_config();
        let off = Config {
            private_home: false,
            ..base_test_config()
        };
        assert_ne!(compute_scope_prefix(&on), compute_scope_prefix(&off));
    }

    #[cfg(unix)]
    #[test]
    fn scope_prefix_keys_the_interpreter_not_its_spelling() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("python3.13"), "").unwrap();
        for venv in ["a", "b"] {
            let bin = d.path().join(venv).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::os::unix::fs::symlink(d.path().join("python3.13"), bin.join("python")).unwrap();
        }
        let with = |p: &str| {
            compute_scope_prefix(&Config {
                python: Some(d.path().join(p)),
                ..base_test_config()
            })
        };
        assert_eq!(with("a/bin/python"), with("b/../a/bin/python"));
        assert_ne!(with("a/bin/python"), with("b/bin/python"));
    }

    #[test]
    fn scope_prefix_tracks_the_timeout_factor() {
        let mut cfg = base_test_config();
        let unset = compute_scope_prefix(&cfg);
        cfg.timeout_factor = Some(3.0);
        let three = compute_scope_prefix(&cfg);
        cfg.timeout_factor = Some(4.0);
        assert_ne!(unset, three);
        assert_ne!(three, compute_scope_prefix(&cfg));
    }

    #[test]
    fn scope_prefix_tracks_only_an_explicit_memory_cap() {
        let mut cfg = base_test_config();
        let auto = compute_scope_prefix(&cfg);
        cfg.max_memory = MemoryCap::Off;
        assert_eq!(
            auto,
            compute_scope_prefix(&cfg),
            "old caches keep their scope"
        );
        cfg.max_memory = MemoryCap::Bytes(1 << 30);
        assert_ne!(auto, compute_scope_prefix(&cfg));
    }

    #[test]
    fn memory_cap_is_explicit_or_sized_from_the_baseline_peak() {
        assert_eq!(memory_cap_bytes(MemoryCap::Off, Some(1 << 30)), None);
        assert_eq!(memory_cap_bytes(MemoryCap::Bytes(5), None), Some(5));
        assert_eq!(
            memory_cap_bytes(MemoryCap::Auto, None),
            None,
            "no baseline, no cap"
        );
        assert_eq!(
            memory_cap_bytes(MemoryCap::Auto, Some(1 << 30)),
            Some(4 << 30)
        );
    }

    #[test]
    fn mutant_scope_survives_edit_to_non_covering_test() {
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Pytest, true);
        let before = fx.scope(&cfg, 1);
        assert_eq!(before, fx.scope(&cfg, 1), "scope must be stable");
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        fx.write("tests/b/conftest.py", "b = 2\n");
        fx.write("tests/b/test_new.py", "def test_new(): pass\n");
        assert_eq!(
            before,
            fx.scope(&cfg, 1),
            "unrelated test edits keep the key"
        );
    }

    #[test]
    fn mutant_scope_changes_when_a_covering_test_is_edited() {
        // The core regression: editing a covering test (no source-AST change)
        // must invalidate the cached outcome, or an agent's add-test/re-run
        // loop reads the stale pre-edit verdict forever.
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Pytest, true);
        let before = fx.scope(&cfg, 1);
        fx.write("tests/a/test_a.py", "def test_a(): assert 1\n");
        assert_ne!(before, fx.scope(&cfg, 1));
    }

    #[test]
    fn mutant_scope_tracks_conftest_chain_and_support_files() {
        for (rel, body) in [
            ("tests/conftest.py", "root = 2\n"),
            ("tests/a/conftest.py", "a = 2\n"),
            ("tests/helpers.py", "def h(): return 1\n"),
            ("tests/b/data.json", "{}"),
        ] {
            let fx = ScopeFixture::new();
            let cfg = fx.config(RunnerKind::Pytest, true);
            let before = fx.scope(&cfg, 1);
            fx.write(rel, body);
            assert_ne!(before, fx.scope(&cfg, 1), "{rel} must invalidate");
        }
    }

    #[test]
    fn mutant_scope_changes_with_a_new_covering_test() {
        let fx = ScopeFixture::new();
        let before = fx.scope(&fx.config(RunnerKind::Pytest, true), 1);
        // Same files on disk, but test_b now also covers line 1.
        fx.write_coverage(&[
            (
                1,
                &["tests/a/test_a.py::test_a", "tests/b/test_b.py::test_b"],
            ),
            (2, &["tests/b/test_b.py::test_b"]),
        ]);
        assert_ne!(before, fx.scope(&fx.config(RunnerKind::Pytest, true), 1));
    }

    #[test]
    fn mutant_scope_resolves_parametrized_and_class_node_ids() {
        let fx = ScopeFixture::new();
        fx.write_coverage(&[(1, &["tests/a/test_a.py::TestA::test_x[1-2]"])]);
        let cfg = fx.config(RunnerKind::Pytest, true);
        let before = fx.scope(&cfg, 1);
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        assert_eq!(
            before,
            fx.scope(&cfg, 1),
            "resolved to test_a.py, not the tree"
        );
        fx.write("tests/a/test_a.py", "def test_a(): assert 1\n");
        assert_ne!(before, fx.scope(&cfg, 1));
    }

    #[test]
    fn mutant_scope_falls_back_to_whole_tree() {
        // Each case must make an unrelated test edit invalidate the key.
        let edit_unrelated = |fx: &ScopeFixture| {
            fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        };

        // No coverage.
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Pytest, false);
        let before = fx.scope(&cfg, 1);
        edit_unrelated(&fx);
        assert_ne!(before, fx.scope(&cfg, 1), "no coverage");

        // unittest ignores coverage selection and runs the full suite.
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Unittest, true);
        let before = fx.scope(&cfg, 1);
        edit_unrelated(&fx);
        assert_ne!(before, fx.scope(&cfg, 1), "unittest");

        // No recorded context for the line (line 3).
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Pytest, true);
        let before = fx.scope(&cfg, 3);
        edit_unrelated(&fx);
        assert_ne!(before, fx.scope(&cfg, 3), "no context");

        // Covering test outside the tests tree (a doctest-style id in src/).
        let fx = ScopeFixture::new();
        fx.write_coverage(&[(1, &["src/calc.py::calc"])]);
        let cfg = fx.config(RunnerKind::Pytest, true);
        let before = fx.scope(&cfg, 1);
        edit_unrelated(&fx);
        assert_ne!(before, fx.scope(&cfg, 1), "covering file outside tests/");
    }

    #[test]
    fn rstest_uses_scoped_fingerprint() {
        let fx = ScopeFixture::new();
        let cfg = fx.config(RunnerKind::Rstest, true);
        let before = fx.scope(&cfg, 1);
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        assert_eq!(before, fx.scope(&cfg, 1));
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
            mutate_migrations: false,
            parity: false,
            ops_allow: None,
            ops_deny: Default::default(),
            diff_base: None,
            since: None,
            coverage_path: None,
            coverage: None,
            coverage_unselected: None,
            coverage_note: None,
            hypothesis_seed: None,
            pytest_args: Vec::new(),
            cache: false,
            cache_audit_rate: 0.0,
            cache_path: PathBuf::from(".fermut/cache.json"),
            smart_order: false,
            pytest_plugin: false,
            schemata: false,
            kill_order_path: PathBuf::from(".fermut/kill-order.json"),
            dominators_path: PathBuf::from(".fermut/dominators.json"),
            operator_profile: crate::config::OperatorProfile::Classic,
            only_dominators: false,
            inferred_audit_rate: 0.0,
            history: false,
            history_path: PathBuf::from(".fermut/history.jsonl"),
            sample_ratio: None,
            sample_seed: None,
            shard: None,
            runner: RunnerKind::Pytest,
            python: None,
            unittest_pattern: None,
            unittest_top_level: None,
            unittest_cwd: None,
            isolation: IsolationMode::Auto,
            equiv_detect: false,
            cache_scope: CacheScope::File,
            fail_under: None,
            exclude: Vec::new(),
            ignore_patterns: Vec::new(),
            verify_baseline: false,
            private_home: true,
            baseline_timeout_secs: 300,
            max_time_secs: None,
            timeout_factor: None,
            baseline_secs: None,
            max_memory: MemoryCap::Auto,
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
            Ok(runner::BaselineStatus::Passed {
                wall: Duration::ZERO,
            })
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
            Ok(runner::BaselineStatus::Passed {
                wall: Duration::ZERO,
            })
        }
    }

    /// Runner that reports every mutant survived.
    struct SurviveRunner;
    impl Runner for SurviveRunner {
        fn run(&self, m: &Arc<Mutant>) -> Result<MutantOutcome> {
            Ok(MutantOutcome::survived(m.clone()))
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Passed {
                wall: Duration::ZERO,
            })
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
            cov_identity: None,
            anchors: &HashMap::new(),
            scope_prefix: "",
            test_tree: &TestTree::default(),
            equiv: None,
            file_sources: &HashMap::new(),
            deadline,
            stats: &CacheStats::default(),
            killer_hits: &Mutex::new(Vec::new()),
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

    /// One mutant on `>=` in `source`, written to `path`, as the visitor would
    /// emit it (absolute-offset id, 1-based line).
    fn ge_mutant_in(path: &std::path::Path, source: &str) -> Arc<Mutant> {
        std::fs::write(path, source).unwrap();
        let start = source.find(">=").unwrap();
        let line = source[..start].matches('\n').count() as u32 + 1;
        Arc::new(Mutant {
            id: format!("{}@{start}:boundary-shift:>=->>", path.display()),
            file: path.to_path_buf(),
            operator: Operator::BoundaryShift,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + 2) as u32),
            ),
            original: ">=".into(),
            replacement: ">".into(),
            line,
            stmt_line: line,
            site: None,
        })
    }

    /// Evaluate `m` against `cache` with fresh per-file artifacts, as a run
    /// over the file's current contents would.
    fn eval_in_file(
        cfg: &Config,
        cache: &Mutex<Cache>,
        runner: &dyn Runner,
        m: &Arc<Mutant>,
    ) -> MutantOutcome {
        let a = analyze_unique_files(
            std::slice::from_ref(m),
            matches!(cfg.cache_scope, CacheScope::Scope),
            false,
        );
        let ctx = EvalCtx {
            cfg,
            filters: &[],
            runner,
            cache,
            file_hashes: &a.hashes,
            scope_maps: &a.scope_maps,
            cov_identity: None,
            anchors: &a.anchors,
            scope_prefix: "",
            test_tree: &TestTree::default(),
            equiv: None,
            file_sources: &HashMap::new(),
            deadline: None,
            stats: &CacheStats::default(),
            killer_hits: &Mutex::new(Vec::new()),
        };
        evaluate(&ctx, m)
    }

    const BAR: &str = "def foo():\n    return 1\n\n\ndef bar(x):\n    return x >= 18\n";

    #[test]
    fn cached_verdict_survives_a_comment_above_the_def() {
        // File mode: a comment keeps the AST hash but moves every byte offset
        // below it, so `mutant.id` changes. The verdict must still be found,
        // and reported at the mutant's new line, not the cached one.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.py");
        let cfg = base_test_config();
        let cache = Mutex::new(Cache::default());

        let before = ge_mutant_in(&path, BAR);
        assert!(matches!(
            eval_in_file(&cfg, &cache, &KillRunner, &before),
            MutantOutcome::Killed { .. }
        ));

        let after = ge_mutant_in(&path, &format!("# a note\n{BAR}"));
        assert_ne!(before.id, after.id, "fixture: the id moved");
        // PanicRunner: a miss would run the mutant and panic.
        let out = eval_in_file(&cfg, &cache, &PanicRunner, &after);
        assert!(matches!(out, MutantOutcome::Killed { .. }), "{out:?}");
        assert_eq!(out.mutant().id, after.id);
        assert_eq!(out.mutant().line, after.line);
    }

    #[test]
    fn scope_mode_verdict_survives_a_line_added_to_a_sibling() {
        // The backlog case: a line added inside `foo` moved every `bar`
        // mutant to a new id, so scope mode re-ran them despite `bar`'s
        // unchanged scope hash.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.py");
        let cfg = Config {
            cache_scope: CacheScope::Scope,
            ..base_test_config()
        };
        let cache = Mutex::new(Cache::default());

        let before = ge_mutant_in(&path, BAR);
        eval_in_file(&cfg, &cache, &KillRunner, &before);
        let edited = BAR.replace("    return 1\n", "    y = 2\n    return y\n");
        let after = ge_mutant_in(&path, &edited);
        assert_ne!(before.id, after.id, "fixture: the id moved");
        let out = eval_in_file(&cfg, &cache, &PanicRunner, &after);
        assert!(matches!(out, MutantOutcome::Killed { .. }), "{out:?}");
        assert_eq!(out.mutant().line, after.line);
    }

    #[test]
    fn file_mode_still_reruns_after_a_code_edit() {
        // The key no longer changes on a sibling edit; the file hash must
        // still invalidate in the default mode.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.py");
        let cfg = base_test_config();
        let cache = Mutex::new(Cache::default());

        eval_in_file(&cfg, &cache, &KillRunner, &ge_mutant_in(&path, BAR));
        let edited = BAR.replace("    return 1\n", "    y = 2\n    return y\n");
        let out = eval_in_file(&cfg, &cache, &SurviveRunner, &ge_mutant_in(&path, &edited));
        assert!(
            matches!(out, MutantOutcome::Survived { .. }),
            "re-ran: {out:?}"
        );
    }

    #[test]
    fn cache_key_is_relative_to_the_top_level_def() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.py");
        let m = ge_mutant_in(&path, BAR);
        let a = analyze_unique_files(std::slice::from_ref(&m), false, false);
        let key = cache_key(&m, &a.anchors);
        let rel = BAR.find(">=").unwrap() - BAR.find("def bar").unwrap();
        assert!(
            key.ends_with(&format!("::bar@{rel}:boundary-shift:>=->>")),
            "{key}"
        );

        // Module level: no enclosing def, the id is the key.
        let top = ge_mutant_in(&path, "LIMIT = 1 >= 0\n");
        let a = analyze_unique_files(std::slice::from_ref(&top), false, false);
        assert_eq!(cache_key(&top, &a.anchors), top.id);
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

    /// `coverage`-mode identity hashes for a mutant on line 3 (`bar`, run by
    /// test_a, which also runs `util.helper`), one on line 5 (`foo`, run by
    /// test_b) and one on line 1 (import time: collecting `tests/t.py` also
    /// ran the `def foo` line), given the sources of `mod.py` and `util.py`.
    fn cov_hashes(module: &str, util: &str) -> [String; 3] {
        use crate::filter::coverage::CoverageContexts;
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("mod.py");
        std::fs::write(&py, module).unwrap();
        std::fs::write(tmp.path().join("util.py"), util).unwrap();
        let doc = r#"{"files": {
            "mod.py": {"contexts": {
                "1": ["tests/t.py|collect"], "4": ["tests/t.py|collect"],
                "3": ["tests/t.py::test_a|run"], "5": ["tests/t.py::test_b|run"]}},
            "util.py": {"contexts": {"2": ["tests/t.py::test_a|run"]}}
        }}"#;
        let cov_path = tmp.path().join("coverage.json");
        std::fs::write(&cov_path, doc).unwrap();
        let cov = CoverageContexts::from_json(&cov_path, tmp.path(), tmp.path()).unwrap();
        let cfg = Config {
            coverage: Some(cov.clone()),
            ..base_test_config()
        };
        let id = CovIdentity::build(&cov, &tmp.path().join("tests"), [py.as_path()]);
        [3, 5, 1].map(|line| {
            let mut m = arith_mutant("x + 1", "+", "-");
            m.file = py.clone();
            m.line = line;
            m.stmt_line = line;
            mutant_identity_hash(&cfg, &m, &HashMap::new(), &HashMap::new(), Some(&id)).unwrap()
        })
    }

    const MOD: &str =
        "import util\ndef bar(x):\n    return x + util.helper()\ndef foo(x):\n    return x * 2\n";
    const UTIL: &str = "def helper():\n    return 3\n";

    #[test]
    fn coverage_identity_tracks_only_the_code_a_mutants_tests_ran() {
        let [bar, foo, import] = cov_hashes(MOD, UTIL);

        // Editing foo's body: bar's tests never ran foo, and importing the
        // module ran only foo's `def` line, not its body.
        let [bar2, foo2, import2] = cov_hashes(&MOD.replace("x * 2", "x * 3"), UTIL);
        assert_eq!(bar, bar2);
        assert_ne!(foo, foo2);
        assert_eq!(import, import2);

        // Editing another file's function bar's test calls: bar re-runs, foo doesn't.
        let [bar3, foo3, _] = cov_hashes(MOD, &UTIL.replace('3', "4"));
        assert_ne!(bar, bar3);
        assert_eq!(foo, foo3);

        // Import-time code anywhere re-runs everything.
        let [bar4, foo4, import4] = cov_hashes(MOD, &format!("LIMIT = 1\n{UTIL}"));
        assert_ne!(bar, bar4);
        assert_ne!(foo, foo4);
        assert_ne!(import, import4);

        // A comment, or a new undecorated helper no test runs: nothing re-runs.
        let all = [bar, foo, import];
        let reformatted = MOD.replace("def foo(x):\n", "def foo(x):  # doubled\n");
        assert_eq!(cov_hashes(&reformatted, UTIL), all);
        let helper = format!("{MOD}def unused():\n    return 0\n");
        assert_eq!(cov_hashes(&helper, UTIL), all);
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
            site: None,
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

    // ---- killer-keyed cache ------------------------------------------------

    /// Scripted runner for the killer-cache tests: `run` kills by `killer`
    /// (or survives when `survive`), `audit` reports `audit_kills`. Counts
    /// both so a test can tell a cache hit (no run) from a miss.
    struct ScriptRunner {
        killer: Option<String>,
        survive: bool,
        audit_kills: bool,
        runs: AtomicUsize,
        audits: AtomicUsize,
    }

    impl ScriptRunner {
        fn killing(killer: Option<&str>) -> Self {
            Self {
                killer: killer.map(str::to_string),
                survive: false,
                audit_kills: true,
                runs: AtomicUsize::new(0),
                audits: AtomicUsize::new(0),
            }
        }
        fn surviving() -> Self {
            Self {
                survive: true,
                ..Self::killing(None)
            }
        }
        fn runs(&self) -> usize {
            self.runs.load(Ordering::Relaxed)
        }
    }

    impl Runner for ScriptRunner {
        fn run(&self, m: &Arc<Mutant>) -> Result<MutantOutcome> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(if self.survive {
                MutantOutcome::survived(m.clone())
            } else {
                MutantOutcome::killed_by(m.clone(), self.killer.clone())
            })
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Passed {
                wall: Duration::ZERO,
            })
        }
        fn audit(&self, m: &Arc<Mutant>, _killer: &str) -> Result<Option<MutantOutcome>> {
            self.audits.fetch_add(1, Ordering::Relaxed);
            Ok(Some(if self.audit_kills {
                MutantOutcome::killed(m.clone())
            } else {
                MutantOutcome::survived(m.clone())
            }))
        }
    }

    const TEST_A: &str = "tests/a/test_a.py::test_a";
    const TEST_B: &str = "tests/b/test_b.py::test_b";

    /// `ScopeFixture` whose line 1 is covered by both `test_a` and `test_b`.
    fn two_covering() -> ScopeFixture {
        let fx = ScopeFixture::new();
        fx.write_coverage(&[(1, &[TEST_A, TEST_B])]);
        fx
    }

    /// Evaluate the line-1 mutant once against the tree as it is now, with
    /// source identity `src_hash`. Returns the outcome and the run's stats;
    /// with `audit_rate`, also runs the post-evaluation audit.
    fn eval_cached(
        fx: &ScopeFixture,
        runner: &dyn Runner,
        cache: &Mutex<Cache>,
        src_hash: &str,
        audit_rate: Option<f64>,
    ) -> (MutantOutcome, CacheStats) {
        let cfg = Config {
            cache_audit_rate: audit_rate.unwrap_or(0.0),
            ..fx.config(RunnerKind::Pytest, true)
        };
        eval_with(fx, &cfg, runner, cache, src_hash, audit_rate.is_some())
    }

    fn eval_with(
        fx: &ScopeFixture,
        cfg: &Config,
        runner: &dyn Runner,
        cache: &Mutex<Cache>,
        src_hash: &str,
        audit: bool,
    ) -> (MutantOutcome, CacheStats) {
        let cfg = cfg.clone();
        let m = Arc::new(fx.mutant(1));
        let mut hashes = HashMap::new();
        hashes.insert(m.file.clone(), src_hash.to_string());
        let tree = TestTree::walk(&cfg.tests_path());
        let prefix = compute_scope_prefix(&cfg);
        let stats = CacheStats::default();
        let killer_hits = Mutex::new(Vec::new());
        let ctx = EvalCtx {
            cfg: &cfg,
            filters: &[],
            runner,
            cache,
            file_hashes: &hashes,
            scope_maps: &HashMap::new(),
            cov_identity: None,
            anchors: &HashMap::new(),
            scope_prefix: &prefix,
            test_tree: &tree,
            equiv: None,
            file_sources: &HashMap::new(),
            deadline: None,
            stats: &stats,
            killer_hits: &killer_hits,
        };
        let mut outcomes = vec![evaluate(&ctx, &m)];
        if audit {
            audit_killer_hits(&cfg, &ctx, &[m], &mut outcomes).unwrap();
        }
        (outcomes.pop().unwrap(), stats)
    }

    /// Seed the cache with a kill by `TEST_A`, then apply `edit` and return
    /// the second evaluation's runner (for its run count) and stats.
    fn killed_then(edit: impl FnOnce(&ScopeFixture)) -> (ScriptRunner, CacheStats) {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        let first = ScriptRunner::killing(Some(TEST_A));
        eval_cached(&fx, &first, &cache, "src-1", None);
        assert_eq!(first.runs(), 1);
        edit(&fx);
        let second = ScriptRunner::killing(Some(TEST_A));
        let (_, stats) = eval_cached(&fx, &second, &cache, "src-1", None);
        (second, stats)
    }

    const TEST_C: &str = "tests/c/test_c.py::test_c";

    /// What a fake runner answers.
    #[derive(Clone, Copy)]
    enum Verdict {
        Survive,
        KillBy(&'static str),
        TimeOut,
    }

    impl Verdict {
        fn outcome(self, m: &Arc<Mutant>) -> MutantOutcome {
            match self {
                Verdict::Survive => MutantOutcome::survived(m.clone()),
                Verdict::KillBy(t) => MutantOutcome::killed_by(m.clone(), Some(t.to_string())),
                Verdict::TimeOut => MutantOutcome::timed_out(m.clone()),
            }
        }
    }

    /// Runner for the survivor-delta tests: a full run answers `full`, a run
    /// on selected tests answers `delta` (or can't select when `None`), and
    /// every selection asked for is recorded.
    struct DeltaRunner {
        full: Verdict,
        delta: Option<Verdict>,
        full_runs: AtomicUsize,
        asked: Mutex<Vec<Vec<String>>>,
    }

    impl DeltaRunner {
        fn new(full: Verdict, delta: Option<Verdict>) -> Self {
            Self {
                full,
                delta,
                full_runs: AtomicUsize::new(0),
                asked: Mutex::new(Vec::new()),
            }
        }
        fn full_runs(&self) -> usize {
            self.full_runs.load(Ordering::Relaxed)
        }
        fn asked(&self) -> Vec<Vec<String>> {
            lock_recover(&self.asked).clone()
        }
    }

    impl Runner for DeltaRunner {
        fn run(&self, m: &Arc<Mutant>) -> Result<MutantOutcome> {
            self.full_runs.fetch_add(1, Ordering::Relaxed);
            Ok(self.full.outcome(m))
        }
        fn run_tests(&self, m: &Arc<Mutant>, tests: &[String]) -> Result<Option<MutantOutcome>> {
            lock_recover(&self.asked).push(tests.to_vec());
            Ok(self.delta.map(|v| v.outcome(m)))
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Passed {
                wall: Duration::ZERO,
            })
        }
    }

    /// Cache a survivor of the line-1 mutant under `fx` as it is, apply
    /// `edit`, then evaluate again with `second`.
    fn survived_then(
        fx: &ScopeFixture,
        edit: impl FnOnce(&ScopeFixture),
        second: &DeltaRunner,
    ) -> (MutantOutcome, CacheStats) {
        let cache = Mutex::new(Cache::default());
        let first = DeltaRunner::new(Verdict::Survive, None);
        let (out, _) = eval_cached(fx, &first, &cache, "src-1", None);
        assert!(matches!(out, MutantOutcome::Survived { .. }), "{out:?}");
        edit(fx);
        eval_cached(fx, second, &cache, "src-1", None)
    }

    fn add_test_c(fx: &ScopeFixture) {
        fx.write("tests/c/test_c.py", "def test_c(): assert False\n");
        fx.write_coverage(&[(1, &[TEST_A, TEST_B, TEST_C])]);
    }

    #[test]
    fn survivor_runs_only_a_new_covering_test() {
        let fx = two_covering();
        let second = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        let (out, stats) = survived_then(&fx, add_test_c, &second);
        assert!(matches!(out, MutantOutcome::Survived { .. }), "{out:?}");
        assert_eq!(second.full_runs(), 0);
        assert_eq!(second.asked(), vec![vec![TEST_C.to_string()]]);
        assert_eq!(stats.delta_hits.load(Ordering::Relaxed), 1);
        assert_eq!(stats.delta_tests.load(Ordering::Relaxed), 1);
        assert_eq!(stats.delta_full_tests.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn delta_survivor_is_stored_under_the_new_scope() {
        // After the delta, nothing changed: a plain full hit.
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &DeltaRunner::new(Verdict::Survive, None),
            &cache,
            "src-1",
            None,
        );
        add_test_c(&fx);
        let delta = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        eval_cached(&fx, &delta, &cache, "src-1", None);
        let third = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        let (_, stats) = eval_cached(&fx, &third, &cache, "src-1", None);
        assert_eq!(stats.full_hits.load(Ordering::Relaxed), 1);
        assert!(third.asked().is_empty());
        assert_eq!(third.full_runs(), 0);
    }

    #[test]
    fn survivor_runs_only_the_edited_file_tests() {
        let fx = two_covering();
        let second = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        survived_then(
            &fx,
            |fx| fx.write("tests/b/test_b.py", "def test_b(): assert 2\n"),
            &second,
        );
        assert_eq!(second.asked(), vec![vec![TEST_B.to_string()]]);
        assert_eq!(second.full_runs(), 0);
    }

    #[test]
    fn survivor_ignores_a_new_uncovering_test_in_a_covering_file() {
        let fx = two_covering();
        let second = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        let (out, _) = survived_then(
            &fx,
            |fx| {
                fx.write(
                    "tests/b/test_b.py",
                    "def test_b(): pass\n\ndef test_b2(): pass\n",
                )
            },
            &second,
        );
        assert!(matches!(out, MutantOutcome::Survived { .. }), "{out:?}");
        assert!(second.asked().is_empty(), "test_b is unchanged");
        assert_eq!(second.full_runs(), 0);
    }

    #[test]
    fn support_file_edit_puts_every_covering_test_in_the_delta() {
        let fx = two_covering();
        let second = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        survived_then(
            &fx,
            |fx| fx.write("tests/helpers.py", "changed = 1\n"),
            &second,
        );
        assert_eq!(
            second.asked(),
            vec![vec![TEST_A.to_string(), TEST_B.to_string()]]
        );
    }

    #[test]
    fn survivor_with_a_test_removed_is_reused_without_running() {
        let fx = two_covering();
        let second = DeltaRunner::new(Verdict::KillBy(TEST_A), Some(Verdict::KillBy(TEST_A)));
        let (out, stats) = survived_then(&fx, |fx| fx.write_coverage(&[(1, &[TEST_A])]), &second);
        assert!(matches!(out, MutantOutcome::Survived { .. }), "{out:?}");
        assert_eq!(second.full_runs(), 0);
        assert!(second.asked().is_empty());
        assert_eq!(stats.delta_hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn delta_kill_is_stored_as_a_kill() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &DeltaRunner::new(Verdict::Survive, None),
            &cache,
            "src-1",
            None,
        );
        add_test_c(&fx);
        let delta = DeltaRunner::new(Verdict::Survive, Some(Verdict::KillBy(TEST_C)));
        let (out, _) = eval_cached(&fx, &delta, &cache, "src-1", None);
        assert_eq!(out.killer(), Some(TEST_C));
        let third = DeltaRunner::new(Verdict::Survive, None);
        let (out, stats) = eval_cached(&fx, &third, &cache, "src-1", None);
        assert_eq!(out.killer(), Some(TEST_C));
        assert_eq!(stats.full_hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn timed_out_verdict_takes_no_delta() {
        // Editing the test that hung can turn a timeout into a survivor.
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &DeltaRunner::new(Verdict::TimeOut, None),
            &cache,
            "src-1",
            None,
        );
        fx.write("tests/b/test_b.py", "def test_b(): assert 2\n");
        let second = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        eval_cached(&fx, &second, &cache, "src-1", None);
        assert_eq!(second.full_runs(), 1);
        assert!(second.asked().is_empty());
    }

    #[test]
    fn runner_that_cannot_select_runs_fully() {
        let fx = two_covering();
        let second = DeltaRunner::new(Verdict::Survive, None);
        survived_then(&fx, add_test_c, &second);
        assert_eq!(second.full_runs(), 1);
    }

    #[test]
    fn source_edit_or_run_shape_change_takes_no_delta() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &DeltaRunner::new(Verdict::Survive, None),
            &cache,
            "src-1",
            None,
        );
        add_test_c(&fx);
        let edited = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        eval_cached(&fx, &edited, &cache, "src-2", None);
        assert_eq!(edited.full_runs(), 1, "source changed");

        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &DeltaRunner::new(Verdict::Survive, None),
            &cache,
            "src-1",
            None,
        );
        add_test_c(&fx);
        let cfg = Config {
            timeout_secs: 99,
            ..fx.config(RunnerKind::Pytest, true)
        };
        let reshaped = DeltaRunner::new(Verdict::Survive, Some(Verdict::Survive));
        eval_with(&fx, &cfg, &reshaped, &cache, "src-1", false);
        assert_eq!(reshaped.full_runs(), 1, "timeout changed");
    }

    #[test]
    fn audited_delta_survivor_uses_the_full_run() {
        // The new test only fails after an old one (order dependence): the
        // delta says survived, the full covering set kills it.
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &DeltaRunner::new(Verdict::Survive, None),
            &cache,
            "src-1",
            None,
        );
        add_test_c(&fx);
        let second = DeltaRunner::new(Verdict::KillBy(TEST_A), Some(Verdict::Survive));
        let cfg = Config {
            cache_audit_rate: 0.05,
            ..fx.config(RunnerKind::Pytest, true)
        };
        // The first delta of a run is always audited.
        let (out, stats) = eval_with(&fx, &cfg, &second, &cache, "src-1", false);
        assert_eq!(out.killer(), Some(TEST_A));
        assert_eq!(second.full_runs(), 1);
        assert_eq!(stats.delta_audited.load(Ordering::Relaxed), 1);
        assert_eq!(stats.delta_disagreed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn is_cached_agrees_with_evaluate() {
        let fx = two_covering();
        let cfg = fx.config(RunnerKind::Pytest, true);
        let cache = Mutex::new(Cache::default());
        let cached = |fx: &ScopeFixture| {
            let mut hashes = HashMap::new();
            hashes.insert(fx.mutant(1).file, "src-1".to_string());
            is_cached(
                &cfg,
                &lock_recover(&cache),
                &FileArtifacts {
                    hashes,
                    ..FileArtifacts::default()
                },
                &compute_scope_prefix(&cfg),
                &TestTree::walk(&cfg.tests_path()),
                &fx.mutant(1),
            )
        };
        assert!(!cached(&fx), "empty cache");
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(TEST_A)),
            &cache,
            "src-1",
            None,
        );
        assert!(cached(&fx), "full hit");
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        assert!(cached(&fx), "killer hit");
        fx.write("tests/a/test_a.py", "def test_a(): assert 2\n");
        assert!(!cached(&fx), "killer edited");
    }

    #[test]
    fn killer_hit_survives_edit_to_other_covering_file() {
        let (runner, stats) = killed_then(|fx| {
            fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        });
        assert_eq!(
            runner.runs(),
            0,
            "kill by test_a must survive a test_b edit"
        );
        assert_eq!(stats.killer_hits.load(Ordering::Relaxed), 1);
        assert_eq!(stats.full_hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn killer_hit_survives_new_test_in_killer_file() {
        let (runner, stats) = killed_then(|fx| {
            fx.write(
                "tests/a/test_a.py",
                "def test_a(): pass\n\ndef test_a2(): assert 1\n",
            );
        });
        assert_eq!(runner.runs(), 0, "test_a is unchanged");
        assert_eq!(stats.killer_hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn killer_file_import_edit_invalidates() {
        let (runner, _) = killed_then(|fx| {
            fx.write("tests/a/test_a.py", "import os\n\ndef test_a(): pass\n");
        });
        assert_eq!(runner.runs(), 1, "shared statements count");
    }

    #[test]
    fn unchanged_tree_is_a_full_hit_not_a_killer_hit() {
        let (runner, stats) = killed_then(|_| {});
        assert_eq!(runner.runs(), 0);
        assert_eq!(stats.full_hits.load(Ordering::Relaxed), 1);
        assert_eq!(stats.killer_hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn killer_edit_invalidates() {
        let (runner, stats) = killed_then(|fx| {
            fx.write("tests/a/test_a.py", "def test_a(): assert 1\n");
        });
        assert_eq!(runner.runs(), 1);
        assert_eq!(stats.misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn killer_conftest_chain_edit_invalidates() {
        for rel in [
            "tests/a/conftest.py",
            "tests/conftest.py",
            "tests/helpers.py",
        ] {
            let (runner, _) = killed_then(|fx| fx.write(rel, "changed = 1\n"));
            assert_eq!(runner.runs(), 1, "{rel} must invalidate the killer hit");
        }
    }

    #[test]
    fn killer_removed_from_covering_set_misses() {
        let (runner, _) = killed_then(|fx| fx.write_coverage(&[(1, &[TEST_B])]));
        assert_eq!(runner.runs(), 1, "test_a no longer covers the mutant");
    }

    #[test]
    fn budget_scales_the_baseline_and_never_exceeds_timeout() {
        let s = Duration::from_secs_f64;
        assert_eq!(
            budget_timeout_secs(30, 3.0, s(1.43)),
            10,
            "3 × 1.43 + 5 = 9.29 → 10"
        );
        assert_eq!(budget_timeout_secs(30, 1.0, s(0.0)), 5, "floor");
        assert_eq!(
            budget_timeout_secs(30, 3.0, s(9.0)),
            30,
            "capped at --timeout"
        );
        assert_eq!(budget_timeout_secs(60, 3.0, s(9.0)), 32);
    }

    #[test]
    fn budgeted_timeout_needs_a_factor_and_a_baseline() {
        let mut cfg = base_test_config();
        cfg.timeout_secs = 60;
        let wall = Some(Duration::from_secs(9));
        assert_eq!(budgeted_timeout_secs(&cfg, wall), None, "no factor");
        cfg.timeout_factor = Some(3.0);
        assert_eq!(budgeted_timeout_secs(&cfg, None), None, "no baseline");
        assert_eq!(budgeted_timeout_secs(&cfg, wall), Some(32));
    }

    /// Runner whose baseline is green and reports a 5 s test process.
    struct SlowGreenRunner;
    impl Runner for SlowGreenRunner {
        fn run(&self, _m: &Arc<Mutant>) -> Result<MutantOutcome> {
            unreachable!()
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Passed {
                wall: Duration::from_secs(5),
            })
        }
    }

    /// Runner whose baseline is red.
    struct RedBaselineRunner;
    impl Runner for RedBaselineRunner {
        fn run(&self, _m: &Arc<Mutant>) -> Result<MutantOutcome> {
            unreachable!()
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            Ok(runner::BaselineStatus::Failed {
                output: "1 failed".into(),
            })
        }
    }

    #[test]
    fn preflight_baseline_times_a_green_suite_and_refuses_a_red_one() {
        let mut cfg = base_test_config();
        let ms = vec![Arc::new(arith_mutant("return x + 0", "+", "-"))];
        cfg.verify_baseline = true;
        let took = preflight_baseline(&cfg, &SlowGreenRunner, &ms).unwrap();
        assert_eq!(
            took,
            Some(Duration::from_secs(5)),
            "the runner's wall, not ours"
        );
        let err = preflight_baseline(&cfg, &RedBaselineRunner, &ms).unwrap_err();
        assert!(err.to_string().contains("1 failed"), "{err}");
        assert_eq!(
            preflight_baseline(&cfg, &RedBaselineRunner, &[]).unwrap(),
            None
        );
        cfg.verify_baseline = false;
        assert_eq!(
            preflight_baseline(&cfg, &RedBaselineRunner, &ms).unwrap(),
            None
        );
    }

    #[test]
    fn narrow_to_baseline_needs_a_deselection_and_no_select_tests() {
        let fx = two_covering();
        let cfg = fx.config(RunnerKind::Pytest, true);
        let set = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect();
        let d = runner::Deselection {
            deselected: set(&[TEST_A]),
            ran: set(&[TEST_B]),
        };
        assert!(narrow_to_baseline(&cfg, &runner::Deselection::default()).is_none());
        let n = narrow_to_baseline(&cfg, &d).expect("narrowed");
        let line1 = |c: &Config| {
            c.coverage
                .as_ref()
                .unwrap()
                .tests_for_mutant(&fx.mutant(1))
                .map(<[String]>::to_vec)
        };
        assert_eq!(line1(&n), Some(vec![TEST_B.to_string()]));
        assert!(
            n.coverage_unselected.is_some(),
            "full contexts kept for the filter"
        );
        assert!(
            narrow_to_baseline(&n, &d).is_none(),
            "--select-tests already narrowed"
        );
    }

    #[test]
    fn killer_outside_select_tests_is_no_killer_hit() {
        // A full run caches a kill by test_a. A `--select-tests` run that
        // selected only test_b must not report test_a's kill: the narrowed
        // contexts don't list test_a, so `compute_killer_scope` refuses it.
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(TEST_A)),
            &cache,
            "src-1",
            None,
        );
        let full = fx.config(RunnerKind::Pytest, true);
        let narrowed = full
            .coverage
            .as_ref()
            .unwrap()
            .select(&std::iter::once(TEST_B.to_string()).collect());
        let cfg = Config {
            coverage_unselected: full.coverage.clone(),
            coverage: Some(narrowed),
            ..full
        };
        let runner = ScriptRunner::surviving();
        let (outcome, stats) = eval_with(&fx, &cfg, &runner, &cache, "src-1", false);
        assert_eq!(runner.runs(), 1, "must re-run against the selection");
        assert_eq!(stats.killer_hits.load(Ordering::Relaxed), 0);
        assert!(
            matches!(outcome, MutantOutcome::Survived { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn source_change_invalidates_killer_hit() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(TEST_A)),
            &cache,
            "src-1",
            None,
        );
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        let second = ScriptRunner::killing(Some(TEST_A));
        eval_cached(&fx, &second, &cache, "src-2", None);
        assert_eq!(second.runs(), 1);
    }

    #[test]
    fn survived_never_uses_killer_scope() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(&fx, &ScriptRunner::surviving(), &cache, "src-1", None);
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        let second = ScriptRunner::surviving();
        eval_cached(&fx, &second, &cache, "src-1", None);
        assert_eq!(
            second.runs(),
            1,
            "a covering-test edit must re-test a survivor"
        );
    }

    #[test]
    fn no_killer_falls_back_to_scope_key() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(&fx, &ScriptRunner::killing(None), &cache, "src-1", None);
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        let second = ScriptRunner::killing(None);
        eval_cached(&fx, &second, &cache, "src-1", None);
        assert_eq!(second.runs(), 1);
    }

    #[test]
    fn killer_hit_resolves_parametrized_and_class_node_ids() {
        let killer = "tests/a/test_a.py::TestA::test_x[1-2]";
        let fx = ScopeFixture::new();
        fx.write_coverage(&[(1, &[killer, TEST_B])]);
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(killer)),
            &cache,
            "src-1",
            None,
        );
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        let second = ScriptRunner::killing(Some(killer));
        let (out, _) = eval_cached(&fx, &second, &cache, "src-1", None);
        assert_eq!(second.runs(), 0);
        assert_eq!(
            out.killer(),
            Some(killer),
            "killer round-trips through the cache"
        );
    }

    #[test]
    fn audit_confirmation_keeps_entry() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(TEST_A)),
            &cache,
            "src-1",
            None,
        );
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        let second = ScriptRunner::killing(Some(TEST_A));
        let (out, stats) = eval_cached(&fx, &second, &cache, "src-1", Some(0.01));
        // Rate 1% of one hit still audits it (minimum one).
        assert_eq!(second.audits.load(Ordering::Relaxed), 1);
        assert_eq!(second.runs(), 0);
        assert_eq!(stats.audited.load(Ordering::Relaxed), 1);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 0);
        assert!(matches!(out, MutantOutcome::Killed { .. }));
    }

    #[test]
    fn audit_disagreement_drops_entry() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(TEST_A)),
            &cache,
            "src-1",
            None,
        );
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        // Audit says the kill doesn't reproduce; the full re-run survives.
        let second = ScriptRunner {
            audit_kills: false,
            ..ScriptRunner::surviving()
        };
        let (out, stats) = eval_cached(&fx, &second, &cache, "src-1", Some(1.0));
        assert_eq!(
            second.runs(),
            1,
            "disagreement must re-run the mutant fully"
        );
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
        assert!(matches!(out, MutantOutcome::Survived { .. }), "{out:?}");
        // The re-run's verdict replaced the stale kill in the cache.
        let third = ScriptRunner::surviving();
        let (out, _) = eval_cached(&fx, &third, &cache, "src-1", None);
        assert_eq!(third.runs(), 0);
        assert!(matches!(out, MutantOutcome::Survived { .. }));
    }

    #[test]
    fn zero_audit_rate_never_audits() {
        let fx = two_covering();
        let cache = Mutex::new(Cache::default());
        eval_cached(
            &fx,
            &ScriptRunner::killing(Some(TEST_A)),
            &cache,
            "src-1",
            None,
        );
        fx.write("tests/b/test_b.py", "def test_b(): assert 1\n");
        let second = ScriptRunner::killing(Some(TEST_A));
        eval_cached(&fx, &second, &cache, "src-1", Some(0.0));
        assert_eq!(second.audits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn sample_for_audit_sizes() {
        let hits = |n: usize| -> Vec<KillerHit> {
            (0..n)
                .map(|i| {
                    let mut m = arith_mutant("x + 0", "+", "-");
                    m.id = format!("m{i}");
                    KillerHit {
                        mutant: Arc::new(m),
                        killer: TEST_A.into(),
                    }
                })
                .collect()
        };
        assert_eq!(sample_for_audit(hits(0), 0.05).len(), 0);
        assert_eq!(sample_for_audit(hits(10), 0.0).len(), 0);
        assert_eq!(sample_for_audit(hits(10), 0.05).len(), 1, "minimum one");
        assert_eq!(sample_for_audit(hits(100), 0.05).len(), 5);
        assert_eq!(sample_for_audit(hits(101), 0.05).len(), 6, "rounds up");
        assert_eq!(sample_for_audit(hits(7), 1.0).len(), 7);
    }

    // ---- audits, persist, file analysis -----------------------------------

    /// Runner whose `audit` verdict is chosen by mutant id (`none`, `err`,
    /// `kill`, anything else survives); a full `run` always survives.
    struct AuditByIdRunner {
        runs: AtomicUsize,
        records: Vec<crate::kill_order::KillRecord>,
    }

    impl AuditByIdRunner {
        fn new() -> Self {
            Self {
                runs: AtomicUsize::new(0),
                records: Vec::new(),
            }
        }
    }

    impl Runner for AuditByIdRunner {
        fn run(&self, m: &Arc<Mutant>) -> Result<MutantOutcome> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(MutantOutcome::survived(m.clone()))
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            unreachable!()
        }
        fn audit(&self, m: &Arc<Mutant>, _killer: &str) -> Result<Option<MutantOutcome>> {
            match m.id.as_str() {
                "none" => Ok(None),
                "err" => anyhow::bail!("audit blew up"),
                "kill" => Ok(Some(MutantOutcome::killed(m.clone()))),
                _ => Ok(Some(MutantOutcome::survived(m.clone()))),
            }
        }
        fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
            self.records.clone()
        }
    }

    /// One mutant per audit verdict, each "killed" by `TEST_A`, plus the
    /// matching killer hits.
    fn audit_fixture() -> (Vec<Arc<Mutant>>, Vec<MutantOutcome>, Vec<KillerHit>) {
        let mutants: Vec<Arc<Mutant>> = ["none", "err", "kill", "survive"]
            .iter()
            .map(|id| {
                let mut m = arith_mutant("x + 0", "+", "-");
                m.id = (*id).into();
                Arc::new(m)
            })
            .collect();
        let outcomes = mutants
            .iter()
            .map(|m| MutantOutcome::killed_by(m.clone(), Some(TEST_A.into())))
            .collect();
        let hits = mutants
            .iter()
            .map(|m| KillerHit {
                mutant: m.clone(),
                killer: TEST_A.into(),
            })
            .collect();
        (mutants, outcomes, hits)
    }

    /// Build an `EvalCtx` over empty cache/hash state and hand it to `f`.
    fn with_ctx<R>(
        cfg: &Config,
        runner: &dyn Runner,
        deadline: Option<Instant>,
        hits: Vec<KillerHit>,
        f: impl FnOnce(&EvalCtx) -> R,
    ) -> R {
        let cache = Mutex::new(Cache::default());
        let killer_hits = Mutex::new(hits);
        let stats = CacheStats::default();
        let empty_hashes = HashMap::new();
        let empty_maps = HashMap::new();
        let empty_sources = HashMap::new();
        let tree = TestTree::default();
        let ctx = EvalCtx {
            cfg,
            filters: &[],
            runner,
            cache: &cache,
            file_hashes: &empty_hashes,
            scope_maps: &empty_maps,
            cov_identity: None,
            anchors: &HashMap::new(),
            scope_prefix: "",
            test_tree: &tree,
            equiv: None,
            file_sources: &empty_sources,
            deadline,
            stats: &stats,
            killer_hits: &killer_hits,
        };
        f(&ctx)
    }

    fn is_killed(o: &MutantOutcome) -> bool {
        matches!(o, MutantOutcome::Killed { .. })
    }

    #[test]
    fn audit_inferred_reruns_kills_that_do_not_reproduce() {
        let cfg = Config {
            inferred_audit_rate: 1.0,
            ..base_test_config()
        };
        let runner = AuditByIdRunner::new();
        let (mutants, mut outcomes, hits) = audit_fixture();
        let failed = with_ctx(&cfg, &runner, None, Vec::new(), |ctx| {
            audit_inferred(&cfg, ctx, &mutants, &mut outcomes, hits).unwrap()
        });
        // An erroring audit and a surviving one both fail; a runner that
        // can't audit (`None`) and a reproduced kill don't.
        assert_eq!(failed, 2);
        assert_eq!(runner.runs.load(Ordering::Relaxed), 2, "failures re-run");
        let killed: Vec<bool> = outcomes.iter().map(is_killed).collect();
        assert_eq!(killed, [true, false, true, false]);
    }

    #[test]
    fn audit_inferred_is_skipped_past_the_deadline() {
        let cfg = Config {
            inferred_audit_rate: 1.0,
            ..base_test_config()
        };
        let runner = AuditByIdRunner::new();
        let (mutants, mut outcomes, hits) = audit_fixture();
        let past = Instant::now() - Duration::from_secs(1);
        let failed = with_ctx(&cfg, &runner, Some(past), Vec::new(), |ctx| {
            audit_inferred(&cfg, ctx, &mutants, &mut outcomes, hits).unwrap()
        });
        assert_eq!(failed, 0);
        assert_eq!(runner.runs.load(Ordering::Relaxed), 0);
        assert!(outcomes.iter().all(is_killed));
    }

    #[test]
    fn audit_killer_hits_counts_errors_as_disagreements_and_skips_unauditable() {
        let cfg = Config {
            cache_audit_rate: 1.0,
            ..base_test_config()
        };
        let runner = AuditByIdRunner::new();
        let (mutants, mut outcomes, hits) = audit_fixture();
        let (audited, dropped) = with_ctx(&cfg, &runner, None, hits, |ctx| {
            audit_killer_hits(&cfg, ctx, &mutants, &mut outcomes).unwrap();
            (
                ctx.stats.audited.load(Ordering::Relaxed),
                ctx.stats.dropped.load(Ordering::Relaxed),
            )
        });
        assert_eq!(audited, 3, "`None` is not an audit");
        assert_eq!(dropped, 2, "error + survivor");
        let killed: Vec<bool> = outcomes.iter().map(is_killed).collect();
        assert_eq!(killed, [true, false, true, false]);
    }

    #[test]
    fn audit_killer_hits_is_skipped_past_the_deadline() {
        let cfg = Config {
            cache_audit_rate: 1.0,
            ..base_test_config()
        };
        let runner = AuditByIdRunner::new();
        let (mutants, mut outcomes, hits) = audit_fixture();
        let past = Instant::now() - Duration::from_secs(1);
        let audited = with_ctx(&cfg, &runner, Some(past), hits, |ctx| {
            audit_killer_hits(&cfg, ctx, &mutants, &mut outcomes).unwrap();
            ctx.stats.audited.load(Ordering::Relaxed)
        });
        assert_eq!(audited, 0);
        assert_eq!(runner.runs.load(Ordering::Relaxed), 0);
        assert!(outcomes.iter().all(is_killed));
    }

    #[test]
    fn erroring_filter_admits_the_mutant() {
        struct Broken;
        impl Filter for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn admits(&self, _m: &Mutant) -> Result<bool> {
                anyhow::bail!("filter crashed")
            }
        }
        let cfg = base_test_config();
        let filters: Vec<Box<dyn Filter>> = vec![Box::new(Broken)];
        let m = Arc::new(arith_mutant("x + 0", "+", "-"));
        with_ctx(&cfg, &KillRunner, None, Vec::new(), |ctx| {
            let ctx = EvalCtx {
                filters: &filters,
                ..*ctx
            };
            assert!(precheck(&ctx, &m).is_none(), "an erroring filter admits");
            assert!(is_killed(&evaluate(&ctx, &m)));
        });
    }

    /// Bookkeeping IO failures (cache, kill-order, kill-sets, history,
    /// dominator store) warn but never sink the run: `persist` still returns
    /// the report and the history entry.
    #[test]
    fn persist_survives_every_bookkeeping_write_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let bad = |name: &str| blocker.join(name);
        let garbage = tmp.path().join("dominators.json");
        std::fs::write(&garbage, "{not json").unwrap();
        let cfg = Config {
            source_root: tmp.path().to_path_buf(),
            cache: true,
            cache_path: bad("cache.json"),
            smart_order: true,
            kill_order_path: bad("kill-order.json"),
            record_kill_sets: Some(bad("kill-sets.jsonl")),
            history: true,
            history_path: bad("history.jsonl"),
            dominators_path: garbage,
            max_time_secs: Some(1),
            ..base_test_config()
        };
        let runner = AuditByIdRunner {
            records: vec![crate::kill_order::KillRecord {
                file: "t.py".into(),
                operator: "arith".into(),
                nodeid: TEST_A.into(),
            }],
            ..AuditByIdRunner::new()
        };
        let m = Arc::new(arith_mutant("x + 0", "+", "-"));
        let outcomes = vec![
            MutantOutcome::killed(m.clone()),
            MutantOutcome::skipped(m, TIME_BUDGET_FILTER),
        ];
        let (report, entry) = persist(
            &cfg,
            Instant::now(),
            &runner,
            &Mutex::new(Cache::default()),
            &CacheStats::default(),
            Some(Instant::now()),
            outcomes,
        );
        assert_eq!(report.outcomes.len(), 2);
        assert_eq!(report.dominator_score, None, "broken store is ignored");
        assert!(entry.is_some(), "entry flows back despite the append error");
        assert!(!bad("history.jsonl").exists());
    }

    #[test]
    fn analyze_unique_files_degrades_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let at = |name: &str, body: &[u8]| {
            let p = tmp.path().join(name);
            std::fs::write(&p, body).unwrap();
            let mut m = arith_mutant("x + 0", "+", "-");
            m.file = p;
            Arc::new(m)
        };
        let good = at("good.py", b"x + 0\n");
        let broken = at("broken.py", b"def (:\n");
        let binary = at("binary.py", b"\xff\xfe x + 0\n");
        let mut missing = arith_mutant("x + 0", "+", "-");
        missing.file = tmp.path().join("missing.py");
        let missing = Arc::new(missing);
        let all = [
            good.clone(),
            broken.clone(),
            binary.clone(),
            missing.clone(),
        ];
        let a = analyze_unique_files(&all, true, true);

        assert!(!a.hashes.contains_key(&missing.file), "unreadable: no hash");
        for m in [&good, &broken, &binary] {
            assert!(a.hashes.contains_key(&m.file), "{}", m.file.display());
        }
        assert!(a.scope_maps.contains_key(&good.file));
        assert!(!a.scope_maps.contains_key(&broken.file), "unparseable");
        assert!(a.sources.contains_key(&broken.file));
        assert!(!a.sources.contains_key(&binary.file), "not utf-8");

        // Scope mode keys the cache on the enclosing scope when a map exists,
        // and falls back to the file hash when it doesn't.
        let cfg = Config {
            cache_scope: CacheScope::Scope,
            ..base_test_config()
        };
        let scoped = mutant_identity_hash(&cfg, &good, &a.hashes, &a.scope_maps, None);
        assert_eq!(
            scoped,
            Some(a.scope_maps[&good.file].scope_hash_for(good.range))
        );
        assert_eq!(
            mutant_identity_hash(&cfg, &broken, &a.hashes, &a.scope_maps, None),
            a.hashes.get(&broken.file).cloned()
        );
    }

    #[test]
    fn hypothesis_seed_is_part_of_the_scope_prefix() {
        let seeded = |s| Config {
            hypothesis_seed: s,
            ..base_test_config()
        };
        let none = compute_scope_prefix(&seeded(None));
        let one = compute_scope_prefix(&seeded(Some(1)));
        assert_ne!(none, one);
        assert_ne!(one, compute_scope_prefix(&seeded(Some(2))));
        assert_eq!(one, compute_scope_prefix(&seeded(Some(1))));
    }

    #[test]
    fn red_baseline_mentions_private_home_only_when_on() {
        let ms = vec![Arc::new(arith_mutant("return x + 0", "+", "-"))];
        let msg = |private_home| {
            let cfg = Config {
                verify_baseline: true,
                private_home,
                ..base_test_config()
            };
            preflight_baseline(&cfg, &RedBaselineRunner, &ms)
                .unwrap_err()
                .to_string()
        };
        assert!(msg(true).contains("--no-private-home"));
        assert!(!msg(false).contains("--no-private-home"));
    }

    // ---- audit / persist error paths ---------------------------------------

    /// Runner whose `run` survives and whose `audit` always errors.
    struct ErrAuditRunner {
        runs: AtomicUsize,
    }

    impl Runner for ErrAuditRunner {
        fn run(&self, m: &Arc<Mutant>) -> Result<MutantOutcome> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(MutantOutcome::survived(m.clone()))
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            unreachable!()
        }
        fn audit(&self, _m: &Arc<Mutant>, _killer: &str) -> Result<Option<MutantOutcome>> {
            anyhow::bail!("audit exploded")
        }
    }

    /// Run `f` against an `EvalCtx` with no cache hashes, no scope maps and the
    /// given filters/deadline/killer hits.
    fn with_bare_ctx<R>(
        cfg: &Config,
        runner: &dyn Runner,
        filters: &[Box<dyn Filter>],
        deadline: Option<Instant>,
        hits: Vec<KillerHit>,
        f: impl FnOnce(&EvalCtx) -> R,
    ) -> R {
        let cache = Mutex::new(Cache::default());
        let stats = CacheStats::default();
        let killer_hits = Mutex::new(hits);
        let tree = TestTree::default();
        let empty_hashes = HashMap::new();
        let empty_maps = HashMap::new();
        let empty_sources = HashMap::new();
        let ctx = EvalCtx {
            cfg,
            filters,
            runner,
            cache: &cache,
            file_hashes: &empty_hashes,
            scope_maps: &empty_maps,
            cov_identity: None,
            anchors: &HashMap::new(),
            scope_prefix: "",
            test_tree: &tree,
            equiv: None,
            file_sources: &empty_sources,
            deadline,
            stats: &stats,
            killer_hits: &killer_hits,
        };
        f(&ctx)
    }

    fn one_hit() -> (Vec<Arc<Mutant>>, Vec<KillerHit>, Vec<MutantOutcome>) {
        let m = Arc::new(arith_mutant("return x + 0", "+", "-"));
        let hits = vec![KillerHit {
            mutant: m.clone(),
            killer: TEST_A.into(),
        }];
        let outcomes = vec![MutantOutcome::killed_by(m.clone(), Some(TEST_A.into()))];
        (vec![m], hits, outcomes)
    }

    #[test]
    fn audit_inferred_reruns_on_disagreement_and_error_but_not_on_none() {
        let cfg = Config {
            inferred_audit_rate: 1.0,
            ..base_test_config()
        };
        // Audit disagrees: the mutant is re-run and its real verdict wins.
        let (ms, hits, mut outs) = one_hit();
        let r = ScriptRunner {
            audit_kills: false,
            ..ScriptRunner::surviving()
        };
        let n = with_bare_ctx(&cfg, &r, &[], None, vec![], |ctx| {
            audit_inferred(&cfg, ctx, &ms, &mut outs, hits).unwrap()
        });
        assert_eq!(n, 1);
        assert_eq!(r.runs(), 1);
        assert!(
            matches!(outs[0], MutantOutcome::Survived { .. }),
            "{:?}",
            outs[0]
        );

        // Audit errors: counted as a failure and re-run too.
        let (ms, hits, mut outs) = one_hit();
        let r = ErrAuditRunner {
            runs: AtomicUsize::new(0),
        };
        let n = with_bare_ctx(&cfg, &r, &[], None, vec![], |ctx| {
            audit_inferred(&cfg, ctx, &ms, &mut outs, hits).unwrap()
        });
        assert_eq!(n, 1);
        assert_eq!(r.runs.load(Ordering::Relaxed), 1);
        assert!(matches!(outs[0], MutantOutcome::Survived { .. }));

        // Runner can't audit (default `Ok(None)`): nothing to compare.
        let (ms, hits, mut outs) = one_hit();
        let n = with_bare_ctx(&cfg, &KillRunner, &[], None, vec![], |ctx| {
            audit_inferred(&cfg, ctx, &ms, &mut outs, hits).unwrap()
        });
        assert_eq!(n, 0);
        assert!(matches!(outs[0], MutantOutcome::Killed { .. }));
    }

    #[test]
    fn audit_inferred_skips_past_the_deadline() {
        let cfg = Config {
            inferred_audit_rate: 1.0,
            ..base_test_config()
        };
        let (ms, hits, mut outs) = one_hit();
        let past = Instant::now() - Duration::from_secs(1);
        let n = with_bare_ctx(&cfg, &PanicRunner, &[], Some(past), vec![], |ctx| {
            audit_inferred(&cfg, ctx, &ms, &mut outs, hits).unwrap()
        });
        assert_eq!(n, 0);
        assert!(matches!(outs[0], MutantOutcome::Killed { .. }));
    }

    #[test]
    fn cache_audit_handles_error_none_and_deadline() {
        let cfg = Config {
            cache_audit_rate: 1.0,
            ..base_test_config()
        };
        // Audit error: the cached kill is dropped and the mutant re-run.
        let (ms, hits, mut outs) = one_hit();
        let r = ErrAuditRunner {
            runs: AtomicUsize::new(0),
        };
        let (audited, dropped) = with_bare_ctx(&cfg, &r, &[], None, hits, |ctx| {
            audit_killer_hits(&cfg, ctx, &ms, &mut outs).unwrap();
            (
                ctx.stats.audited.load(Ordering::Relaxed),
                ctx.stats.dropped.load(Ordering::Relaxed),
            )
        });
        assert_eq!((audited, dropped), (1, 1));
        assert_eq!(r.runs.load(Ordering::Relaxed), 1);
        assert!(matches!(outs[0], MutantOutcome::Survived { .. }));

        // Runner can't audit: kill kept, nothing counted as audited.
        let (ms, hits, mut outs) = one_hit();
        let audited = with_bare_ctx(&cfg, &KillRunner, &[], None, hits, |ctx| {
            audit_killer_hits(&cfg, ctx, &ms, &mut outs).unwrap();
            ctx.stats.audited.load(Ordering::Relaxed)
        });
        assert_eq!(audited, 0);
        assert!(matches!(outs[0], MutantOutcome::Killed { .. }));

        // Budget exhausted: the audit is skipped, the runner never touched.
        let (ms, hits, mut outs) = one_hit();
        let past = Instant::now() - Duration::from_secs(1);
        with_bare_ctx(&cfg, &PanicRunner, &[], Some(past), hits, |ctx| {
            audit_killer_hits(&cfg, ctx, &ms, &mut outs).unwrap();
        });
        assert!(matches!(outs[0], MutantOutcome::Killed { .. }));
    }

    /// Runner that hands back one kill record and one kill-set, so `persist`
    /// has something to write.
    struct RecordingRunner;

    impl Runner for RecordingRunner {
        fn run(&self, _m: &Arc<Mutant>) -> Result<MutantOutcome> {
            unreachable!()
        }
        fn baseline(&self) -> Result<runner::BaselineStatus> {
            unreachable!()
        }
        fn take_kill_records(&self) -> Vec<crate::kill_order::KillRecord> {
            vec![crate::kill_order::KillRecord {
                file: "t.py".into(),
                operator: "arith-op-swap".into(),
                nodeid: TEST_A.into(),
            }]
        }
    }

    #[test]
    fn persist_survives_every_side_write_failing() {
        // A regular file used as a directory: every write beneath it fails.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let under = |name: &str| blocker.join(name);
        let dominators = tmp.path().join("dominators.json");
        std::fs::write(&dominators, "not json").unwrap();
        let cfg = Config {
            cache: true,
            cache_path: under("cache.json"),
            smart_order: true,
            kill_order_path: under("kill-order.json"),
            record_kill_sets: Some(under("kill-sets.jsonl")),
            history: true,
            history_path: under("history.jsonl"),
            dominators_path: dominators,
            max_time_secs: Some(1),
            ..base_test_config()
        };
        let m = Arc::new(arith_mutant("return x + 0", "+", "-"));
        let outcomes = vec![
            MutantOutcome::killed(m.clone()),
            MutantOutcome::skipped(m.clone(), TIME_BUDGET_FILTER),
        ];
        let (report, entry) = persist(
            &cfg,
            Instant::now(),
            &RecordingRunner,
            &Mutex::new(Cache::default()),
            &CacheStats::default(),
            Some(Instant::now()),
            outcomes,
        );
        assert_eq!(report.outcomes.len(), 2);
        assert_eq!(report.dominator_score, None, "broken store is ignored");
        assert!(entry.is_some(), "history entry still flows back");
        assert!(!under("history.jsonl").exists());
    }

    #[test]
    fn stale_dominator_store_yields_no_score() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dominators.json");
        let store = serde_json::json!({
            "version": crate::subsume::STORE_VERSION,
            "generated_at": "2026-01-01T00:00:00Z",
            "kill_sets": "ks.jsonl",
            "stats": {
                "records": 0, "killed": 0, "survived": 0, "killed_unattributed": 0,
                "other": 0, "classes": 0, "dominators": 0, "reduction_pct": 0.0,
                "dominator_score": null
            },
            "classes": [],
            "file_hashes": {}
        });
        std::fs::write(&path, store.to_string()).unwrap();
        let cfg = Config {
            dominators_path: path,
            ..base_test_config()
        };
        let m = Arc::new(arith_mutant("return x + 0", "+", "-"));
        // `t.py` was never recorded in the store: stale, so no score.
        assert_eq!(dominator_score(&cfg, &[MutantOutcome::killed(m)]), None);
    }

    #[test]
    fn analyze_unique_files_skips_unreadable_and_non_utf8_files() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("bin.py");
        std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x80]).unwrap();
        let mk = |file: PathBuf| {
            let mut m = arith_mutant("x + 0", "+", "-");
            m.file = file;
            Arc::new(m)
        };
        let ms = vec![mk(tmp.path().join("missing.py")), mk(binary.clone())];
        let a = analyze_unique_files(&ms, true, true);
        assert_eq!(a.hashes.len(), 1, "missing file gets no hash");
        assert!(a.hashes.contains_key(&binary), "byte-fallback hash");
        assert!(a.scope_maps.is_empty(), "unparseable: no scope map");
        assert!(a.sources.is_empty(), "non-utf8: no source");
    }

    #[test]
    fn hypothesis_seed_feeds_the_scope_prefix() {
        let none = base_test_config();
        let seeded = Config {
            hypothesis_seed: Some(7),
            ..base_test_config()
        };
        let other = Config {
            hypothesis_seed: Some(8),
            ..base_test_config()
        };
        let p = compute_scope_prefix(&seeded);
        assert_ne!(p, compute_scope_prefix(&none));
        assert_ne!(p, compute_scope_prefix(&other));
    }

    #[test]
    fn red_baseline_message_mentions_private_home_only_when_on() {
        let ms = vec![Arc::new(arith_mutant("return x + 0", "+", "-"))];
        let mut cfg = Config {
            verify_baseline: true,
            ..base_test_config()
        };
        let msg = |cfg: &Config| {
            preflight_baseline(cfg, &RedBaselineRunner, &ms)
                .unwrap_err()
                .to_string()
        };
        assert!(msg(&cfg).contains("--no-private-home"));
        cfg.private_home = false;
        assert!(!msg(&cfg).contains("--no-private-home"));
    }

    #[test]
    fn runner_build_picks_each_kind_without_spawning() {
        let m = Arc::new(arith_mutant("return x + 0", "+", "-"));
        for kind in [RunnerKind::Pytest, RunnerKind::Rstest, RunnerKind::Unittest] {
            let cfg = Config {
                runner: kind,
                tests: Some(PathBuf::from("tests")),
                ..base_test_config()
            };
            let r = runner::build(&cfg);
            assert!(r.take_kill_records().is_empty(), "{kind:?}");
            assert!(r.take_kill_sets().is_empty(), "{kind:?}");
            if matches!(kind, RunnerKind::Unittest) {
                // unittest can't select a single test: never audits.
                assert!(r.audit(&m, TEST_A).unwrap().is_none());
            }
        }
    }
}
