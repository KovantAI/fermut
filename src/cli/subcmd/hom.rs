//! `fermut hom` — the higher-order-mutant experiment (Phase 1).
//!
//! Measures whether **strongly-subsuming higher-order mutants (SSHOMs)** exist
//! in real Python. An SSHOM `h` built from first-order mutants `f1,f2` is killed
//! only by tests that kill *both*: `K(h) ≠ ∅ ∧ K(h) ⊆ K(f1) ∩ K(f2)`. One SSHOM
//! can then replace both FOMs with no loss of detection — the "fewer-but-
//! stronger" mechanism the HOM literature (all Java to date) reports.
//!
//! Pipeline:
//!   1. Load per-FOM kill-sets from a `fermut run --record-kill-sets` JSONL
//!      (Phase 0). Keep only *killed* FOMs — a survivor has no replacement value.
//!   2. Pair candidates: same file, non-overlapping ranges, and a non-empty
//!      kill-set overlap `K(f1) ∩ K(f2) ≠ ∅`. The overlap gate is exact, not a
//!      heuristic: `K(h) ⊆ K(f1) ∩ K(f2)`, so an empty intersection *cannot*
//!      yield an SSHOM — pruning it loses nothing and collapses the O(n²) pairing
//!      to the few pairs that could matter (the portable stand-in for the Java
//!      CPDA/variational search).
//!   3. Build each second-order mutant and run it in two `-x` stages: the
//!      shared killers `K(f1) ∩ K(f2)`, then the rest of `T_cov(f1) ∪ T_cov(f2)`.
//!      With schemata on, each pair is one case of its function's switch and
//!      each stage a fork of the template; otherwise (and for pairs the schema
//!      can't hold) both edits are spliced into a worker mirror.
//!   4. Classify sshom / decoupled (`K(h)=∅`) / non-subsuming, write a JSONL, and
//!      print SSHOM density + a first-cut net-FOM-reduction estimate.
//!
//! Reuses the hardened runner plumbing (mirror copy, PYTHONPATH pin, `.pyc`
//! suppression, drain-safe wait) rather than re-implementing it, so the
//! correctness fixes those carry still hold.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use rayon::prelude::*;
use ruff_text_size::{Ranged, TextRange, TextSize};
use serde::Deserialize;

use crate::cli::build_config::build_config;
use crate::cli::RunConfigArgs;
use crate::config::RunnerKind;
use crate::mutator::{self, Mutant};
use crate::runner::exit::{
    classify_exit, is_collection_failure, is_project_config_error, ExitVerdict, TestTool,
};
use crate::runner::process_group::kill_group;
use crate::runner::pytest::wait_draining_stdout;
use crate::runner::{apply_patch, configure_mirror_cmd, Mirror};

/// `fermut hom` arguments.
#[derive(clap::Args, Debug)]
pub(crate) struct HomArgs {
    /// Python source root (same target used for the Phase-0 run).
    /// Omitted: the config's `source_root`, else the current directory.
    pub(crate) path: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) cfg_args: RunConfigArgs,

    /// Per-FOM kill-sets JSONL from `fermut run --record-kill-sets` (Phase 0).
    #[arg(long, value_name = "PATH")]
    pub(crate) kill_sets: PathBuf,

    /// Write per-candidate HOM results (JSONL) here.
    #[arg(long, value_name = "PATH", default_value = "hom-results.jsonl")]
    pub(crate) out: PathBuf,

    /// Safety cap on candidate pairs actually run (highest kill-overlap first).
    /// Dropped pairs are reported so the coverage bound is never silent.
    #[arg(long, value_name = "N", default_value_t = 2000)]
    pub(crate) max_pairs: usize,

    /// Also pair FOMs in *different* functions. Off by default: SSHOMs are
    /// function-local fault interactions, so cross-function pairs are almost all
    /// non-subsuming noise that dominates cost. Pass this to reproduce the
    /// same-file-only gate.
    #[arg(long)]
    pub(crate) cross_function: bool,
}

/// The Phase-0 record fields this command reads (a subset of
/// [`crate::runner::kill_sets::KillSetRecord`], with owned `status` for
/// deserialization).
#[derive(Deserialize)]
struct FomKillSet {
    mutant_id: String,
    status: String,
    kill_set: Vec<String>,
}

