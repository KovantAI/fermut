//! Per-run history log (`.fermut/history.jsonl`).
//!
//! Each `fermut run` appends one JSON line summarising the run: timestamp,
//! mutation score, status counts, the fermut version, and (when discoverable)
//! the git sha/branch. The file is JSON-lines so appends are cheap, partial reads
//! survive truncation, and the schema can extend without breaking older
//! readers (downstream just ignores unknown fields).
//!
//! `fermut trend` reads this file. The cache and history are intentionally
//! separate files: cache rotation (`fermut clean`) shouldn't lose history.

mod git;
mod render;
mod schema;
mod trend;

pub use render::{sparkline, sparkline_scaled};
#[cfg(test)]
use schema::format_iso8601_utc;
pub use schema::{
    append, config_hash, default_history_path, load, load_with_stats, mixed_config_hashes,
    resolve_root, HistoryEntry, LoadStats, CURRENT_SCHEMA_V,
};
#[cfg(test)]
use trend::parse_id_file;
pub(crate) use trend::trend_step;
pub use trend::{
    branch_scoped_regression, regression_against, survivor_age_map, survivor_diff,
    survivors_by_file, trailing_streak, StreakDir,
};

// Test-only names the (verbatim-moved) tests reference bare via `use super::*`.
#[cfg(test)]
use crate::config::Config;
#[cfg(test)]
use crate::report::{MutantOutcome, Report};
#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        assert_ne!(base, ruff, "ruff-filtered run must differ");
        assert_ne!(base, ty, "ty-filtered run must differ");
        assert_ne!(ruff, ty, "ruff vs ty must differ");
        assert_ne!(base, equiv, "equiv-detect toggle must differ");

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

    #[test]
    fn from_report_stamps_current_fermut_version() {
        use crate::mutator::{Mutant, Operator};
        use ruff_text_size::TextRange;
        let m = Mutant {
            id: "x".into(),
            file: PathBuf::from("a.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        };
        let report = Report::new(vec![MutantOutcome::killed(m)]);
        let e = HistoryEntry::from_report(&report, Path::new("."), None, None);
        assert_eq!(e.fermut_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    fn mutant(id: &str) -> crate::mutator::Mutant {
        use crate::mutator::{Mutant, Operator};
        use ruff_text_size::TextRange;
        Mutant {
            id: id.into(),
            file: PathBuf::from("a.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn from_report_marks_time_budget_run_partial() {
        // A run with any time-budget skip scored a truncated subset → partial,
        // so the regression gate/trend excludes it.
        let report = Report::new(vec![
            MutantOutcome::killed(mutant("k")),
            MutantOutcome::skipped(mutant("s"), crate::engine::TIME_BUDGET_FILTER),
        ]);
        let e = HistoryEntry::from_report(&report, Path::new("."), None, None);
        assert!(e.partial, "time-budget skip must flag the entry partial");
        assert!(!e.is_comparable());
    }

    #[test]
    fn from_report_full_run_is_not_partial() {
        // No time-budget skip (a plain coverage skip is not a budget cutoff) →
        // a full, comparable run.
        let report = Report::new(vec![
            MutantOutcome::killed(mutant("k")),
            MutantOutcome::skipped(mutant("s"), "coverage"),
        ]);
        let e = HistoryEntry::from_report(&report, Path::new("."), None, None);
        assert!(!e.partial, "a non-budget skip must not flag partial");
        assert!(e.is_comparable());
    }

    #[test]
    fn append_then_load_roundtrips_entries() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let entry = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 87.5,
            killed: 7,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(8),
            duration_ms: Some(1234),
            config_hash: Some("deadbeef".into()),
            fermut_version: Some("9.9.9".into()),
            git_sha: Some("abc1234".into()),
            git_branch: Some("main".into()),
            survivor_ids: None,
            baseline: false,
            partial: false,
        };
        append(&p, &entry).unwrap();
        append(&p, &entry).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!((loaded[0].mutation_score - 87.5).abs() < f64::EPSILON);
        assert_eq!(loaded[0].killed, 7);
        assert_eq!(loaded[0].git_sha.as_deref(), Some("abc1234"));
    }

    #[test]
    fn baseline_flag_round_trips_and_is_omitted_when_false() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let mut anchor = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 54.0,
            killed: 5,
            survived: 4,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(9),
            duration_ms: None,
            config_hash: None,
            fermut_version: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
        };
        // A false flag is skipped on serialize — no schema bloat on run rows.
        let normal_json = serde_json::to_string(&anchor).unwrap();
        assert!(!normal_json.contains("baseline"), "got: {normal_json}");

        // A true flag is written and survives a load round-trip.
        anchor.baseline = true;
        append(&p, &anchor).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].baseline, "baseline flag lost on round-trip");

        // A pre-field entry (no `baseline` key) defaults to false.
        let legacy: HistoryEntry = serde_json::from_str(
            r#"{"v":1,"timestamp":"t","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}"#,
        )
        .unwrap();
        assert!(!legacy.baseline);
    }

    #[test]
    fn load_skips_malformed_lines() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let valid = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 50.0,
            killed: 1,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(2),
            duration_ms: None,
            config_hash: None,
            fermut_version: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
        };
        let line = serde_json::to_string(&valid).unwrap();
        std::fs::write(&p, format!("not json\n{line}\n{{partial:")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].killed, 1);
    }

    #[test]
    fn load_with_stats_counts_malformed_and_newer_schema() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let valid = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: 50.0,
            killed: 1,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: Some(2),
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
            fermut_version: None,
        };
        let good = serde_json::to_string(&valid).unwrap();
        // A well-formed entry stamped one schema version ahead of this binary.
        let newer = format!(
            r#"{{"v":{},"timestamp":"t","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}}"#,
            CURRENT_SCHEMA_V + 1
        );
        // valid, malformed, newer-schema, blank line (ignored, not counted).
        std::fs::write(&p, format!("{good}\nnot json\n{newer}\n\n")).unwrap();

        let (entries, stats) = load_with_stats(&p).unwrap();
        assert_eq!(entries.len(), 1, "only the one in-range entry loads");
        assert_eq!(stats.loaded, 1);
        assert_eq!(stats.malformed, 1);
        assert_eq!(stats.newer_schema, 1);
        assert_eq!(stats.dropped(), 2);

        // Missing file → all zeros, no error.
        let (empty, s0) = load_with_stats(&tmp.path().join("nope.jsonl")).unwrap();
        assert!(empty.is_empty());
        assert_eq!(s0.dropped(), 0);
    }

    #[test]
    fn concurrent_appends_dont_corrupt_lines() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let writers = 8usize;
        let per_writer = 25usize;
        let mut handles = Vec::new();
        for tid in 0..writers {
            let p = p.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..per_writer {
                    let entry = HistoryEntry {
                        schema_version: CURRENT_SCHEMA_V,
                        timestamp: format!("2026-06-04T12:{tid:02}:{i:02}Z"),
                        mutation_score: tid as f64,
                        killed: i,
                        survived: 0,
                        timed_out: 0,
                        skipped: 0,
                        errored: 0,
                        equivalent: 0,
                        total: None,
                        duration_ms: None,
                        config_hash: None,
                        fermut_version: None,
                        git_sha: None,
                        git_branch: None,
                        survivor_ids: None,
                        baseline: false,
                        partial: false,
                    };
                    append(&p, &entry).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), writers * per_writer);
    }

    #[test]
    fn load_skips_entries_with_future_schema_version() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let future_v = CURRENT_SCHEMA_V + 5;
        let line = format!(
            r#"{{"v":{future_v},"timestamp":"2026-06-04T12:00:00Z","mutation_score":80.0,"killed":4,"survived":1,"timed_out":0,"skipped":0,"errored":0}}"#
        );
        let valid = HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:01:00Z".into(),
            mutation_score: 90.0,
            killed: 9,
            survived: 1,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: None,
            duration_ms: None,
            config_hash: None,
            fermut_version: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: None,
            baseline: false,
            partial: false,
        };
        let valid_line = serde_json::to_string(&valid).unwrap();
        std::fs::write(&p, format!("{line}\n{valid_line}\n")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!((loaded[0].mutation_score - 90.0).abs() < f64::EPSILON);
    }

    #[test]
    fn load_accepts_pretag_entries_as_v0() {
        // Entries written before the `v` field existed have no version tag.
        // Serde's `default` makes them deserialize as `schema_version: 0`,
        // which is <= CURRENT_SCHEMA_V so they're kept.
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("history.jsonl");
        let line = r#"{"timestamp":"2026-06-04T12:00:00Z","mutation_score":75.0,"killed":3,"survived":1,"timed_out":0,"skipped":0,"errored":0}"#;
        std::fs::write(&p, format!("{line}\n")).unwrap();
        let loaded = load(&p).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].schema_version, 0);
    }

    #[test]
    fn load_returns_empty_when_missing() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("nope.jsonl");
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn iso8601_known_epoch_values() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso8601_utc(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(format_iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn default_history_path_uses_dot_fermut() {
        let p = default_history_path(Path::new("/proj"));
        assert!(p.ends_with(".fermut/history.jsonl"));
    }

    #[test]
    fn resolve_root_agrees_for_run_subpath_and_bare_trend() {
        // The regression: `fermut run src/` and a bare `fermut trend` (path
        // `.`) must resolve to the same project root so trend reads what run
        // wrote. Anchor is the pyproject.toml marker at the project root.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();

        let from_run = resolve_root(&src);
        let from_trend = resolve_root(&root);
        assert_eq!(
            from_run, from_trend,
            "run-on-src and trend-on-root must land on the same project root"
        );
        assert_eq!(from_run, root);
    }

    #[test]
    fn resolve_root_falls_back_to_existing_dot_fermut() {
        // No project marker, but a prior run left a `.fermut/` at the root.
        // A subpath lookup must climb to it instead of anchoring on the
        // subdir, so trend finds the existing history.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".fermut")).unwrap();
        let src = root.join("pkg");
        std::fs::create_dir_all(&src).unwrap();

        assert_eq!(resolve_root(&src), root);
    }

    #[test]
    fn resolve_root_file_target_uses_parent() {
        // No marker, no prior `.fermut/`: a single-file target resolves to its
        // parent directory, never to `file.py/.fermut`.
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let file = root.join("mod.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        assert_eq!(resolve_root(&file), root);
    }

    #[test]
    fn sparkline_renders_8_buckets() {
        let s = sparkline([0.0, 12.5, 25.0, 50.0, 75.0, 100.0]);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.first(), Some(&'▁'));
        assert_eq!(chars.last(), Some(&'█'));
        assert_eq!(chars.len(), 6);
    }

    #[test]
    fn sparkline_handles_empty_input() {
        assert!(sparkline(std::iter::empty::<f64>()).is_empty());
    }

    #[test]
    fn sparkline_clamps_out_of_range_values() {
        let s = sparkline([-50.0, 200.0]);
        assert_eq!(s.chars().collect::<Vec<_>>(), vec!['▁', '█']);
    }

    #[test]
    fn sparkline_scaled_fills_height_for_narrow_window() {
        // Fixed-scale [0,100] crushes 85→95 into the same bucket; scaled
        // to its own range it spans floor→top.
        let s = sparkline_scaled([85.0, 95.0], 85.0, 95.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.first(), Some(&'▁'));
        assert_eq!(chars.last(), Some(&'█'));
    }

    #[test]
    fn sparkline_scaled_clamps_to_explicit_bounds() {
        let s = sparkline_scaled([50.0, 200.0, -10.0], 80.0, 90.0);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars, vec!['▁', '█', '▁']);
    }

    #[test]
    fn sparkline_scaled_collapses_zero_span_safely() {
        let s = sparkline_scaled([42.0, 42.0, 42.0], 42.0, 42.0);
        assert_eq!(s.chars().collect::<Vec<_>>(), vec!['▁', '▁', '▁']);
    }

    fn mk(score: f64, branch: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: score,
            // Non-zero so the entry is a real scored run, not a scoreless
            // vacuous-100 that trend/regression now filter out.
            killed: 1,
            survived: 0,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: None,
            duration_ms: None,
            config_hash: None,
            fermut_version: None,
            git_sha: None,
            git_branch: branch.map(str::to_string),
            survivor_ids: None,
            baseline: false,
            partial: false,
        }
    }

    #[test]
    fn branch_scoped_regression_skips_other_branches() {
        let es = vec![
            mk(90.0, Some("main")),
            mk(50.0, Some("feature/x")), // big drop on feature, ignored
            mk(88.0, Some("main")),      // 2pt drop on main → reported
        ];
        assert_eq!(branch_scoped_regression(&es), Some(2.0));
    }

    #[test]
    fn branch_scoped_regression_none_when_improving() {
        let es = vec![mk(80.0, Some("main")), mk(85.0, Some("main"))];
        assert_eq!(branch_scoped_regression(&es), None);
    }

    #[test]
    fn branch_scoped_regression_none_when_no_prior_on_branch() {
        let es = vec![mk(50.0, Some("other")), mk(90.0, Some("main"))];
        // No prior 'main' entry; falls back to the only prior (other) and
        // 50→90 is an improvement, not a regression.
        assert_eq!(branch_scoped_regression(&es), None);
    }

    fn mk_with_survivors(score: f64, ids: Option<&[&str]>) -> HistoryEntry {
        let mut e = mk(score, None);
        e.survivor_ids = ids.map(|s| s.iter().map(|x| x.to_string()).collect());
        e
    }

    #[test]
    fn survivor_diff_finds_new_and_killed() {
        let prev = mk_with_survivors(90.0, Some(&["a", "b", "c"]));
        let curr = mk_with_survivors(85.0, Some(&["b", "c", "d", "e"]));
        let (new_surv, new_killed) = survivor_diff(&prev, &curr).unwrap();
        assert_eq!(new_surv, vec!["d", "e"]);
        assert_eq!(new_killed, vec!["a"]);
    }

    #[test]
    fn survivor_diff_returns_none_when_either_lacks_data() {
        let with = mk_with_survivors(90.0, Some(&["a"]));
        let without = mk_with_survivors(85.0, None);
        assert!(survivor_diff(&with, &without).is_none());
        assert!(survivor_diff(&without, &with).is_none());
    }

    #[test]
    fn survivor_diff_handles_empty_sets() {
        let a = mk_with_survivors(100.0, Some(&[]));
        let b = mk_with_survivors(100.0, Some(&[]));
        let (n, k) = survivor_diff(&a, &b).unwrap();
        assert!(n.is_empty() && k.is_empty());
    }

    #[test]
    fn branch_scoped_regression_best_effort_when_branch_missing() {
        let es = vec![mk(90.0, None), mk(70.0, Some("main"))];
        assert_eq!(branch_scoped_regression(&es), Some(20.0));
    }

    #[test]
    fn regression_against_compares_in_memory_current() {
        // Simulates an append-failed run: history file contains only the
        // prior entries, current is in memory. Gate must compare current
        // against the matching-branch prior, not against the prior's prior.
        let prior = vec![
            mk(90.0, Some("main")), // older
            mk(85.0, Some("main")), // latest matching-branch prior
        ];
        let current = mk(75.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(10.0));
    }

    #[test]
    fn regression_against_returns_none_when_improving() {
        let prior = vec![mk(80.0, Some("main"))];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_other_branch_priors() {
        let prior = vec![
            mk(90.0, Some("main")),      // last matching-branch prior
            mk(50.0, Some("feature/x")), // ignored
        ];
        let current = mk(88.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(2.0));
    }

    #[test]
    fn regression_against_returns_none_when_prior_empty() {
        let current = mk(80.0, Some("main"));
        assert_eq!(regression_against(&[], &current), None);
    }

    /// A scored `mk` entry forced back to zero buckets — its `mutation_score`
    /// is the vacuous 100.0 floor, so `is_scoreless()` is true.
    fn mk_scoreless(branch: Option<&str>) -> HistoryEntry {
        let mut e = mk(100.0, branch);
        e.killed = 0;
        assert!(e.is_scoreless());
        e
    }

    #[test]
    fn regression_against_ignores_scoreless_current() {
        // Current run scored nothing (all errored/skipped) → vacuous 100.0.
        // Comparing a real 90% prior against it would fabricate a -10pt drop.
        let prior = vec![mk(90.0, Some("main"))];
        let current = mk_scoreless(Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_scoreless_prior() {
        // The most recent prior scored nothing (vacuous 100.0). It must be
        // skipped so the gate compares against the last real score (90→85).
        let prior = vec![
            mk(90.0, Some("main")),     // last real prior
            mk_scoreless(Some("main")), // vacuous 100.0, must be skipped
        ];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(5.0));
    }

    #[test]
    fn trailing_streak_ignores_scoreless_entries() {
        // A vacuous-100 run sitting between real scores must not invent a move.
        let es = vec![
            mk(50.0, None),
            mk(60.0, None),
            mk_scoreless(None), // vacuous 100.0 — dropped, not a +40 spike
            mk(70.0, None),
        ];
        assert_eq!(trailing_streak(&es), Some((StreakDir::Up, 2)));
    }

    #[test]
    fn trailing_streak_ignores_partial_entries() {
        // A `--max-time` partial run's subset score must not invent or break a
        // streak. 50→60→70 is a clean 2-run up streak; a partial 20 wedged in
        // would otherwise read as a down-then-up whipsaw.
        let es = vec![
            mk(50.0, None),
            mk(60.0, None),
            mk_partial(20.0, None), // subset score — dropped, not a −40 move
            mk(70.0, None),
        ];
        assert_eq!(trailing_streak(&es), Some((StreakDir::Up, 2)));
    }

    /// A real scored entry flagged `partial` — as a `--max-time` run truncated
    /// by the budget would be. Scored (not scoreless), but not comparable.
    fn mk_partial(score: f64, branch: Option<&str>) -> HistoryEntry {
        let mut e = mk(score, branch);
        e.partial = true;
        assert!(
            !e.is_scoreless(),
            "partial entry still scored a real subset"
        );
        assert!(
            !e.is_comparable(),
            "partial entry must be excluded from the gate"
        );
        e
    }

    #[test]
    fn regression_against_ignores_partial_current() {
        // Current run was --max-time-truncated → scored a nondeterministic
        // subset. Comparing a real 90% prior against it would fabricate a drop.
        let prior = vec![mk(90.0, Some("main"))];
        let current = mk_partial(70.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_partial_prior() {
        // The most recent prior was a partial (budget-truncated) run. It must
        // be skipped so the gate compares against the last full run (90→85),
        // never poisoning the baseline with a subset score.
        let prior = vec![
            mk(90.0, Some("main")),         // last full prior
            mk_partial(50.0, Some("main")), // budget-truncated, must be skipped
        ];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(5.0));
    }

    #[test]
    fn trend_step_deltas_and_seeds_only_comparable_runs() {
        // full(80) → partial(40) → full(78): the partial neither shows a delta
        // nor advances the baseline, so the third row deltas 78−80 = −2.
        let a = mk(80.0, None);
        let b = mk_partial(40.0, None);
        let c = mk(78.0, None);

        let (d_a, prev) = trend_step(None, &a);
        assert_eq!(d_a, None, "first comparable row has no prior");
        assert_eq!(prev, Some(80.0));

        let (d_b, prev) = trend_step(prev, &b);
        assert_eq!(d_b, None, "partial row shows no delta");
        assert_eq!(
            prev,
            Some(80.0),
            "partial row does NOT advance the baseline"
        );

        let (d_c, prev) = trend_step(prev, &c);
        assert_eq!(
            d_c,
            Some(-2.0),
            "deltas against the last full run, not the subset"
        );
        assert_eq!(prev, Some(78.0));
    }

    #[test]
    fn trend_step_skips_scoreless_like_partial() {
        // A scoreless (vacuous-100) row behaves the same: no delta, no seed.
        let a = mk(90.0, None);
        let s = mk_scoreless(None);
        let (_, prev) = trend_step(None, &a);
        let (d_s, prev_after) = trend_step(prev, &s);
        assert_eq!(d_s, None);
        assert_eq!(
            prev_after,
            Some(90.0),
            "scoreless must not reseed the baseline"
        );
    }

    #[test]
    fn parse_id_file_extracts_prefix() {
        assert_eq!(parse_id_file("src/foo.py@42:a->b"), Some("src/foo.py"));
        assert_eq!(
            parse_id_file("pkg/sub/mod.py@1234:return->pass"),
            Some("pkg/sub/mod.py")
        );
    }

    #[test]
    fn parse_id_file_handles_at_in_path() {
        // POSIX paths can technically contain `@`. The rightmost valid
        // `@<digits>:` boundary wins.
        assert_eq!(
            parse_id_file("weird@dir/m.py@7:x->y"),
            Some("weird@dir/m.py")
        );
    }

    #[test]
    fn parse_id_file_rejects_unrecognised_shape() {
        assert_eq!(parse_id_file("no-at-here"), None);
        assert_eq!(parse_id_file("file.py@notdigits:foo"), None);
        assert_eq!(parse_id_file("file.py@42-missing-colon"), None);
    }

    #[test]
    fn survivors_by_file_groups_and_sorts_by_count() {
        let ids = vec![
            "src/a.py@1:x->y".to_string(),
            "src/b.py@2:x->y".to_string(),
            "src/a.py@3:x->y".to_string(),
            "src/a.py@4:x->y".to_string(),
            "garbage".to_string(),
        ];
        let groups = survivors_by_file(&ids);
        // a.py (3) before b.py (1) before <unknown> (1, alphabetically later).
        assert_eq!(groups[0].0, "src/a.py");
        assert_eq!(groups[0].1.len(), 3);
        assert_eq!(groups[1].0, "<unknown>");
        assert_eq!(groups[2].0, "src/b.py");
    }

    #[test]
    fn survivors_by_file_empty_input_empty_output() {
        assert!(survivors_by_file(&[]).is_empty());
    }

    fn mk_surv(ids: &[&str]) -> HistoryEntry {
        let mut e = mk(0.0, None);
        e.survivor_ids = Some(ids.iter().map(|s| s.to_string()).collect());
        e
    }

    #[test]
    fn survivor_age_counts_consecutive_appearances() {
        let entries = vec![
            mk_surv(&["a", "b"]), // age would be 3 for a, but b was killed in r2
            mk_surv(&["a"]),      // b not here
            mk_surv(&["a", "c"]), // newest
        ];
        let ages = survivor_age_map(&entries);
        assert_eq!(ages.get("a").copied(), Some(3));
        assert_eq!(ages.get("c").copied(), Some(1));
        // b is not in the latest run — not tracked.
        assert!(!ages.contains_key("b"));
    }

    #[test]
    fn survivor_age_streak_breaks_on_missing_data() {
        let entries = vec![
            mk_surv(&["a"]),
            {
                let mut e = mk(0.0, None);
                e.survivor_ids = None; // older entry, pre-field
                e
            },
            mk_surv(&["a"]),
        ];
        let ages = survivor_age_map(&entries);
        // Latest run has "a" → age starts at 1. Walking back, we hit a
        // missing-ids entry; can't claim "a" survived there, streak stops.
        assert_eq!(ages.get("a").copied(), Some(1));
    }

    #[test]
    fn survivor_age_empty_inputs_safe() {
        assert!(survivor_age_map(&[]).is_empty());
        let no_ids = mk(0.0, None);
        assert!(survivor_age_map(&[no_ids]).is_empty());
        let empty = mk_surv(&[]);
        assert!(survivor_age_map(&[empty]).is_empty());
    }

    #[test]
    fn survivor_age_single_entry_gives_age_one() {
        let entries = vec![mk_surv(&["x", "y"])];
        let ages = survivor_age_map(&entries);
        assert_eq!(ages.get("x").copied(), Some(1));
        assert_eq!(ages.get("y").copied(), Some(1));
    }
}
