//! End-to-end smoke tests.
//!
//! These run the `fermut` binary against `examples/sample` (flat layout) and
//! `examples/monorepo_sample` (uv workspace with multiple member packages) to
//! confirm fermut walks both shapes. The flat-layout `run` test asserts that
//! the weak boundary test (`in_range`) leaves at least one survivor.

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;

fn sample_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("examples/sample");
    p
}

fn monorepo_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("examples/monorepo_sample");
    p
}

/// Recursively copy `src` into `dst` (both dirs). Used to run write-heavy tests
/// against a throwaway copy of a shared example tree, so learned artifacts
/// (`.coverage`, `.fermut/`) never land in the checked-in sample or race a
/// sibling test that reads the same tree.
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
fn list_emits_mutations() {
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("list")
        .arg(sample_path().join("src"))
        .assert()
        .success()
        .stdout(contains("arith-op-swap"))
        .stdout(contains("boundary-shift"));
}

#[test]
fn list_emits_mutations_monorepo() {
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("list")
        .arg(monorepo_path().join("packages"))
        .assert()
        .success()
        .stdout(contains("arith-op-swap"))
        .stdout(contains("boundary-shift"))
        .stdout(contains("pkg_a"))
        .stdout(contains("pkg_b"));
}

#[test]
#[ignore = "requires pytest + ty on PATH; enable once env is set up"]
fn run_finds_known_survivor() {
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("run")
        .arg(sample_path().join("src"))
        .arg("--tests")
        .arg(sample_path().join("tests"))
        .arg("--no-ty-filter")
        .assert()
        .failure() // survivors → exit 1
        .stdout(contains("SURVIVED"));
}

#[test]
#[ignore = "requires pytest + pytest-cov + coverage on PATH; enable once env is set up"]
fn smart_order_builds_sidecar_and_keeps_verdict() {
    // Two runs: the first learns which tests kill mutants and writes
    // `.fermut/kill-order.json`; the second reads it to order tests. The
    // mutation result must be identical across both — ordering only changes
    // *which test runs first*, never the kill/survive verdict.
    //
    // Smart ordering only ever reorders *coverage-selected* tests, so the
    // feature is a no-op without per-test coverage: with the whole-tests-dir
    // sweep there is nothing to reorder and the sidecar is never written.
    // So we generate a per-test `.coverage` first and feed it to `run`. The
    // sample's `test_add`/`test_add_commutes` pair covers `add`'s mutated line
    // twice — the >1-test case the capture path needs.
    //
    // This test WRITES learned artifacts (`.coverage`, `.fermut/kill-order.json`)
    // beside the project, so run it against a throwaway copy of the sample rather
    // than the checked-in tree — otherwise those files linger as untracked and
    // could race a sibling e2e reading the same sample under `cargo test`'s
    // parallelism. The tempdir (and everything under it) is removed on drop, on
    // the happy path and on any panic below.
    let tmp = tempfile::tempdir().unwrap();
    let sample = tmp.path().join("sample");
    copy_dir(&sample_path(), &sample);

    let src = sample.join("src");
    let tests = sample.join("tests");
    // Kill-order sidecar lands at the project root's `.fermut/`.
    let sidecar = sample.join(".fermut").join("kill-order.json");
    let coverage = sample.join(".coverage");

    // Build the per-test coverage database (pytest-cov contexts).
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("coverage")
        .arg(&sample)
        .assert()
        .success();
    assert!(
        coverage.exists(),
        "coverage step must write the .coverage database at {}",
        coverage.display()
    );

    let run = || {
        Command::cargo_bin("fermut")
            .unwrap()
            .arg("run")
            .arg(&src)
            .arg("--tests")
            .arg(&tests)
            .arg("--coverage")
            .arg(&coverage)
            .arg("--no-ty-filter")
            .arg("--no-cache") // isolate ordering from cache reuse
            // Smart ordering is on by default (timeout or not); pass the flag
            // explicitly to pin the behavior this test exercises even if the
            // default ever changes.
            .arg("--smart-order")
            .assert()
            .failure() // survivors → exit 1
            .stdout(contains("SURVIVED"));
    };

    run(); // builds the sidecar from learned kills
    assert!(
        sidecar.exists(),
        "first run must write the kill-order sidecar at {}",
        sidecar.display()
    );

    // The verdict-invariance check above passes even if ordering is a silent
    // no-op: a killer node id that pytest's `-rfE` summary reports differently
    // from the coverage-selected id would never match in `order()`, so the
    // sidecar would grow but never reorder — and the score wouldn't budge. Guard
    // that link directly: the recorded killer must be a real, `::`-formed test
    // id whose file resolves under the sample, i.e. exactly the shape a
    // coverage-selected id has and `order()` can lift. If pytest ever normalizes
    // ids differently from what we pass as positionals, this assertion trips.
    //
    // Sidecar shape: `{ file : { operator : { nodeid : count } } }`.
    let raw = std::fs::read_to_string(&sidecar).expect("sidecar readable");
    let ko: serde_json::Value = serde_json::from_str(&raw).expect("sidecar is valid JSON");
    let mut recorded = 0usize;
    for (_file, ops) in ko.as_object().expect("top level is a map") {
        for (_op, nodeids) in ops.as_object().expect("operator level is a map") {
            for (nodeid, count) in nodeids.as_object().expect("nodeid level is a map") {
                recorded += 1;
                assert!(
                    count.as_u64().is_some_and(|c| c >= 1),
                    "kill count for {nodeid:?} must be >= 1, got {count}"
                );
                // A well-formed pytest nodeid is `path::test`. A bare path (a
                // collection error) or a whitespace-truncated id would fail here.
                let (path_part, func_part) = nodeid.split_once("::").unwrap_or_else(|| {
                    panic!("recorded killer {nodeid:?} is not a `path::test` node id")
                });
                assert!(
                    !func_part.is_empty(),
                    "recorded killer {nodeid:?} has an empty test part"
                );
                // The path the id resolves to must exist under the sample tree.
                // A normalized-away id (wrong prefix, absolute rewrite) wouldn't.
                assert!(
                    sample.join(path_part).exists(),
                    "recorded killer {nodeid:?} resolves to {path_part:?}, which does not \
                     exist under the sample — pytest reported an id we can't match",
                );
            }
        }
    }
    assert!(
        recorded >= 1,
        "smart ordering must have learned at least one killer into the sidecar"
    );

    run(); // reads the sidecar to order; same verdict

    // Artifacts live under `tmp`, removed when the tempdir drops.
}