/// A killed FOM joined to its kill-set and coverage-selected tests.
struct KilledFom {
    mutant: Mutant,
    kills: BTreeSet<String>,
    covering: BTreeSet<String>,
    /// Start offset of the innermost enclosing function, or `None` for
    /// module-level code. The same-function gate pairs only FOMs sharing this.
    fn_id: Option<u32>,
}

/// One candidate pair's outcome.
#[derive(serde::Serialize)]
struct HomResult {
    f1_id: String,
    f2_id: String,
    file: String,
    line1: u32,
    line2: u32,
    /// sshom | decoupled | non-subsuming
    class: &'static str,
    k_f1: usize,
    k_f2: usize,
    intersection: usize,
}

pub(crate) fn run(args: HomArgs) -> Result<()> {
    let HomArgs {
        path,
        cfg_args,
        kill_sets,
        out,
        max_pairs,
        cross_function,
    } = args;
    tracing::warn!("`fermut hom` is experimental: its flags and output format may change");
    let cfg = build_config(path, cfg_args)?;

    let coverage = cfg.coverage.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "`fermut hom` needs per-test coverage to bound each candidate's test set. \
             Generate it (`COVERAGE_CORE=ctrace pytest --cov=src --cov-context=test`) and pass `--coverage`."
        )
    })?;
    let exe = match cfg.runner {
        RunnerKind::Pytest => "pytest",
        RunnerKind::Rstest => "rstest",
        RunnerKind::Unittest => {
            anyhow::bail!("`fermut hom` supports the pytest/rstest runner only")
        }
    };

    // 1. Load Phase-0 kill-sets, keep the killed FOMs' kill-sets by mutant id.
    let fom_kills = load_kill_sets(&kill_sets)?;
    let killed_ids: usize = fom_kills.len();

    // 2. Regenerate the FOM catalogue and join by id — the catalogue carries the
    //    ranges/replacements the JSONL omits.
    let catalogue =
        mutator::collect_from_tree(&cfg.source_root, &cfg.exclude, &cfg.ignore_patterns)
            .context("regenerating FOM catalogue")?;
    // Cache each file's function spans once so the same-function gate is a cheap
    // lookup per mutant rather than a re-parse.
    let mut spans_by_file: HashMap<PathBuf, Vec<TextRange>> = HashMap::new();
    let mut killed: Vec<KilledFom> = Vec::new();
    for m in catalogue {
        let Some(kills) = fom_kills.get(&m.id) else {
            continue;
        };
        let covering: BTreeSet<String> = coverage
            .tests_for_mutant(&m)
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default();
        let spans = spans_by_file
            .entry(m.file.clone())
            .or_insert_with(|| function_spans(&m.file));
        let fn_id = enclosing_fn(spans, m.range.start());
        killed.push(KilledFom {
            mutant: m,
            kills: kills.iter().cloned().collect(),
            covering,
            fn_id,
        });
    }
    // Dedup FOMs that are the *same textual edit* under different operator tags
    // (e.g. number-shift vs number-to-zero both emitting `1->0`). Identical edits
    // produce identical higher-order mutants, so keeping one representative is
    // lossless for HOM pairing — unlike kill-set dedup, which is NOT safe here:
    // two FOMs with the same K(f) can still combine differently with a third
    // edit, so their second-order behavior can diverge.
    let before_dedup = killed.len();
    let mut seen_edits = std::collections::HashSet::new();
    killed.retain(|k| {
        let m = &k.mutant;
        seen_edits.insert((
            m.file.clone(),
            u32::from(m.range.start()),
            u32::from(m.range.end()),
            m.replacement.clone(),
        ))
    });
    tracing::info!(
        killed_foms = killed.len(),
        from_records = killed_ids,
        identical_edits_collapsed = before_dedup - killed.len(),
        "joined killed FOMs to the catalogue (after identical-edit dedup)"
    );

    // 3. Candidate pairs: same file, non-overlapping ranges, non-empty kill
    //    overlap. Rank by overlap size so the `--max-pairs` cap keeps the
    //    likeliest SSHOMs.
    let mut candidates: Vec<(usize, usize, usize)> = Vec::new(); // (i, j, overlap)
    for i in 0..killed.len() {
        for j in (i + 1)..killed.len() {
            let a = &killed[i];
            let b = &killed[j];
            if a.mutant.file != b.mutant.file {
                continue;
            }
            // Same-function gate (default): SSHOMs are function-local, so cross-
            // function pairs are almost all non-subsuming noise. `--cross-function`
            // disables it. Two module-level FOMs (both `None`) count as same scope.
            if !cross_function && a.fn_id != b.fn_id {
                continue;
            }
            if ranges_overlap(&a.mutant, &b.mutant) {
                continue;
            }
            let overlap = a.kills.intersection(&b.kills).count();
            if overlap == 0 {
                continue;
            }
            candidates.push((i, j, overlap));
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.2));
    let total_candidates = candidates.len();
    let dropped = total_candidates.saturating_sub(max_pairs);
    if dropped > 0 {
        tracing::warn!(
            total_candidates,
            max_pairs,
            dropped,
            "candidate pairs exceed --max-pairs; running the {max_pairs} highest-overlap pairs \
             and dropping {dropped} (raise --max-pairs for full coverage)"
        );
        candidates.truncate(max_pairs);
    }

    // 4. Build + run each HOM. Parallel across rayon workers, each reusing its
    //    own thread-local mirror (via `with_worker_mirror`) — the same
    //    per-worker-mirror reuse the engine relies on, so a HOM's two-edit patch
    //    and revert stay isolated to one worker. A single pair's failure (spawn
    //    error, unreadable mirror file) drops that pair rather than the run.
    //
    //    Results stream to the output file as they complete (locked writer) so a
    //    long run is watchable and survives an interrupt with partial data; a
    //    progress counter logs every 1000 pairs.
    let tests = cfg.tests_path();
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let python = cfg.python.as_deref();
    let source_root = &cfg.source_root;

    // Schemata: each pair is one more case in its function's switch (in each
    // of its two, for a cross-function pair), and each stage a fork of a
    // template that imported and collected the suite once. Pairs the schema
    // can't hold and runs it can't answer go to the spawn path below.
    #[cfg(unix)]
    let schema = crate::runner::schema_runner(&cfg);
    #[cfg(unix)]
    if let Some(r) = &schema {
        r.prepare_entries(
            candidates
                .iter()
                .map(|&(i, j, _)| hom_entry(&killed[i].mutant, &killed[j].mutant))
                .collect(),
        );
    }

    let writer = std::sync::Mutex::new(open_writer(&out)?);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let total = candidates.len();
    tracing::info!(pairs = total, "running HOM candidates");

    let results: Vec<HomResult> = candidates
        .par_iter()
        .filter_map(|(i, j, _overlap)| {
            let a = &killed[*i];
            let b = &killed[*j];
            let inter: BTreeSet<String> = a.kills.intersection(&b.kills).cloned().collect();
            // Tests outside the intersection decide sshom vs non-subsuming; the
            // intersection alone decides sshom vs decoupled. Both stages use `-x`
            // (we only need "any killer?" per stage), so most pairs short-circuit
            // instead of running the full union without `-x`.
            let outside: Vec<String> = a
                .covering
                .union(&b.covering)
                .filter(|t| !inter.contains(*t))
                .cloned()
                .collect();
            let inter_vec: Vec<String> = inter.iter().cloned().collect();
            if inter_vec.is_empty() && outside.is_empty() {
                return None; // no tests to run — cannot classify
            }
            #[cfg(unix)]
            let id = hom_id(&a.mutant, &b.mutant);
            let stage = |stage_tests: &[String]| -> Result<bool> {
                if stage_tests.is_empty() {
                    return Ok(false);
                }
                let spawn = || {
                    crate::runner::with_worker_mirror(&tests, cfg.isolation, python, |mirror| {
                        let _patched = apply_both(mirror, &a.mutant, &b.mutant)?;
                        any_test_fails(mirror, cfg.private_home, python, exe, timeout, stage_tests)
                    })
                };
                #[cfg(unix)]
                if let Some(r) = &schema {
                    return r.probe(&id, stage_tests, spawn);
                }
                spawn()
            };
            let staged = stage(&inter_vec).and_then(|i| Ok((i, stage(&outside)?)));
            let (inter_kill, outside_kill) = match staged {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(f1 = %a.mutant.id, f2 = %b.mutant.id, error = %e, "HOM run failed; skipping pair");
                    return None;
                }
            };
            let class = classify(inter_kill, outside_kill);
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % 1000 == 0 {
                tracing::info!(done = n, total, "HOM progress");
            }
            let result = HomResult {
                f1_id: a.mutant.id.clone(),
                f2_id: b.mutant.id.clone(),
                file: rel_file(&a.mutant.file, source_root),
                line1: a.mutant.line,
                line2: b.mutant.line,
                class,
                k_f1: a.kills.len(),
                k_f2: b.kills.len(),
                intersection: inter.len(),
            };
            if let Ok(mut w) = writer.lock() {
                use std::io::Write;
                if let Ok(line) = serde_json::to_string(&result) {
                    let _ = writeln!(w, "{line}");
                }
            }
            Some(result)
        })
        .collect();

    if let Ok(mut w) = writer.lock() {
        use std::io::Write;
        let _ = w.flush();
    }
    let gate = if cross_function {
        "same-file (cross-function allowed)"
    } else {
        "same-function"
    };
    print_summary(
        &results,
        killed.len(),
        total_candidates,
        dropped,
        &out,
        gate,
    );
    Ok(())
}

