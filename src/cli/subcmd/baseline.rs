//! `fermut baseline` — the day-one "where do I stand" command.
//!
//! `run`/`score`/`next` all assume you already know what mutation testing is
//! and have a `.coverage` wired up. A newcomer has neither. `baseline` is the
//! single command they run first: it sanity-checks the environment, measures
//! line coverage, runs a fast sampled mutation pass over *covered* code, and
//! prints the one number nobody has shown them before — the gap between "code
//! ran under test" (line coverage) and "a test would actually notice if the
//! code were wrong" (mutation score).
//!
//! Pipeline, all reuse:
//!   1. `doctor::diagnose` — block early if the toolchain/config is broken.
//!   2. `coverage::coverage` — build/refresh `.coverage`, then read line %.
//!   3. `engine::run` with `--sample` + coverage filter — mutation score on
//!      covered code only (mutating dead code conflates two problems).
//!   4. Grade band + gap + top survivor files + a pointer to `fermut next`.
//!
//! The result is appended to `history.jsonl` with `baseline: true` so it
//! anchors the `trend`/`score` graph at run zero.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::cli::build_config::build_config;
use crate::cli::FilterArgs;
use crate::report::{MutantOutcome, Report};

/// Default fraction of mutants tested in the sampled pass. Tuned for a
/// minutes-not-hours first impression; `--full` overrides to 1.0.
const DEFAULT_SAMPLE: f64 = 0.1;
/// Fixed seed so two baselines on the same tree report the same number.
const SAMPLE_SEED: u64 = 0;

#[derive(Debug, Clone)]
pub struct BaselineOpts {
    pub path: PathBuf,
    /// Skip sampling — mutate every covered mutant for an exact score.
    pub full: bool,
    /// Sampling fraction when not `--full`. Defaults to `DEFAULT_SAMPLE`.
    pub sample: Option<f64>,
    /// How many worst-offender files to list. Defaults to 3.
    pub top: usize,
    pub format: BaselineFormat,
    /// Filter chain (coverage path, excludes, op allow/deny). Threaded
    /// through from the CLI exactly like `run`.
    pub filter: FilterArgs,
}

#[derive(Debug, Clone, Copy)]
pub enum BaselineFormat {
    Human,
    Json,
}

/// One grade band for the headline verdict. The thresholds are on the
/// mutation score *of covered code* — line coverage can't earn a grade
/// because a 100%-covered suite full of assertion-free tests still scores
/// near zero, which is the whole lesson.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Grade {
    Strong,
    Ok,
    Weak,
    Smoke,
    /// Nothing scored — the run's mutation score is the vacuous 100.0 floor,
    /// not a measurement, so no band applies (all-errored / all-skipped).
    Na,
}