#[test]
#[ignore = "requires rstest (pytest-compatible drop-in) + ty on PATH; enable once env is set up"]
fn run_finds_known_survivor_with_rstest() {
    // `rstest` shares the pytest runner, so the same weak `in_range` boundary
    // test must leave a survivor — proving the `--runner rstest` path drives
    // the sample suite identically to `--runner pytest`.
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("run")
        .arg(sample_path().join("src"))
        .arg("--tests")
        .arg(sample_path().join("tests"))
        .arg("--runner")
        .arg("rstest")
        .arg("--no-ty-filter")
        .assert()
        .failure() // survivors → exit 1
        .stdout(contains("SURVIVED"));
}

#[test]
#[ignore = "requires pytest + coverage on PATH; enable once env is set up"]
fn baseline_reports_grade_and_anchors_trend() {
    let sample = sample_path();

    // `baseline` builds coverage, runs a sampled mutation pass over covered
    // code, and emits the graded verdict. `--full` for determinism (no
    // sampling), JSON so we can assert on field names. The sample's weak
    // `in_range` boundary test guarantees survivors, so the score is well
    // under 100% — enough to exercise the gap + grade path.
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("baseline")
        .arg(&sample)
        .arg("--full")
        .arg("--format")
        .arg("json")
        .assert()
        .success() // baseline reports; it does not gate, so exit 0
        .stdout(contains("\"mutation_score\""))
        .stdout(contains("\"grade\""))
        .stdout(contains("\"worst_files\""));

    // The run wrote a history entry flagged `baseline: true`; `trend` marks
    // it as the anchor in its table.
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("trend")
        .arg(&sample)
        .assert()
        .success()
        .stdout(contains("[baseline]"));
}

#[test]
fn next_rejects_all_with_limit() {
    // `--all` and `--limit` are mutually exclusive; clap rejects the combo
    // before any report is read.
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("next")
        .arg("nonexistent.json")
        .arg("--all")
        .arg("--limit")
        .arg("3")
        .assert()
        .failure()
        .stderr(contains("cannot be used with"));
}

#[test]
fn next_rejects_zero_limit() {
    // `--limit 0` is meaningless ("the zero best targets"); rejected before
    // the report is read, so a missing path is fine.
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("next")
        .arg("nonexistent.json")
        .arg("--limit")
        .arg("0")
        .assert()
        .failure()
        .stderr(contains("--limit must be at least 1"));
}

#[test]
fn next_rejects_max_tokens_with_human_format() {
    // `--max-tokens` budgets the JSON output; pairing it with `--format human`
    // is rejected before the report is read.
    Command::cargo_bin("fermut")
        .unwrap()
        .arg("next")
        .arg("nonexistent.json")
        .arg("--max-tokens")
        .arg("500")
        .arg("--format")
        .arg("human")
        .assert()
        .failure()
        .stderr(contains("--max-tokens applies to JSON output only"));
}

/// Minimal report: one killed mutant (for the score denominator) plus two
/// boundary-shift survivors on adjacent lines in the same file — one cluster
/// of size 2. No coverage-skip outcome, so `coverage_selected` is false.
const NEXT_REPORT: &str = r#"{
  "outcomes": [
    { "status": "killed",
      "mutant": { "id": "a.py@10:boundary-shift:k", "file": "a.py",
        "operator": "boundary-shift", "range": [10, 12],
        "original": ">=", "replacement": ">", "line": 5 } },
    { "status": "survived",
      "mutant": { "id": "a.py@40:boundary-shift:s1", "file": "a.py",
        "operator": "boundary-shift", "range": [40, 42],
        "original": ">=", "replacement": ">", "line": 10 } },
    { "status": "survived",
      "mutant": { "id": "a.py@60:boundary-shift:s2", "file": "a.py",
        "operator": "boundary-shift", "range": [60, 62],
        "original": "<=", "replacement": "<", "line": 11 } }
  ]
}"#;

#[test]
fn next_ranks_cluster_from_report() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    std::fs::write(&report, NEXT_REPORT).unwrap();

    Command::cargo_bin("fermut")
        .unwrap()
        .arg("next")
        .arg(&report)
        .arg("--all")
        .assert()
        .success() // `next` exits 0 even with survivors
        .stdout(contains("\"rank\": 1"))
        .stdout(contains("\"cluster_size\": 2"))
        .stdout(contains("\"ease\": \"high\""))
        // two survivors share the cluster → one sibling on the representative
        .stdout(contains("\"sibling_ids\""))
        .stdout(contains("a.py@60:boundary-shift:s2"))
        // no coverage-skip outcome in the report → flag false in JSON
        .stdout(contains("\"coverage_selected\": false"));
}