/// Classify from the two staged booleans: `inter_kill` = some test in
/// `K(f1)∩K(f2)` kills the HOM; `outside_kill` = some test outside the
/// intersection kills it.
/// - any outside killer → **non-subsuming** (`K(h) ⊄ intersection`)
/// - else killed only within the intersection → **sshom**
/// - else nothing kills it → **decoupled** (the faults mask each other)
fn classify(inter_kill: bool, outside_kill: bool) -> &'static str {
    if outside_kill {
        "non-subsuming"
    } else if inter_kill {
        "sshom"
    } else {
        "decoupled"
    }
}

/// Byte ranges of every function definition in `file` (all nesting depths).
/// Empty on an unreadable file — the same-function gate then treats those
/// mutants as module-level (`None`).
fn function_spans(file: &Path) -> Vec<TextRange> {
    std::fs::read_to_string(file)
        .map(|s| crate::ast_hash::function_spans(&s))
        .unwrap_or_default()
}

/// The innermost function span containing `off`, keyed by its start offset;
/// `None` if the offset is module-level. "Innermost" = the shortest containing
/// span, so a method inside a class inside a function resolves to the method.
fn enclosing_fn(spans: &[TextRange], off: TextSize) -> Option<u32> {
    spans
        .iter()
        .filter(|r| r.contains(off))
        .min_by_key(|r| r.len())
        .map(|r| u32::from(r.start()))
}

