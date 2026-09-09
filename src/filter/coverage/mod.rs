//! Coverage-driven test selection (plus the "filter" side effect of skipping
//! mutants on lines no test ever executed).
//!
//! Reads a `coverage.json` produced by coverage.py and indexes the per-line
//! **test contexts** — the list of pytest node ids that executed each line.
//! Generated with:
//!
//! ```sh
//! pytest --cov=src --cov-context=test    # requires pytest-cov; emits pytest nodeIDs
//! coverage json -o coverage.json --show-contexts
//! fermut run path --coverage coverage.json
//! ```
//!
//! `coverage run --context=LABEL` sets a single static label, not per-test
//! contexts, and won't work here. Coverage.py's `dynamic_context = test_function`
//! does produce per-test contexts but as dotted Python module paths, which
//! are not valid pytest selectors when fed back via `-k`. pytest-cov is the
//! only path that yields selectors fermut can hand back to pytest.
//!
//! What fermut does with the result:
//!
//! 1. **Filter:** mutants on lines with no test context are skipped (running
//!    them would always survive, since no test exercises the line).
//! 2. **Select:** for the lines that do have contexts, only those tests are
//!    passed to pytest per mutant. Cuts wall time dramatically when each
//!    mutant only needs a handful of tests instead of the full suite.
//!
//! Both consult a mutant through [`CoverageContexts::tests_for_mutant`], which
//! falls back to the head line of the mutant's enclosing statement. Some
//! continuation lines never appear in the contexts map even though the
//! statement around them ran: CPython folds a collection literal of three or
//! more constant elements into a single constant load attributed to the
//! literal's first line, so the element lines emit no line event and can carry
//! no test context. Without the fallback every mutant on such a line is
//! dropped as "uncovered" and no test can rescue it.
//!
//! This is narrower than "coverage.py is statement-granular". That holds for
//! the report's `executed_lines`, which collapses continuation lines onto the
//! statement head — but not for the per-line `contexts` map fermut reads, where
//! a wrapped call's argument lines, a boolean operand on its own line and a
//! literal below the folding threshold do each get their own entry. See
//! `docs/guides/coverage.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tracing::debug;

use super::Filter;
use crate::mutator::Mutant;

mod order;
mod parse;

pub(crate) use parse::is_sqlite;

/// Per-file, per-line index of test node ids that touched the line.
#[derive(Debug, Default)]
pub struct CoverageContexts {
    map: HashMap<PathBuf, HashMap<u32, Vec<String>>>,
    /// Cache mapping caller-supplied paths to the canonical form used in `map`.
    /// Populated lazily on first miss so per-mutant lookups skip a `canonicalize`
    /// syscall after the first hit per unique source file.
    canonical_cache: RwLock<HashMap<PathBuf, PathBuf>>,
    /// Global per-test coverage breadth: how many `(file, line)` cells each test
    /// node id executes across the whole coverage map. Computed once from `map`.
    /// Used as the smart-order *tie-breaker* (and cold-start fallback) behind the
    /// mutated-file-scoped [`file_test_breadth`](Self::file_test_breadth): among
    /// tests tied on the mutated file, the one narrower across the whole repo
    /// sinks below the one narrower there too. Global breadth alone can misrank a
    /// broad-but-relevant killer, which only weakens the `-x` short-circuit —
    /// never the verdict.
    test_breadth: HashMap<String, u32>,
    /// Per-`(file, test)` breadth: for each source file, how many of *its* lines
    /// each test node id executes. This is the specificity signal smart ordering
    /// actually wants — a test touching the fewest lines *of the mutated file* is
    /// the most focused on it, so the likeliest `-x` killer, even if it ranges
    /// widely across other files. Scoping to the mutated file is what keeps a
    /// broad integration test that heavily exercises the mutated function from
    /// sinking below a narrow test that merely grazes one of its lines.
    file_test_breadth: HashMap<PathBuf, HashMap<String, u32>>,
}

