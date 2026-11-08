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
use std::process::Command;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::cli::build_config::build_config;
use crate::cli::Format;
use crate::report::{MutantOutcome, Report};

/// Default fraction of mutants tested in the sampled pass. Tuned for a
/// minutes-not-hours first impression; `--full` overrides to 1.0.
const DEFAULT_SAMPLE: f64 = 0.1;
/// Fixed seed so two baselines on the same tree report the same number.
const SAMPLE_SEED: u64 = 0;

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

pub(crate) fn baseline(args: BaselineArgs) -> Result<()> {
    let format = args.format;
    let out = compute_baseline(args)?;
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&out)?),
        Format::Human => print_human(&out),
    }
    Ok(())
}

/// Run the baseline pipeline and return the structured report without
/// printing. Shared by the `baseline` subcommand and the MCP
/// `fermut_baseline` tool so both surface identical numbers.
pub(crate) fn compute_baseline(args: BaselineArgs) -> Result<BaselineReport> {
    // 1. Gate on environment health. A broken toolchain produces a
    //    confidently-wrong score, which is worse than no score on day one.
    let diag = crate::cli::subcmd::doctor::diagnose(&args.path);
    if diag.get("healthy").and_then(|v| v.as_bool()) == Some(false) {
        return Err(anyhow!(
            "environment not ready — run `fermut doctor` and fix the failing \
             checks, then re-run `fermut baseline`. More: {}",
            crate::TROUBLESHOOTING_URL
        ));
    }

    // Resolve the project base dir so the coverage DB path is absolute. The
    // `.coverage` filter path below MUST be absolute: build_config resolves a
    // relative path against the process cwd, not the project dir, so a bare
    // `.coverage` breaks whenever `baseline` is invoked from elsewhere (e.g.
    // `fermut baseline path/to/project` from a repo root).
    let loaded = crate::config::loader::LoadedConfig::load(&args.path)
        .context("loading project config for baseline")?;
    let cov_db = loaded.base_dir.join(".coverage");

    // 2. Build/refresh `.coverage`, then read the line-coverage headline.
    crate::cli::subcmd::coverage::coverage(crate::cli::subcmd::coverage::CoverageArgs {
        path: args.path.clone(),
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
    let sample = if args.full {
        None
    } else {
        Some(args.sample.unwrap_or(DEFAULT_SAMPLE))
    };
    let mut filter = args.filter.clone();
    // Force the coverage filter on if the user didn't pick a path — the
    // whole point is "score on covered code".
    if filter.coverage.is_none() && !filter.no_coverage {
        filter.coverage = Some(cov_db.clone());
    }
    let cfg = build_config(
        args.path.clone(),
        crate::cli::RunConfigArgs {
            // engine must NOT append history; we write our own entry below
            // marked `baseline: true`, else the anchor would be an ordinary,
            // unmarked run entry.
            no_history: true,
            sample,
            sample_seed: Some(SAMPLE_SEED),
            filter,
            ..Default::default()
        },
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
    let worst_files = top_survivor_files(&report, args.top);
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
        if let MutantOutcome::Survived { mutant, .. } = o {
            *by_file
                .entry(crate::util::display_path(&mutant.file).to_string())
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
    fn print_human_handles_graded_and_na_reports() {
        // Smoke-test both branches of the printer (graded with coverage +
        // sample note, and N/A without coverage) — neither may panic.
        print_human(&sample_report(Grade::Weak, true, Some(80.0)));
        print_human(&sample_report(Grade::Na, false, None));
    }

    #[test]
    fn report_json_omits_absent_line_coverage() {
        let v = serde_json::to_value(sample_report(Grade::Na, false, None)).unwrap();
        assert_eq!(v["grade"], "na");
        assert!(v.get("line_coverage").is_none());
        assert!(v.get("untested_risk").is_none());
        let v = serde_json::to_value(sample_report(Grade::Ok, true, Some(75.0))).unwrap();
        assert_eq!(v["line_coverage"], 75.0);
        assert_eq!(v["untested_risk"], 25.0);
    }

    #[cfg(unix)]
    fn fake_interp(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("py");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn read_line_coverage_parses_total_and_degrades_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = fake_interp(tmp.path(), "echo ' 87 '");
        assert_eq!(read_line_coverage(&ok, tmp.path()), Some(87.0));
        let bad = fake_interp(tmp.path(), "exit 1");
        assert_eq!(read_line_coverage(&bad, tmp.path()), None);
        let junk = fake_interp(tmp.path(), "echo nope");
        assert_eq!(read_line_coverage(&junk, tmp.path()), None);
        assert_eq!(
            read_line_coverage(&tmp.path().join("missing"), tmp.path()),
            None
        );
    }

    #[test]
    fn baseline_refuses_an_unhealthy_environment() {
        // A configured tests dir that doesn't exist fails `doctor`, so
        // baseline must bail before measuring anything.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join("fermut.toml"),
            "tests = \"missing_tests\"\n",
        )
        .unwrap();
        let err = baseline(BaselineArgs {
            path: tmp.path().to_path_buf(),
            full: false,
            sample: None,
            top: 3,
            format: Format::Json,
            filter: Default::default(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("environment not ready"), "{err}");
    }

    #[test]
    fn round1_rounds_to_one_decimal() {
        assert!((round1(28.04) - 28.0).abs() < f64::EPSILON);
        assert!((round1(28.05) - 28.1).abs() < f64::EPSILON);
    }

    #[test]
    fn every_grade_has_a_distinct_blurb() {
        let blurbs: std::collections::HashSet<_> = [
            Grade::Strong,
            Grade::Ok,
            Grade::Weak,
            Grade::Smoke,
            Grade::Na,
        ]
        .iter()
        .map(|g| g.blurb())
        .collect();
        assert_eq!(blurbs.len(), 5);
        assert!(Grade::Na.blurb().contains("nothing scored"));
    }

    fn sample_report(grade: Grade, sampled: bool, line_coverage: Option<f64>) -> BaselineReport {
        BaselineReport {
            line_coverage,
            mutation_score: 54.0,
            scored: 10,
            quality_gap: 46.0,
            untested_risk: line_coverage.map(|lc| round1(100.0 - lc)),
            grade,
            killed: 5,
            survived: 5,
            sampled,
            sample_fraction: if sampled { 0.1 } else { 1.0 },
            worst_files: vec![FileSurvivors {
                file: "a.py".into(),
                survivors: 5,
            }],
        }
    }

    #[test]
    fn print_human_renders_scored_and_na_variants() {
        // Wiring smoke over every branch of the human renderer: sampled with
        // coverage + worst files, full without coverage, and the N/A grade.
        print_human(&sample_report(Grade::Weak, true, Some(90.0)));
        print_human(&sample_report(Grade::Strong, false, None));
        let mut na = sample_report(Grade::Na, false, None);
        na.worst_files.clear();
        print_human(&na);
        // JSON omits the absent coverage axes rather than emitting null.
        let v = serde_json::to_value(sample_report(Grade::Na, false, None)).unwrap();
        assert_eq!(v["grade"], "na");
        assert!(v.get("line_coverage").is_none());
        assert!(v.get("untested_risk").is_none());
    }

    #[test]
    fn compute_baseline_refuses_an_unhealthy_environment() {
        // A configured tests dir that doesn't exist is a doctor `fail`, so
        // baseline must stop before building coverage or running mutants.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n[tool.fermut]\ntests = \"no_such_tests\"\n",
        )
        .unwrap();
        let err = compute_baseline(BaselineArgs {
            path: tmp.path().to_path_buf(),
            full: false,
            sample: None,
            top: 3,
            format: Format::Json,
            filter: Default::default(),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("environment not ready"), "got: {err}");
        assert!(!tmp.path().join(".coverage").exists());
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct BaselineArgs {
    /// Where to start the project-root walk. Defaults to cwd.
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    /// Mutate every covered mutant for the exact score instead of a
    /// sampled estimate. Slower; use once the sampled run looks right.
    #[arg(long)]
    pub(crate) full: bool,

    /// Sampling fraction (0.0–1.0) for the fast pass. Defaults to 0.1.
    /// Ignored with `--full`.
    #[arg(long, value_name = "RATIO")]
    pub(crate) sample: Option<f64>,

    /// How many worst-offender files to list. Defaults to 3.
    #[arg(long, default_value_t = 3, value_name = "N")]
    pub(crate) top: usize,

    /// Output format. `human` (default) prints the graded verdict;
    /// `json` emits the same numbers for an agent / dashboard.
    #[arg(long, value_enum, default_value_t = crate::cli::Format::Human)]
    pub(crate) format: crate::cli::Format,

    #[command(flatten)]
    pub(crate) filter: crate::cli::FilterArgs,
}