/// Two mutants' byte ranges touch — applying both edits is ill-defined, so the
/// pair is skipped. Half-open ranges: adjacency (`a.end == b.start`) is fine.
fn ranges_overlap(a: &Mutant, b: &Mutant) -> bool {
    let (a0, a1) = (u32::from(a.range.start()), u32::from(a.range.end()));
    let (b0, b1) = (u32::from(b.range.start()), u32::from(b.range.end()));
    a0 < b1 && b0 < a1
}

/// The schema id of the pair's higher-order mutant.
#[cfg(unix)]
fn hom_id(f1: &Mutant, f2: &Mutant) -> String {
    format!("hom:{}+{}", f1.id, f2.id)
}

/// The pair's higher-order mutant as one schema entry: both edits.
#[cfg(unix)]
fn hom_entry(f1: &Mutant, f2: &Mutant) -> crate::runner::schema::SchemaEntry {
    let mut entry = crate::runner::schema::SchemaEntry::from(f1);
    entry
        .edits
        .extend(crate::runner::schema::SchemaEntry::from(f2).edits);
    entry.id = hom_id(f1, f2);
    entry
}

/// Splice both edits into the mirror via [`apply_patch`] guards in
/// **descending start-offset order**: patching the later edit first leaves the
/// earlier edit's byte offsets valid (bytes before it are untouched), so a
/// length-changing replacement can't shift the second range. Each guard
/// restores the bytes it found, so they must drop in reverse: a tuple drops
/// its first field first, hence `(earlier, later)`.
fn apply_both(
    mirror: &Mirror,
    f1: &Mutant,
    f2: &Mutant,
) -> Result<(crate::runner::PatchGuard, crate::runner::PatchGuard)> {
    let (later, earlier) = if f1.range.start() >= f2.range.start() {
        (f1, f2)
    } else {
        (f2, f1)
    };
    let later = apply_patch(mirror, later)?;
    Ok((apply_patch(mirror, earlier)?, later))
}