impl CoverageContexts {
    /// Resolve a caller-supplied path to the canonical key `map` and
    /// `file_test_breadth` are indexed by, reusing (and populating) the canonical
    /// cache. The single path-resolution ladder shared by
    /// [`tests_for`](Self::tests_for) and the breadth ordering, so the two can
    /// never disagree about a file's key. Direct hit (path already keys `map`)
    /// avoids the `canonicalize` syscall; misses canonicalize once and cache the
    /// mapping — unique-file count is small relative to mutant count, so hit rate
    /// is high after warm-up. Returns the path unchanged when it already keys
    /// `map`.
    fn resolve_key(&self, file: &Path) -> PathBuf {
        if self.map.contains_key(file) {
            return file.to_path_buf();
        }
        if let Some(canonical) = self
            .canonical_cache
            .read()
            .ok()
            .and_then(|g| g.get(file).cloned())
        {
            return canonical;
        }
        let canonical = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        if let Ok(mut guard) = self.canonical_cache.write() {
            guard.insert(file.to_path_buf(), canonical.clone());
        }
        canonical
    }

    /// Test node ids that executed `file:line`, or `None` if none recorded.
    /// Caller distinguishes "no contexts → skip mutant" from "selection empty".
    ///
    /// Caller paths often already match the canonical form stored in `map`
    /// (mutants and coverage typically share an absolute prefix), so we try a
    /// direct lookup first. On miss we canonicalize and cache the mapping —
    /// unique-file count is small relative to mutant count, so cache hit rate
    /// is high after warm-up.
    pub fn tests_for(&self, file: &Path, line: u32) -> Option<&[String]> {
        self.map
            .get(&self.resolve_key(file))
            .and_then(|per_line| per_line.get(&line))
            .map(|v| v.as_slice())
    }

    /// Test node ids that executed a mutant, or `None` if none recorded.
    ///
    /// Tries the mutated line first, then the head line of the enclosing
    /// statement — the line coverage.py attributes the whole statement's
    /// execution to. Both the filter and per-mutant test selection go through
    /// here so they cannot disagree about whether a mutant is covered.
    pub fn tests_for_mutant(&self, m: &Mutant) -> Option<&[String]> {
        if let Some(tests) = self.tests_for(&m.file, m.line) {
            if !tests.is_empty() {
                return Some(tests);
            }
        }
        if m.stmt_line == 0 || m.stmt_line == m.line {
            return None;
        }
        self.tests_for(&m.file, m.stmt_line)
    }
}

pub struct CoverageFilter {
    ctx: Arc<CoverageContexts>,
}

impl CoverageFilter {
    pub fn new(ctx: Arc<CoverageContexts>) -> Self {
        Self { ctx }
    }
}