impl Grade {
    fn of(mutation_score: f64) -> Self {
        match mutation_score {
            s if s >= 80.0 => Grade::Strong,
            s if s >= 60.0 => Grade::Ok,
            s if s >= 40.0 => Grade::Weak,
            _ => Grade::Smoke,
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Grade::Strong => "tests catch most logic changes",
            Grade::Ok => "real gaps in covered code",
            Grade::Weak => "many covered lines untested for behavior",
            Grade::Smoke => "mostly smoke tests — they run code but assert little",
            Grade::Na => "nothing scored — no test-quality signal",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct BaselineReport {
    /// Line coverage percent, from coverage.py over the measured source root.
    /// `null` when it couldn't be read (the mutation half still stands).
    #[serde(skip_serializing_if = "Option::is_none")]
    line_coverage: Option<f64>,
    /// Mutation score on covered code, percent. Carries the vacuous `100.0`
    /// floor when `scored == 0` — read `scored` to tell a genuine perfect run
    /// from a scoreless N/A (mirrors [`crate::report::Summary`]).
    mutation_score: f64,
    /// Mutants with a real verdict (`killed + timed_out + survived`) — the
    /// score denominator. `0` means `mutation_score` is a vacuous floor, not a
    /// measurement (grade `na`), so JSON/MCP consumers must not read the score
    /// as a genuine 100%.
    scored: usize,
    /// `100 - mutation_score`: of the code tests *do* execute, the share of
    /// behavior changes no test would notice — the false-confidence gap.
    /// This is on the covered-code denominator (same as `mutation_score`),
    /// NOT `line_coverage - mutation_score`: those two are on different
    /// denominators (whole file vs covered lines) and their difference can go
    /// negative, which is meaningless. Always present (mutation always runs).
    quality_gap: f64,
    /// `100 - line_coverage`: the separate axis — lines no test executes at
    /// all. `null` without a line-coverage reading. Orthogonal to
    /// `quality_gap`: one is "untested behavior in covered code", the other
    /// is "code never run".
    #[serde(skip_serializing_if = "Option::is_none")]
    untested_risk: Option<f64>,
    grade: Grade,
    killed: usize,
    survived: usize,
    /// True when this was a sampled (not full) pass — the score is an
    /// estimate, not the exact figure `run` would produce.
    sampled: bool,
    sample_fraction: f64,
    /// Files with the most survivors, worst first. The first places to spend
    /// test-writing effort.
    worst_files: Vec<FileSurvivors>,
}

#[derive(Debug, Serialize)]
struct FileSurvivors {
    file: String,
    survivors: usize,
}

pub fn baseline(opts: BaselineOpts) -> Result<()> {
    let format = opts.format;
    let out = compute_baseline(opts)?;
    match format {
        BaselineFormat::Json => println!("{}", serde_json::to_string_pretty(&out)?),
        BaselineFormat::Human => print_human(&out),
    }
    Ok(())
}

/// Run the baseline pipeline and return the structured report without
/// printing. Shared by the `baseline` subcommand and the MCP
/// `fermut_baseline` tool so both surface identical numbers.
pub(crate) fn compute_baseline(opts: BaselineOpts) -> Result<BaselineReport> {
    // 1. Gate on environment health. A broken toolchain produces a
    //    confidently-wrong score, which is worse than no score on day one.
    let diag = crate::cli::subcmd::doctor::diagnose(&opts.path);
    if diag.get("healthy").and_then(|v| v.as_bool()) == Some(false) {
        return Err(anyhow!(
            "environment not ready — run `fermut doctor` and fix the failing \
             checks, then re-run `fermut baseline`"
        ));
    }

    // Resolve the project base dir so the coverage DB path is absolute. The
    // `.coverage` filter path below MUST be absolute: build_config resolves a
    // relative path against the process cwd, not the project dir, so a bare
    // `.coverage` breaks whenever `baseline` is invoked from elsewhere (e.g.
    // `fermut baseline path/to/project` from a repo root).
    let loaded = crate::config::loader::LoadedConfig::load(&opts.path)
        .context("loading project config for baseline")?;
    let cov_db = loaded.base_dir.join(".coverage");

    // 2. Build/refresh `.coverage`, then read the line-coverage headline.
    crate::cli::subcmd::coverage::coverage(crate::cli::subcmd::coverage::CoverageOpts {
        path: opts.path.clone(),
        source: None,
        tests: None,
        full: false,
        output: Some(cov_db.clone()),
        pytest_args: Vec::new(),
        python: None,
    })
    .context("building coverage database for baseline")?;
    // `coverage` has no console script we rely on, so it's `<interp> -m
    // coverage`. Resolve the same interpreter the runner would (venv near the
    // source root, else a PATH-probed `python3`/`python`) so a `python3`-only
    // box doesn't silently drop the line-coverage headline.
    let interp = crate::runner::interpreter(
        crate::runner::resolve_python(&loaded.base_dir, None).as_deref(),
    );
    let line_coverage = read_line_coverage(&interp, &loaded.base_dir);

    // 3. Sampled mutation pass, restricted to covered lines. We reuse the
    //    full `build_config` path so the filter chain (coverage, excludes,
    //    op allow/deny) matches what `run` would do — only the sample knob
    //    differs.
    let sample = if opts.full {
        None
    } else {
        Some(opts.sample.unwrap_or(DEFAULT_SAMPLE))
    };
    let mut filter = opts.filter.clone();
    // Force the coverage filter on if the user didn't pick a path — the
    // whole point is "score on covered code".
    if filter.coverage.is_none() && !filter.no_coverage {
        filter.coverage = Some(cov_db.clone());
    }
    let cfg = build_config(
        opts.path.clone(),
        None,       // tests
        None,       // jobs
        None,       // timeout
        false,      // no_ty_filter
        false,      // ruff_filter
        false,      // tce
        None,       // hypothesis_seed
        Vec::new(), // pytest_args
        false,      // no_cache
        None,       // cache_path
        true,       // no_history — engine must NOT append; we write our
        // own entry below marked `baseline: true`, else the
        // anchor would be an ordinary, unmarked run entry.
        None,   // history_path
        sample, // sample
        Some(SAMPLE_SEED),
        None,  // shard
        None,  // runner
        None,  // python
        None,  // isolation
        false, // no_equiv_detect
        None,  // cache_scope
        None,  // fail_under
        false, // no_verify_baseline
        None,  // baseline_timeout
        false, // no_smart_order
        false, // smart_order
        None,  // max_time
        filter,
    )?;

    let started = Instant::now();
    let (report, _entry) = crate::engine::run(&cfg)?;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    // 4. Anchor the trend graph. Write one history entry flagged
    //    `baseline: true` so `trend`/`dashboard` can mark run zero. We append
    //    it ourselves (engine history is off above) so the flag survives.
    //    Best-effort: a read-only `.fermut/` shouldn't fail the report.
    {
        let mut entry = crate::history::HistoryEntry::from_report(
            &report,
            &cfg.source_root,
            Some(elapsed_ms),
            None,
        );
        entry.baseline = true;
        if let Err(e) = crate::history::append(&cfg.history_path, &entry) {
            tracing::warn!(error = %e, path = %cfg.history_path.display(), "baseline: could not write history anchor");
        }
    }

    let summary = report.summary();
    let worst_files = top_survivor_files(&report, opts.top);
    // A scoreless run (nothing killed/survived/timed out) carries the vacuous
    // 100.0 floor, not a measurement — grade it N/A rather than a perfect
    // "Strong", which would read as an A for a run that scored nothing.
    let grade = if report.is_scoreless() {
        Grade::Na
    } else {
        Grade::of(summary.mutation_score)
    };
    // The blind spot in covered code — never negative, same denominator as
    // the mutation score. (Line coverage is a separate axis below.)
    let quality_gap = round1(100.0 - summary.mutation_score);
    let untested_risk = line_coverage.map(|lc| round1(100.0 - lc));

    Ok(BaselineReport {
        line_coverage,
        mutation_score: summary.mutation_score,
        scored: summary.scored,
        quality_gap,
        untested_risk,
        grade,
        killed: summary.killed,
        survived: summary.survived,
        sampled: sample.is_some(),
        sample_fraction: sample.unwrap_or(1.0),
        worst_files,
    })
}

/// Read overall line coverage from coverage.py. `coverage report
/// --format=total` prints just the integer percent to stdout — cheaper and
/// more robust than re-parsing the SQLite numbits ourselves. Best-effort:
/// any failure degrades to `None` and the mutation half of the report still
/// stands.
fn read_line_coverage(interp: &std::path::Path, path: &std::path::Path) -> Option<f64> {
    let out = Command::new(interp)
        .arg("-m")
        .arg("coverage")
        .arg("report")
        .arg("--format=total")
        .current_dir(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

/// Survivors grouped by source file, worst first, truncated to `top`.
fn top_survivor_files(report: &Report, top: usize) -> Vec<FileSurvivors> {
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    for o in &report.outcomes {
        if let MutantOutcome::Survived { mutant } = o {
            *by_file
                .entry(mutant.file.display().to_string())
                .or_default() += 1;
        }
    }
    let mut v: Vec<FileSurvivors> = by_file
        .into_iter()
        .map(|(file, survivors)| FileSurvivors { file, survivors })
        .collect();
    // Most survivors first; file name as a stable tiebreak.
    v.sort_by(|a, b| b.survivors.cmp(&a.survivors).then(a.file.cmp(&b.file)));
    v.truncate(top);
    v
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn print_human(r: &BaselineReport) {
    println!("fermut baseline\n");
    match r.line_coverage {
        Some(lc) => println!("  line coverage     {lc:.0}%"),
        None => println!("  line coverage     —   (coverage.py report unavailable)"),
    }
    let est = if r.sampled {
        format!(
            "  (on covered code, {:.0}% sample)",
            r.sample_fraction * 100.0
        )
    } else {
        "  (on covered code)".to_string()
    };
    if matches!(r.grade, Grade::Na) {
        println!("  mutation score    N/A  (nothing scored — no mutants got a verdict)");
    } else {
        println!("  mutation score    {:.0}%{est}", r.mutation_score);
        println!("  ───────────────────────────────");
        println!(
            "  test-quality gap  {:.0} pts   ← covered code whose behavior no test checks",
            r.quality_gap
        );
        if let Some(risk) = r.untested_risk {
            println!("  untested risk     {risk:.0}%     ← lines no test executes at all");
        }
    }
    println!("\n  grade: {:?} — {}", r.grade, r.grade.blurb());

    if !r.worst_files.is_empty() {
        println!("\n  worst files (by survivors):");
        for f in &r.worst_files {
            println!("    {:>3}  {}", f.survivors, f.file);
        }
    }
    if r.sampled {
        println!("\n  note: sampled estimate — run `fermut baseline --full` for the exact score");
    }
    println!("\n  → write the highest-value test next:  fermut next");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_from_json(json: &str) -> Report {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn grade_bands_split_at_documented_thresholds() {
        assert!(matches!(Grade::of(80.0), Grade::Strong));
        assert!(matches!(Grade::of(95.0), Grade::Strong));
        assert!(matches!(Grade::of(79.9), Grade::Ok));
        assert!(matches!(Grade::of(60.0), Grade::Ok));
        assert!(matches!(Grade::of(59.9), Grade::Weak));
        assert!(matches!(Grade::of(40.0), Grade::Weak));
        assert!(matches!(Grade::of(39.9), Grade::Smoke));
        assert!(matches!(Grade::of(0.0), Grade::Smoke));
    }

    #[test]
    fn worst_files_ranks_by_survivor_count_then_name() {
        // b.py has 2 survivors, a.py and c.py one each. Killed mutants ignored.
        let report = report_from_json(
            r#"{"outcomes":[
                {"status":"survived","mutant":{"id":"b.py@1:boundary-shift:>=->>","file":"b.py","operator":"boundary-shift","range":[1,2],"original":">=","replacement":">","line":1}},
                {"status":"survived","mutant":{"id":"b.py@2:boundary-shift:<=-><","file":"b.py","operator":"boundary-shift","range":[2,3],"original":"<=","replacement":"<","line":2}},
                {"status":"survived","mutant":{"id":"a.py@1:boundary-shift:>=->>","file":"a.py","operator":"boundary-shift","range":[1,2],"original":">=","replacement":">","line":1}},
                {"status":"survived","mutant":{"id":"c.py@1:boundary-shift:>=->>","file":"c.py","operator":"boundary-shift","range":[1,2],"original":">=","replacement":">","line":1}},
                {"status":"killed","mutant":{"id":"a.py@9:arith-op-swap:+->-","file":"a.py","operator":"arith-op-swap","range":[9,10],"original":"+","replacement":"-","line":9}}
            ]}"#,
        );
        let worst = top_survivor_files(&report, 3);
        assert_eq!(worst.len(), 3);
        assert_eq!(worst[0].file, "b.py");
        assert_eq!(worst[0].survivors, 2);
        // a.py before c.py — equal counts, name tiebreak.
        assert_eq!(worst[1].file, "a.py");
        assert_eq!(worst[2].file, "c.py");
    }

    #[test]
    fn worst_files_truncates_to_top() {
        let report = report_from_json(
            r#"{"outcomes":[
                {"status":"survived","mutant":{"id":"a.py@1:x:>=->>","file":"a.py","operator":"boundary-shift","range":[1,2],"original":">=","replacement":">","line":1}},
                {"status":"survived","mutant":{"id":"b.py@1:x:>=->>","file":"b.py","operator":"boundary-shift","range":[1,2],"original":">=","replacement":">","line":1}}
            ]}"#,
        );
        assert_eq!(top_survivor_files(&report, 1).len(), 1);
    }

    #[test]
    fn worst_files_empty_when_no_survivors() {
        let report = report_from_json(
            r#"{"outcomes":[
                {"status":"killed","mutant":{"id":"a.py@9:arith-op-swap:+->-","file":"a.py","operator":"arith-op-swap","range":[9,10],"original":"+","replacement":"-","line":9}}
            ]}"#,
        );
        assert!(top_survivor_files(&report, 3).is_empty());
    }

    #[test]
    fn quality_gap_is_covered_code_complement_never_negative() {
        // The gap is 100 - mutation_score (covered-code denominator), so a
        // suite that out-scores its line coverage still yields a non-negative
        // gap — unlike the old line% - mutation% which went negative.
        let gap = round1(100.0 - 100.0);
        assert_eq!(gap, 0.0);
        let gap = round1(100.0 - 54.0);
        assert_eq!(gap, 46.0);
    }

    #[test]
    fn round1_rounds_to_one_decimal() {
        assert!((round1(28.04) - 28.0).abs() < f64::EPSILON);
        assert!((round1(28.05) - 28.1).abs() < f64::EPSILON);
    }
}