/// Run `tests` against the already-patched `mirror` with `-x` and report whether
/// any failed. `-x` short-circuits at the first failure, so this is a cheap
/// "any killer?" probe. Both stages of a pair are classified alike here and on
/// the schemata path, as the main runner does: a failing (1) or interrupting
/// (2) test or a signal is a kill. Exit 3 or 4 is a kill only when the pair
/// broke an import (pytest can't resolve the node ids, or a 9.2+ `-p` plugin
/// import fails), checked by re-running with `--collect-only`; otherwise it,
/// like exit 5 (nothing ran), fails the pair rather than counting as one.
fn any_test_fails(
    mirror: &Mirror,
    private_home: bool,
    python: Option<&Path>,
    exe: &str,
    timeout: Duration,
    tests: &[String],
) -> Result<bool> {
    let command = |extra: Option<&str>| -> Result<std::process::Command> {
        let mut cmd = crate::runner::pytest::framework_command(python, exe);
        cmd.arg("-x").arg("--tb=no").arg("-q");
        cmd.args(extra);
        crate::runner::pytest::single_worker(&mut cmd, exe, &[]);
        cmd.args(tests);
        configure_mirror_cmd(&mut cmd, mirror, private_home)?;
        Ok(cmd)
    };
    let mut cmd = command(None)?;
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    let child = cmd.spawn().context("spawning HOM test run")?;
    let (status, _output) = wait_draining_stdout(child, timeout, kill_group)?;
    // A timeout (`None`) counts as not-killed for this stage — conservative:
    // it can only understate a kill, never fabricate one.
    let Some(status) = status else {
        return Ok(false);
    };
    match classify_exit(TestTool::Pytest, status.code()) {
        ExitVerdict::Survived => Ok(false),
        ExitVerdict::Killed => Ok(true),
        ExitVerdict::Anomaly(3 | 4)
            if import_broke(command(Some("--collect-only"))?, mirror, timeout) =>
        {
            Ok(true)
        }
        ExitVerdict::Anomaly(code) => anyhow::bail!("{exe} exited {code}"),
    }
}

/// Run `cmd` (the stage's `--collect-only` twin) and report whether a selected
/// module failed to collect or project code raised while pytest configured
/// itself — the main runner's broken-import check. Any probe failure (spawn
/// error, timeout, unreadable output) answers `false`.
fn import_broke(mut cmd: std::process::Command, mirror: &Mirror, timeout: Duration) -> bool {
    let mut probe = || -> Result<bool> {
        let log = tempfile::NamedTempFile::new().context("creating collection probe log")?;
        cmd.stdout(log.as_file().try_clone()?)
            .stderr(log.as_file().try_clone()?);
        let child = cmd.spawn().context("spawning HOM collection probe")?;
        if wait_draining_stdout(child, timeout, kill_group)?
            .0
            .is_none()
        {
            return Ok(false);
        }
        let out = std::fs::read(log.path()).context("reading collection probe log")?;
        let out = String::from_utf8_lossy(&out);
        Ok(is_collection_failure(&out) || is_project_config_error(&out, &mirror.root))
    };
    probe().unwrap_or(false)
}

fn load_kill_sets(path: &Path) -> Result<HashMap<String, Vec<String>>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading kill-sets file {}", path.display()))?;
    let mut map = HashMap::new();
    for (n, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: FomKillSet = serde_json::from_str(line)
            .with_context(|| format!("parsing kill-sets line {}", n + 1))?;
        // Only killed FOMs carry a meaningful kill-set; survivors/timeouts/errors
        // have no replacement value and can't found an SSHOM.
        if rec.status == "killed" && !rec.kill_set.is_empty() {
            map.insert(rec.mutant_id, rec.kill_set);
        }
    }
    Ok(map)
}

fn rel_file(file: &Path, source_root: &Path) -> String {
    file.strip_prefix(source_root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Open the output file for streaming JSONL writes (truncating any prior run).
fn open_writer(path: &Path) -> Result<std::io::BufWriter<std::fs::File>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating HOM output dir {}", parent.display()))?;
        }
    }
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(std::io::BufWriter::new(file))
}