impl Filter for CoverageFilter {
    fn name(&self) -> &'static str {
        "coverage"
    }

    fn admits(&self, m: &Mutant) -> Result<bool> {
        match self.ctx.tests_for_mutant(m) {
            Some(tests) if !tests.is_empty() => Ok(true),
            _ => {
                // One uncovered mutant per untested line on large targets would
                // flood logs at `warn`. Drop to `debug`; aggregate counts are
                // already reported by the higher-level filter summary.
                debug!(
                    file = %m.file.display(),
                    line = m.line,
                    stmt_line = m.stmt_line,
                    "no test context for line or its statement head; skipping mutant"
                );
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse::{detect_coverage_cwd, is_sqlite, numbits_to_lines};
    use super::*;

    fn write_doc(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("coverage.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn make_mutant(file: PathBuf, line: u32) -> Mutant {
        crate::mutator::Mutant {
            id: "id".into(),
            file,
            operator: crate::mutator::Operator::ArithOpSwap,
            range: ruff_text_size::TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line,
            stmt_line: line,
        }
    }

    #[test]
    fn detect_coverage_cwd_absolute_keys_use_project_root() {
        // `.coverage` SQLite (and any coverage.py run with `relative_files`
        // off) records ABSOLUTE file keys. Those `exists()` regardless of the
        // dir we probe, so the relative-key join heuristic can't discriminate
        // — it would pick `source_root` (`.../src`) and hand back the wrong
        // cwd, prefixing every node id with `src/`. Assert we fall back to
        // `project_root` (pytest's rootdir) instead.
        // Keys must be genuinely absolute on the host platform: on Windows a
        // `/proj/...` path lacks a drive prefix, so `is_absolute()` is false and
        // the probe would (correctly) treat it as relative. Use a drive-rooted
        // path there so the "all keys absolute" branch is what we exercise.
        #[cfg(windows)]
        let (source_root, project_root, abs_keys) = (
            Path::new(r"C:\proj\src"),
            Path::new(r"C:\proj"),
            [r"C:\proj\src\calculator.py", r"C:\proj\src\util.py"],
        );
        #[cfg(not(windows))]
        let (source_root, project_root, abs_keys) = (
            Path::new("/proj/src"),
            Path::new("/proj"),
            ["/proj/src/calculator.py", "/proj/src/util.py"],
        );
        assert_eq!(
            detect_coverage_cwd(source_root, project_root, &abs_keys),
            project_root.to_path_buf()
        );
    }

    #[test]
    fn detect_coverage_cwd_relative_keys_probe_ancestors() {
        // Relative keys still drive the join-probe: coverage run from the
        // project root records `src/calculator.py`, which resolves under the
        // parent of `source_root`, not `source_root` itself.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        let src = proj.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("calculator.py"), "x = 1\n").unwrap();

        let cwd = detect_coverage_cwd(&src, proj, &["src/calculator.py"]);
        assert_eq!(cwd, proj.to_path_buf());
    }

    #[test]
    fn parses_contexts_strips_phase_suffix_and_empties() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/test_a.py::test_x|run", "", "tests/test_a.py::test_x|teardown"],
                        "2": [""]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        let tests = ctx.tests_for(&py, 1).unwrap();
        assert_eq!(tests, &["tests/test_a.py::test_x".to_string()]);
        // Line 2 only had empty contexts → dropped entirely.
        assert!(ctx.tests_for(&py, 2).is_none());
    }

    #[test]
    fn breadth_map_counts_lines_per_test() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foo.py"), "a = 1\nb = 2\nc = 3\n").unwrap();
        // test_wide covers 3 lines; test_narrow covers 1.
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/t.py::test_wide|run"],
                        "2": ["tests/t.py::test_wide|run", "tests/t.py::test_narrow|run"],
                        "3": ["tests/t.py::test_wide|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let breadth = ctx.breadth_map();
        assert_eq!(breadth.get("tests/t.py::test_wide"), Some(&3));
        assert_eq!(breadth.get("tests/t.py::test_narrow"), Some(&1));
        // A test not in the coverage map has no breadth.
        assert_eq!(breadth.get("tests/t.py::test_absent"), None);
    }

    fn ctx_with_breadth(dir: &Path) -> Arc<CoverageContexts> {
        std::fs::write(dir.join("foo.py"), "a = 1\nb = 2\nc = 3\n").unwrap();
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/t.py::wide|run"],
                        "2": ["tests/t.py::wide|run", "tests/t.py::narrow|run"],
                        "3": ["tests/t.py::wide|run", "tests/t.py::mid|run"]
                    }
                }
            }
        }"#;
        // wide→3 lines, mid→1, narrow→1.
        let path = write_doc(dir, doc);
        CoverageContexts::from_json(&path, dir, dir).unwrap()
    }

    #[test]
    fn order_by_breadth_puts_the_most_specific_test_first() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        // Input in an arbitrary order; wide (breadth 3) must sink, the two
        // breadth-1 tests keep their relative input order (stable).
        let input: Vec<String> = ["tests/t.py::wide", "tests/t.py::narrow", "tests/t.py::mid"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
        assert_eq!(
            out,
            vec![
                "tests/t.py::narrow".to_string(),
                "tests/t.py::mid".to_string(),
                "tests/t.py::wide".to_string(),
            ]
        );
    }

    #[test]
    fn order_by_breadth_unknown_ids_sort_last_and_keep_order() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        // `ghost` isn't in the coverage map → treated as maximally broad → last;
        // `narrow` (breadth 1) leads.
        let input: Vec<String> = ["tests/t.py::ghost", "tests/t.py::narrow"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
        assert_eq!(out[0], "tests/t.py::narrow");
        assert_eq!(out[1], "tests/t.py::ghost");
    }

    #[test]
    fn order_by_breadth_is_always_a_permutation() {
        // Verdict-invariance: ordering only permutes the set. pytest `-x` exits
        // non-zero iff any selected test fails — independent of order — so an
        // identical set is an identical kill/survive verdict.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        for raw in [
            vec![],
            vec!["tests/t.py::wide"],
            vec!["tests/t.py::wide", "tests/t.py::narrow", "tests/t.py::mid"],
            vec!["ghost", "tests/t.py::wide", "dup", "dup"],
        ] {
            let input: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
            let out: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
            let mut a = input.clone();
            let mut b = out.clone();
            a.sort();
            b.sort();
            assert_eq!(a, b, "must be a permutation of {input:?}, got {out:?}");
        }
    }

    #[test]
    fn order_by_breadth_in_scopes_to_the_mutated_file() {
        // `big` covers 2 lines of foo.py and nothing else → global breadth 2.
        // `small` covers 1 line of foo.py but all 3 lines of bar.py → global 4.
        // Global ordering would put `big` first (2 < 4); scoped-to-foo ordering
        // must put `small` first (1 foo-line < 2), because `small` is the test
        // most focused on the mutated file — the fix for the "broad-but-relevant
        // killer sinks" misrank.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foo.py"), "a = 1\nb = 2\n").unwrap();
        std::fs::write(tmp.path().join("bar.py"), "x = 1\ny = 2\nz = 3\n").unwrap();
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/t.py::big|run"],
                        "2": ["tests/t.py::big|run", "tests/t.py::small|run"]
                    }
                },
                "bar.py": {
                    "contexts": {
                        "1": ["tests/t.py::small|run"],
                        "2": ["tests/t.py::small|run"],
                        "3": ["tests/t.py::small|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        let input: Vec<String> = ["tests/t.py::big", "tests/t.py::small"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Global ordering: big (2) before small (4).
        let global: Vec<String> = ctx.order_by_breadth(&input).into_iter().cloned().collect();
        assert_eq!(global[0], "tests/t.py::big");

        // Scoped to foo.py: small (1 foo-line) before big (2 foo-lines).
        let foo = tmp.path().join("foo.py");
        let scoped: Vec<String> = ctx
            .order_by_breadth_in(&foo, &input)
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(
            scoped,
            vec![
                "tests/t.py::small".to_string(),
                "tests/t.py::big".to_string()
            ],
            "scoped ordering prefers the test narrowest in the mutated file"
        );
    }

    #[test]
    fn order_by_breadth_in_unknown_file_falls_back_to_global() {
        // A mutated file with no coverage entry → no per-file breadth → the
        // scoped method must fall back to global breadth, not scramble the order.
        // wide→3 global, narrow/mid→1 global → narrow, mid, wide.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        let input: Vec<String> = ["tests/t.py::wide", "tests/t.py::narrow", "tests/t.py::mid"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out: Vec<String> = ctx
            .order_by_breadth_in(Path::new("/nonexistent/nope.py"), &input)
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(
            out,
            vec![
                "tests/t.py::narrow".to_string(),
                "tests/t.py::mid".to_string(),
                "tests/t.py::wide".to_string(),
            ],
            "no per-file breadth → global tie-breaker orders the set"
        );
    }

    #[test]
    fn order_by_breadth_in_unknown_ids_sort_last_and_keep_order() {
        // `ghost` is in neither the per-file nor the global breadth map → both
        // keys u32::MAX → sorts last; the known narrow test leads.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_breadth(tmp.path());
        let foo = tmp.path().join("foo.py");
        let input: Vec<String> = ["tests/t.py::ghost", "tests/t.py::narrow"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out: Vec<String> = ctx
            .order_by_breadth_in(&foo, &input)
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(out[0], "tests/t.py::narrow");
        assert_eq!(out[1], "tests/t.py::ghost");
    }

    #[test]
    fn missing_coverage_file_gives_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("coverage.json");
        let err = CoverageContexts::from_path(&missing, tmp.path(), tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "got: {msg}");
        // Names the two generation commands and the opt-out.
        assert!(msg.contains("--cov-context=test"), "got: {msg}");
        assert!(msg.contains("coverage json"), "got: {msg}");
        assert!(msg.contains("--no-coverage"), "got: {msg}");
        // Must NOT leak the bare OS error.
        assert!(!msg.contains("os error"), "leaked raw OS error: {msg}");
    }

    #[test]
    fn rejects_doc_with_no_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = r#"{"files": { "foo.py": { "contexts": {} } }}"#;
        let path = write_doc(tmp.path(), doc);
        let err = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("no per-test contexts"));
    }

    #[test]
    fn rejects_when_all_contexts_outside_project_root() {
        // Sibling-package layout: project_root is `proj_a/`, but every test
        // node id points at `proj_b/tests/...`. Without the dedicated branch,
        // users get the misleading "regenerate with --show-contexts" message
        // even though the coverage file is well-formed.
        let tmp = tempfile::tempdir().unwrap();
        let proj_a = tmp.path().join("proj_a");
        let proj_b = tmp.path().join("proj_b");
        std::fs::create_dir_all(proj_a.join("src")).unwrap();
        std::fs::create_dir_all(proj_b.join("tests")).unwrap();
        std::fs::write(proj_a.join("src/foo.py"), "x = 1\n").unwrap();
        std::fs::write(proj_b.join("tests/test_a.py"), "def test_x(): pass\n").unwrap();

        let doc = r#"{
            "files": {
                "src/foo.py": {
                    "contexts": {
                        "1": ["../proj_b/tests/test_a.py::test_x|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let err = CoverageContexts::from_json(&path, &proj_a, &proj_a).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fall outside project_root"),
            "unexpected error: {msg}"
        );
        assert!(!msg.contains("Regenerate with"), "wrong branch: {msg}");
    }

    #[test]
    fn filter_admits_covered_skips_uncovered() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let f = CoverageFilter::new(ctx);

        assert!(f.admits(&make_mutant(py.clone(), 1)).unwrap());
        assert!(!f.admits(&make_mutant(py.clone(), 99)).unwrap());
    }

    /// A mutant on a continuation line whose own line carries no context, so a
    /// strict per-line lookup drops it and no test can rescue it.
    fn continuation_mutant(file: PathBuf, line: u32, stmt_line: u32) -> Mutant {
        Mutant {
            stmt_line,
            ..make_mutant(file, line)
        }
    }

    #[test]
    fn filter_admits_mutant_on_continuation_line_of_covered_statement() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        // DEFAULT_ITEMS = [        <- line 1, the head the fold attributes to
        //     "read",             <- lines 2-4, where the mutants land; three
        //     "write",               constant elements, so CPython folds them
        //     "admin",               onto line 1 and they get no context
        // ]
        std::fs::write(
            &py,
            "DEFAULT_ITEMS = [\n    \"read\",\n    \"write\",\n    \"admin\",\n]\n",
        )
        .unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let f = CoverageFilter::new(ctx.clone());

        let m = continuation_mutant(py.clone(), 2, 1);
        assert!(
            f.admits(&m).unwrap(),
            "mutant on a continuation line of a covered statement must not be dropped"
        );
        // Selection must agree with the filter, or the admitted mutant would
        // run against an empty test set.
        assert_eq!(
            ctx.tests_for_mutant(&m).unwrap(),
            &["tests/test_a.py::test_x".to_string()]
        );
    }

    #[test]
    fn own_line_context_wins_over_the_statement_head() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        // Both lines carry contexts, and the head's set is the looser one: a
        // test that short-circuits before the continuation line still records
        // the head. The tighter own-line set must win, or selection silently
        // widens to every test that merely entered the statement.
        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": ["tests/test_a.py::test_head|run",
                              "tests/test_a.py::test_elt|run"],
                        "2": ["tests/test_a.py::test_elt|run"]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        let m = continuation_mutant(py.clone(), 2, 1);
        assert_eq!(
            ctx.tests_for_mutant(&m).unwrap(),
            &["tests/test_a.py::test_elt".to_string()],
            "the fallback must not be reached when the mutated line has contexts"
        );
    }

    #[test]
    fn filter_still_skips_when_the_statement_head_is_uncovered() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        let f = CoverageFilter::new(ctx.clone());

        // Statement spans lines 40-41; neither is covered.
        let m = continuation_mutant(py.clone(), 41, 40);
        assert!(!f.admits(&m).unwrap());
        assert!(ctx.tests_for_mutant(&m).is_none());
    }

    #[test]
    fn absent_stmt_line_disables_the_fallback() {
        // `stmt_line: 0` is what a report written before the field existed
        // deserializes to. It must behave exactly like a per-line lookup
        // rather than resolving to line 1.
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();

        assert!(ctx
            .tests_for_mutant(&continuation_mutant(py.clone(), 2, 0))
            .is_none());
    }

    #[test]
    fn rebases_node_ids_when_coverage_ran_from_monorepo_root() {
        // Layout: monorepo/proj_a/{foo.py, tests/test_a.py}, coverage run
        // from monorepo/ so keys are `proj_a/foo.py` and node ids are
        // `proj_a/tests/test_a.py::test_x`. project_root = proj_a.
        let tmp = tempfile::tempdir().unwrap();
        let monorepo = tmp.path();
        let proj_a = monorepo.join("proj_a");
        let tests_dir = proj_a.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(proj_a.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(tests_dir.join("test_a.py"), "def test_x(): pass\n").unwrap();
        // pyproject.toml is what find_project_root keys on, but from_json
        // takes project_root directly — drop it so the test is hermetic.

        let doc = r#"{
            "files": {
                "proj_a/foo.py": {
                    "contexts": { "1": ["proj_a/tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(monorepo, doc);
        let ctx = CoverageContexts::from_json(&path, &proj_a, &proj_a).unwrap();

        let tests = ctx.tests_for(&proj_a.join("foo.py"), 1).unwrap();
        assert_eq!(tests, &["tests/test_a.py::test_x".to_string()]);
    }

    #[test]
    fn drops_node_ids_outside_project_root() {
        // Tests that live outside the project (sibling package in a monorepo)
        // can't be invoked from inside the mirror — drop them.
        let tmp = tempfile::tempdir().unwrap();
        let monorepo = tmp.path();
        let proj_a = monorepo.join("proj_a");
        let proj_b_tests = monorepo.join("proj_b/tests");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b_tests).unwrap();
        std::fs::write(proj_a.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(proj_b_tests.join("test_b.py"), "def test_y(): pass\n").unwrap();

        let doc = r#"{
            "files": {
                "proj_a/foo.py": {
                    "contexts": {
                        "1": [
                            "proj_a/tests/test_a.py::test_x|run",
                            "proj_b/tests/test_b.py::test_y|run"
                        ]
                    }
                }
            }
        }"#;
        // Only the proj_a test should survive; proj_b test lives outside
        // project_root and pytest in the mirror can't reach it.
        std::fs::create_dir_all(proj_a.join("tests")).unwrap();
        std::fs::write(proj_a.join("tests/test_a.py"), "def test_x(): pass\n").unwrap();

        let path = write_doc(monorepo, doc);
        let ctx = CoverageContexts::from_json(&path, &proj_a, &proj_a).unwrap();
        let tests = ctx.tests_for(&proj_a.join("foo.py"), 1).unwrap();
        assert_eq!(tests, &["tests/test_a.py::test_x".to_string()]);
    }

    #[test]
    fn handles_source_root_below_project_root() {
        // source_root is `proj/src/pkg` but project_root is `proj/`, and
        // coverage was run from `proj/`. Detection walks up from source_root
        // until file keys resolve, finding `proj/`.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path();
        let pkg = proj.join("src/pkg");
        let tests = proj.join("tests");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(pkg.join("foo.py"), "x = 1\n").unwrap();
        std::fs::write(tests.join("test_a.py"), "def test_x(): pass\n").unwrap();

        let doc = r#"{
            "files": {
                "src/pkg/foo.py": {
                    "contexts": { "1": ["tests/test_a.py::test_x|run"] }
                }
            }
        }"#;
        let path = write_doc(proj, doc);
        let ctx = CoverageContexts::from_json(&path, &pkg, proj).unwrap();

        let tests_for = ctx.tests_for(&pkg.join("foo.py"), 1).unwrap();
        assert_eq!(tests_for, &["tests/test_a.py::test_x".to_string()]);
    }

    /// Encode line numbers into a coverage.py numbits blob (inverse of
    /// `numbits_to_lines`), for building test fixtures.
    fn lines_to_numbits(lines: &[u32]) -> Vec<u8> {
        let max = lines.iter().copied().max().unwrap_or(0);
        let mut blob = vec![0u8; (max as usize / 8) + 1];
        for &n in lines {
            blob[n as usize / 8] |= 1 << (n % 8);
        }
        blob
    }

    /// Write a minimal coverage.py v7 SQLite db with the given
    /// (file_path, context, lines) rows.
    fn write_cov_db(path: &Path, rows: &[(&str, &str, &[u32])]) {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE file (id integer primary key, path text, unique(path));
             CREATE TABLE context (id integer primary key, context text, unique(context));
             CREATE TABLE line_bits (file_id integer, context_id integer, numbits blob,
                 unique(file_id, context_id));",
        )
        .unwrap();
        let mut next_file = 1i64;
        let mut next_ctx = 1i64;
        let mut files: HashMap<&str, i64> = HashMap::new();
        let mut ctxs: HashMap<&str, i64> = HashMap::new();
        for (file, ctx, lines) in rows {
            let fid = *files.entry(file).or_insert_with(|| {
                let id = next_file;
                next_file += 1;
                conn.execute("INSERT INTO file (id, path) VALUES (?1, ?2)", (id, file))
                    .unwrap();
                id
            });
            let cid = *ctxs.entry(ctx).or_insert_with(|| {
                let id = next_ctx;
                next_ctx += 1;
                conn.execute(
                    "INSERT INTO context (id, context) VALUES (?1, ?2)",
                    (id, ctx),
                )
                .unwrap();
                id
            });
            conn.execute(
                "INSERT INTO line_bits (file_id, context_id, numbits) VALUES (?1, ?2, ?3)",
                (fid, cid, lines_to_numbits(lines)),
            )
            .unwrap();
        }
    }

    #[test]
    fn numbits_roundtrips() {
        for lines in [vec![0u32], vec![1, 7, 8, 9, 22, 23, 31], vec![100, 255]] {
            assert_eq!(numbits_to_lines(&lines_to_numbits(&lines)), lines);
        }
    }

    #[test]
    fn sqlite_sniffer_distinguishes_db_from_json() {
        let tmp = tempfile::tempdir().unwrap();
        let json = tmp.path().join("coverage.json");
        std::fs::write(&json, r#"{"files": {}}"#).unwrap();
        assert!(!is_sqlite(&json).unwrap());

        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();
        let db = tmp.path().join(".coverage");
        write_cov_db(&db, &[("foo.py", "t.py::test_x|run", &[1])]);
        assert!(is_sqlite(&db).unwrap());
    }

    #[test]
    fn reads_coverage_sqlite_db() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\ny = 2\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        std::fs::write(tmp.path().join("tests/test_a.py"), "def test_x(): pass\n").unwrap();

        let db = tmp.path().join(".coverage");
        write_cov_db(
            &db,
            &[
                ("foo.py", "tests/test_a.py::test_x|run", &[1]),
                ("foo.py", "tests/test_a.py::test_x|setup", &[1]),
                ("foo.py", "", &[2]), // import-time context, dropped
            ],
        );

        // from_path sniffs SQLite and routes to from_coverage_db.
        let ctx = CoverageContexts::from_path(&db, tmp.path(), tmp.path()).unwrap();
        assert_eq!(
            ctx.tests_for(&py, 1).unwrap(),
            &["tests/test_a.py::test_x".to_string()]
        );
        // Line 2 only had the empty import-time context -> no selectable test.
        assert!(ctx.tests_for(&py, 2).is_none());
    }

    #[test]
    fn dedupes_repeated_test_ids_across_phases() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("foo.py");
        std::fs::write(&py, "x = 1\n").unwrap();

        let doc = r#"{
            "files": {
                "foo.py": {
                    "contexts": {
                        "1": [
                            "tests/test_a.py::test_x|run",
                            "tests/test_a.py::test_x|setup",
                            "tests/test_a.py::test_x|teardown"
                        ]
                    }
                }
            }
        }"#;
        let path = write_doc(tmp.path(), doc);
        let ctx = CoverageContexts::from_json(&path, tmp.path(), tmp.path()).unwrap();
        assert_eq!(ctx.tests_for(&py, 1).unwrap().len(), 1);
    }
}
