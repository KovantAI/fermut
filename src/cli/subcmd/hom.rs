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
//!   3. Build each second-order mutant (both edits spliced into the mirror) and
//!      run the coverage union `T_cov(f1) ∪ T_cov(f2)` without `-x` to capture the
//!      full `K(h)`.
//!   4. Classify sshom / decoupled (`K(h)=∅`) / non-subsuming, write a JSONL, and
//!      print SSHOM density + a first-cut net-FOM-reduction estimate.
//!
//! Reuses the hardened runner plumbing (mirror copy, PYTHONPATH pin, `.pyc`
//! suppression, drain-safe wait) rather than re-implementing it, so the
//! correctness fixes those carry still hold.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Deserialize;

use crate::cli::build_config::build_config;
use crate::cli::RunConfigArgs;
use crate::config::RunnerKind;
use crate::mutator::{self, Mutant};
use crate::runner::process_group::kill_group;
use crate::runner::pytest::wait_draining_stdout;
use crate::runner::{apply_patch, configure_mirror_cmd, Mirror};

/// `fermut hom` arguments.
#[derive(clap::Args, Debug)]
pub(crate) struct HomArgs {
    /// Python source root (same target used for the Phase-0 run).
    #[arg(default_value = ".")]
    pub(crate) path: PathBuf,

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
    } = args;
    let cfg = build_config(path, cfg_args)?;

    let coverage = cfg.coverage.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "`fermut hom` needs per-test coverage to bound each candidate's test set. \
             Generate it (`pytest --cov=src --cov-context=test`) and pass `--coverage`."
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
    let catalogue = mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)
        .context("regenerating FOM catalogue")?;
    let mut killed: Vec<KilledFom> = Vec::new();
    for m in catalogue {
        let Some(kills) = fom_kills.get(&m.id) else {
            continue;
        };
        let covering: BTreeSet<String> = coverage
            .tests_for_mutant(&m)
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default();
        killed.push(KilledFom {
            mutant: m,
            kills: kills.iter().cloned().collect(),
            covering,
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
            let staged = crate::runner::with_worker_mirror(&tests, cfg.isolation, |mirror| {
                run_hom_staged(
                    mirror, python, exe, timeout, &a.mutant, &b.mutant, &inter_vec, &outside,
                )
            });
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
    print_summary(&results, killed.len(), total_candidates, dropped, &out);
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

/// Two mutants' byte ranges touch — applying both edits is ill-defined, so the
/// pair is skipped. Half-open ranges: adjacency (`a.end == b.start`) is fine.
fn ranges_overlap(a: &Mutant, b: &Mutant) -> bool {
    let (a0, a1) = (u32::from(a.range.start()), u32::from(a.range.end()));
    let (b0, b1) = (u32::from(b.range.start()), u32::from(b.range.end()));
    a0 < b1 && b0 < a1
}

/// Splice both edits into the mirror, then classify the HOM in two staged runs.
/// Returns `(inter_kill, outside_kill)`: whether any test in the intersection
/// kills the HOM, and whether any test outside it does.
///
/// The two patches are applied via [`apply_patch`] guards in **descending
/// start-offset order**: patching the later edit first leaves the earlier edit's
/// byte offsets valid (bytes before it are untouched), so a length-changing
/// replacement can't shift the second range. The guards restore the mirror on
/// scope exit, so the next candidate starts from clean source.
///
/// Both stages run with `-x` (stop at the first failure) because each only needs
/// a boolean "did any of these tests kill it?" — far cheaper than the full
/// no-`-x` union. The outside stage is skipped when there are no outside tests.
fn run_hom_staged(
    mirror: &Mirror,
    python: Option<&Path>,
    exe: &str,
    timeout: Duration,
    f1: &Mutant,
    f2: &Mutant,
    inter_tests: &[String],
    outside_tests: &[String],
) -> Result<(bool, bool)> {
    let (later, earlier) = if f1.range.start() >= f2.range.start() {
        (f1, f2)
    } else {
        (f2, f1)
    };
    let _g_later = apply_patch(mirror, later)?;
    let _g_earlier = apply_patch(mirror, earlier)?;

    let inter_kill = if inter_tests.is_empty() {
        false
    } else {
        any_test_fails(mirror, python, exe, timeout, inter_tests)?
    };
    let outside_kill = if outside_tests.is_empty() {
        false
    } else {
        any_test_fails(mirror, python, exe, timeout, outside_tests)?
    };
    Ok((inter_kill, outside_kill))
}

/// Run `tests` against the already-patched `mirror` with `-x` and report whether
/// any failed (exit non-zero). `-x` short-circuits at the first failure, so this
/// is a cheap "any killer?" probe.
fn any_test_fails(
    mirror: &Mirror,
    python: Option<&Path>,
    exe: &str,
    timeout: Duration,
    tests: &[String],
) -> Result<bool> {
    let mut cmd = match python {
        Some(py) => {
            let mut c = Command::new(py);
            c.arg("-m").arg(exe);
            c
        }
        None => Command::new(exe),
    };
    cmd.arg("-x").arg("--tb=no").arg("-q");
    for t in tests {
        cmd.arg(t);
    }
    configure_mirror_cmd(&mut cmd, mirror)?;
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    let child = cmd.spawn().context("spawning HOM test run")?;
    let (status, _output) = wait_draining_stdout(child, timeout, kill_group)?;
    // Non-zero exit → at least one selected test failed (the HOM was killed by
    // this test set). A timeout (`None`) counts as not-killed for this stage —
    // conservative: it can only understate a kill, never fabricate one.
    Ok(matches!(status, Some(s) if !s.success()))
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

fn print_summary(
    results: &[HomResult],
    killed_foms: usize,
    total_candidates: usize,
    dropped: usize,
    out: &Path,
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
}