#[allow(clippy::too_many_arguments)]
fn print_summary(
    results: &[HomResult],
    killed_foms: usize,
    total_candidates: usize,
    dropped: usize,
    out: &Path,
    gate: &str,
) {
    let sshom = results.iter().filter(|r| r.class == "sshom").count();
    let decoupled = results.iter().filter(|r| r.class == "decoupled").count();
    let non_sub = results
        .iter()
        .filter(|r| r.class == "non-subsuming")
        .count();
    let run = results.len();

    // First-cut net FOM reduction: distinct FOMs that appear in at least one
    // SSHOM pair could be collapsed onto their subsuming HOMs. A true minimum
    // needs a set-cover over the SSHOM graph — this is the loose upper bound.
    let mut covered: BTreeSet<&str> = BTreeSet::new();
    for r in results.iter().filter(|r| r.class == "sshom") {
        covered.insert(&r.f1_id);
        covered.insert(&r.f2_id);
    }
    let density = if run > 0 {
        100.0 * sshom as f64 / run as f64
    } else {
        0.0
    };

    println!("HOM experiment (2nd order)");
    println!("  gate:               {}", gate);
    println!("  killed FOMs:        {killed_foms}");
    println!("  candidate pairs:    {total_candidates} (kill-overlap gate)");
    if dropped > 0 {
        println!("  dropped (max-pairs):{dropped}");
    }
    println!("  pairs run:          {run}");
    println!("  SSHOMs:             {sshom}  ({density:.1}% of run pairs)");
    println!("  decoupled (K(h)=∅): {decoupled}");
    println!("  non-subsuming:      {non_sub}");
    println!(
        "  FOMs in an SSHOM:   {} / {killed_foms}  (loose collapse upper bound)",
        covered.len()
    );
    println!("  results:            {}", out.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_text_size::{TextRange, TextSize};

    #[test]
    fn enclosing_fn_resolves_innermost_and_module_level() {
        let src = "\
x = 1

def outer():
    y = 2
    def inner():
        z = 3
        return z
    return inner()
";
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("m.py");
        std::fs::write(&p, src).unwrap();
        let spans = function_spans(&p);
        assert_eq!(spans.len(), 2, "outer + inner");

        let off = |needle: &str| TextSize::from(src.find(needle).unwrap() as u32);
        // module-level assignment → None
        assert_eq!(enclosing_fn(&spans, off("x = 1")), None);
        // `z = 3` is inside inner (the shortest containing span)
        let inner_id = enclosing_fn(&spans, off("z = 3"));
        // `y = 2` is inside outer only
        let outer_id = enclosing_fn(&spans, off("y = 2"));
        assert!(inner_id.is_some() && outer_id.is_some());
        assert_ne!(inner_id, outer_id, "inner and outer are distinct functions");
        // `return z` also resolves to inner — same function as `z = 3`
        assert_eq!(enclosing_fn(&spans, off("return z")), inner_id);
    }

    #[test]
    fn classify_sshom_decoupled_non_subsuming() {
        // Killed only within the intersection → SSHOM.
        assert_eq!(classify(true, false), "sshom");
        // Killed by nothing → the faults masked each other.
        assert_eq!(classify(false, false), "decoupled");
        // A killer outside the intersection → not subsuming (regardless of inter).
        assert_eq!(classify(true, true), "non-subsuming");
        assert_eq!(classify(false, true), "non-subsuming");
    }

    fn mutant_at(start: u32, end: u32) -> Mutant {
        Mutant {
            id: format!("m@{start}"),
            file: PathBuf::from("f.py"),
            operator: crate::mutator::Operator::ArithOpSwap,
            range: TextRange::new(TextSize::from(start), TextSize::from(end)),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    #[test]
    fn ranges_overlap_detects_touching_but_allows_adjacent() {
        // [0,2) vs [1,3) overlap.
        assert!(ranges_overlap(&mutant_at(0, 2), &mutant_at(1, 3)));
        // [0,2) vs [2,4) are adjacent, not overlapping.
        assert!(!ranges_overlap(&mutant_at(0, 2), &mutant_at(2, 4)));
        // Disjoint.
        assert!(!ranges_overlap(&mutant_at(0, 2), &mutant_at(5, 7)));
    }

    #[test]
    fn load_kill_sets_keeps_only_killed_with_nonempty_sets() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ks.jsonl");
        std::fs::write(
            &p,
            "\
{\"mutant_id\":\"a\",\"file\":\"f.py\",\"operator\":\"o\",\"line\":1,\"status\":\"killed\",\"kill_set\":[\"t1\",\"t2\"]}\n\
{\"mutant_id\":\"b\",\"file\":\"f.py\",\"operator\":\"o\",\"line\":2,\"status\":\"survived\",\"kill_set\":[]}\n\
{\"mutant_id\":\"c\",\"file\":\"f.py\",\"operator\":\"o\",\"line\":3,\"status\":\"killed\",\"kill_set\":[]}\n",
        )
        .unwrap();
        let map = load_kill_sets(&p).unwrap();
        assert_eq!(map.len(), 1, "only the killed FOM with a non-empty set");
        assert_eq!(
            map.get("a").unwrap(),
            &vec!["t1".to_string(), "t2".to_string()]
        );
    }

    #[test]
    fn load_kill_sets_skips_blank_lines_and_names_a_bad_line() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ks.jsonl");
        std::fs::write(
            &p,
            "\n   \n{\"mutant_id\":\"a\",\"status\":\"killed\",\"kill_set\":[\"t\"]}\n",
        )
        .unwrap();
        assert_eq!(load_kill_sets(&p).unwrap().len(), 1);

        std::fs::write(&p, "\n{not json}\n").unwrap();
        let err = load_kill_sets(&p).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
        assert!(load_kill_sets(&tmp.path().join("missing.jsonl")).is_err());
    }

    #[test]
    fn open_writer_creates_parent_dirs_and_errors_when_parent_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a").join("b").join("out.jsonl");
        {
            use std::io::Write;
            let mut w = open_writer(&p).unwrap();
            writeln!(w, "x").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x\n");

        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let err = open_writer(&blocker.join("out.jsonl")).unwrap_err();
        assert!(err.to_string().contains("creating HOM output dir"), "{err}");
    }

    #[test]
    fn rel_file_strips_source_root_or_keeps_path() {
        assert_eq!(
            rel_file(Path::new("/r/src/m.py"), Path::new("/r")),
            "src/m.py"
        );
        assert_eq!(rel_file(Path::new("/x/m.py"), Path::new("/r")), "/x/m.py");
    }

    #[test]
    fn both_edits_apply_and_drop_back_to_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        let src = "def f(a, b):\n    return a + b * 2\n";
        std::fs::write(dir.join("m.py"), src).unwrap();
        let mirror =
            crate::runner::build_mirror(&dir.join("tests"), crate::config::IsolationMode::Copy)
                .unwrap();
        let edit = |needle: &str, replacement: &str| {
            let start = src.find(needle).unwrap() as u32;
            Mutant {
                file: dir.join("m.py"),
                range: TextRange::new(
                    TextSize::from(start),
                    TextSize::from(start + needle.len() as u32),
                ),
                replacement: replacement.into(),
                ..mutant_at(0, 0)
            }
        };
        // A length-changing earlier edit: the later one's offsets must hold.
        let (plus, two) = (edit("+", " - 1 -"), edit("2", "3"));
        let target = mirror.root.join("m.py");
        for (f1, f2) in [(&plus, &two), (&two, &plus)] {
            {
                let _patched = apply_both(&mirror, f1, f2).unwrap();
                assert_eq!(
                    std::fs::read_to_string(&target).unwrap(),
                    "def f(a, b):\n    return a  - 1 - b * 3\n"
                );
            }
            assert_eq!(std::fs::read_to_string(&target).unwrap(), src, "restored");
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_stage_kills_on_a_failing_test_or_broken_import() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
        let mirror =
            crate::runner::build_mirror(&dir.join("tests"), crate::config::IsolationMode::Copy)
                .unwrap();
        // A fake interpreter exiting with the code its "test id" names; an id
        // `N:broken` also reports a broken import under `--collect-only`, and
        // `sig` dies by a signal.
        let fake = dir.join("fake-python");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a; do [ \"$a\" = --collect-only ] && probe=1; last=$a; done\n\
             [ \"$last\" = sig ] && kill -KILL $$\n\
             case \"$last\" in *:broken) [ -n \"$probe\" ] && echo 'ImportError while loading conftest';; esac\n\
             exit ${last%%:*}\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let run = |code: &str| {
            any_test_fails(
                &mirror,
                true,
                Some(&fake),
                "pytest",
                Duration::from_secs(10),
                &[code.to_string()],
            )
        };
        assert!(!run("0").unwrap());
        assert!(run("1").unwrap() && run("2").unwrap() && run("sig").unwrap());
        assert!(
            run("3:broken").unwrap() && run("4:broken").unwrap(),
            "import broke: a kill"
        );
        for code in ["3", "4", "5", "5:broken"] {
            assert!(run(code).is_err(), "exit {code} fails the pair");
        }
    }

    #[test]
    fn print_summary_handles_no_results() {
        // Zero runs must not divide by zero.
        print_summary(&[], 0, 0, 0, Path::new("out.jsonl"), "same-function");
    }
}
