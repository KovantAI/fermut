//! Structured per-run results from fermut's embedded pytest reporter plugin.
//!
//! The plugin (`python/pytest_fermut/_fermut_reporter.py`, embedded here) is
//! loaded into every per-mutant pytest run with `-p _fermut_reporter` and
//! appends JSON-lines events to the file named by `FERMUT_RESULT`. Reading
//! those events replaces guesswork the exit code alone can't settle:
//!
//! - **Exit 4.** Under node-id selection a mutant that breaks a selected test
//!   module's import exits 4, the same as a stale node id. The plugin sees a
//!   failed `collect` for that module, a `conftest_error` when a conftest
//!   imports the mutated code, or a `config_error` raised through project code
//!   while pytest configures itself (a `filterwarnings` entry naming a warning
//!   class in the package under test imports it). The last two fail before
//!   any session starts. No `--collect-only` probe re-run is needed.
//! - **The killer.** The first failed test phase names the killing test, with
//!   no stdout capture or `-rfE` summary parsing.
//!
//! The exit code stays authoritative for the verdict itself; this only refines
//! it. A missing, empty, or unknown-schema file yields `None` and the runner
//! falls back to the exit-code path, so a plugin that failed to load can never
//! change a verdict.

use std::path::Path;

use serde::Deserialize;

/// Module name passed to `pytest -p`. Deliberately not `fermut`, so it never
/// collides with a pytest plugin registered under that name.
pub(crate) const REPORTER_MODULE: &str = "_fermut_reporter";

/// The plugin source, written into each worker mirror's plugin directory.
pub(crate) const REPORTER_SOURCE: &str =
    include_str!("../../python/pytest_fermut/_fermut_reporter.py");

/// Environment variable naming the per-run result file.
pub(crate) const RESULT_ENV: &str = "FERMUT_RESULT";

/// The result-file schema this parser understands.
const SCHEMA_VERSION: u64 = 1;

/// What the reporter plugin observed during one run.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RunResult {
    /// Collectors (test modules) that failed, by node id — an import error.
    pub collect_errors: Vec<String>,
    /// An initial conftest failed to import before the session started.
    pub conftest_error: bool,
    /// Configuring pytest failed before the session started (other than a
    /// conftest import). `Some(true)` when the exception chain ran through
    /// project code, i.e. the mutated package raised while being imported;
    /// `Some(false)` for a plain usage error.
    pub config_error: Option<bool>,
    /// Node ids of failed test phases, in the order they were reported.
    pub failures: Vec<String>,
}

#[derive(Deserialize)]
struct Event {
    ev: String,
    #[serde(default)]
    v: Option<u64>,
    #[serde(default)]
    nodeid: Option<String>,
    #[serde(default)]
    project: Option<bool>,
}

impl RunResult {
    /// Read and parse a result file. `None` when it is missing or unusable.
    pub(crate) fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        Self::parse(&String::from_utf8_lossy(&bytes))
    }

    /// Parse JSON-lines events. Lines that don't parse are skipped (a run
    /// killed mid-write leaves a truncated last line). Returns `None` when no
    /// session started and configuration didn't fail — the plugin never ran —
    /// or when any session reports a schema version other than
    /// [`SCHEMA_VERSION`].
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut out = Self::default();
        let mut started = false;
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<Event>(line) else {
                continue;
            };
            match event.ev.as_str() {
                "start" => {
                    if event.v != Some(SCHEMA_VERSION) {
                        return None;
                    }
                    started = true;
                }
                "conftest_error" => out.conftest_error = true,
                "config_error" => {
                    let project = event.project.unwrap_or(false);
                    out.config_error = Some(out.config_error.unwrap_or(false) || project);
                }
                // The session root (`""`) also reports failed under `-x` once a
                // module failed; only a named collector is evidence.
                "collect" => {
                    if let Some(id) = event.nodeid.filter(|id| !id.is_empty()) {
                        out.collect_errors.push(id);
                    }
                }
                "test" => {
                    if let Some(id) = event.nodeid {
                        out.failures.push(id);
                    }
                }
                _ => {}
            }
        }
        (started || out.conftest_error || out.config_error.is_some()).then_some(out)
    }

    /// Whether the run failed because test code could not be imported: a
    /// test module failed to collect, a conftest failed to load, or project
    /// code raised while pytest was configuring itself. (A green baseline
    /// under the same configuration rules out a pre-existing breakage.)
    pub(crate) fn import_broke(&self) -> bool {
        self.conftest_error || self.config_error == Some(true) || !self.collect_errors.is_empty()
    }

    /// The killing test: the first failed test phase.
    pub(crate) fn killer(&self) -> Option<&str> {
        self.failures.first().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_failure_names_first_failed_test() {
        let text = r#"{"v":1,"ev":"start","pytest":"8.4.2","pid":1}
{"ev":"begin","nodeid":"tests/t.py::a","pid":1}
{"ev":"dur","nodeid":"tests/t.py::a","dur":0.1,"pid":1}
{"ev":"begin","nodeid":"tests/t.py::b[x - y]","pid":1}
{"ev":"test","nodeid":"tests/t.py::b[x - y]","when":"call","dur":0.0,"pid":1}
{"ev":"test","nodeid":"tests/t.py::c","when":"setup","dur":0.0,"pid":1}
{"ev":"finish","exit":1,"pid":1}
"#;
        let r = RunResult::parse(text).unwrap();
        assert_eq!(r.killer(), Some("tests/t.py::b[x - y]"));
        assert_eq!(r.failures.len(), 2);
        assert!(!r.import_broke());
    }

    #[test]
    fn module_collect_error_is_an_import_break() {
        // What pytest 7–9 report under `-x` when a selected module's import
        // fails: the module, then the session root.
        let text = r#"{"v":1,"ev":"start","pytest":"9.1.1","pid":1}
{"ev":"collect","nodeid":"tests/test_a.py","pid":1}
{"ev":"collect","nodeid":"","pid":1}
{"ev":"finish","exit":4,"pid":1}
"#;
        let r = RunResult::parse(text).unwrap();
        assert!(r.import_broke());
        assert_eq!(r.collect_errors, vec!["tests/test_a.py".to_string()]);
        assert_eq!(r.killer(), None);
    }

    #[test]
    fn root_collect_failure_alone_is_not_an_import_break() {
        let text = r#"{"v":1,"ev":"start","pid":1}
{"ev":"collect","nodeid":"","pid":1}
"#;
        assert!(!RunResult::parse(text).unwrap().import_broke());
    }

    #[test]
    fn stale_node_id_is_not_an_import_break() {
        let text = r#"{"v":1,"ev":"start","pid":1}
{"ev":"finish","exit":4,"pid":1}
"#;
        let r = RunResult::parse(text).unwrap();
        assert!(!r.import_broke());
    }

    #[test]
    fn conftest_error_without_session_is_an_import_break() {
        let text = r#"{"ev":"conftest_error","path":"tests/conftest.py","exc":"ModuleNotFoundError","pid":1}
"#;
        let r = RunResult::parse(text).unwrap();
        assert!(r.conftest_error);
        assert!(r.import_broke());
    }

    #[test]
    fn config_error_in_project_code_is_an_import_break() {
        // pyjwt's `filterwarnings = ["ignore::jwt.warnings…"]` imports the
        // package while pytest configures; a mutant that makes it raise fails
        // there, before any session.
        let text = r#"{"ev":"config_error","exc":"UsageError","project":true,"pid":1}
"#;
        let r = RunResult::parse(text).unwrap();
        assert_eq!(r.config_error, Some(true));
        assert!(r.import_broke());
    }

    #[test]
    fn config_error_outside_project_is_not_an_import_break() {
        // e.g. `-W ignore::Bogus`: a usage error the mutant didn't cause. The
        // result is still trusted (no probe needed), it just isn't a kill.
        let text = r#"{"ev":"config_error","exc":"UsageError","project":false,"pid":1}
"#;
        let r = RunResult::parse(text).unwrap();
        assert_eq!(r.config_error, Some(false));
        assert!(!r.import_broke());
    }

    #[test]
    fn sessions_from_parallel_workers_are_merged() {
        // rstest runs several pytest sessions that append to the same file.
        let text = r#"{"v":1,"ev":"start","pid":1}
{"v":1,"ev":"start","pid":2}
{"ev":"dur","nodeid":"tests/t.py::a","dur":0.1,"pid":1}
{"ev":"test","nodeid":"tests/t.py::b","when":"call","dur":0.0,"pid":2}
{"ev":"finish","exit":0,"pid":1}
{"ev":"finish","exit":1,"pid":2}
"#;
        assert_eq!(
            RunResult::parse(text).unwrap().killer(),
            Some("tests/t.py::b")
        );
    }

    #[test]
    fn truncated_last_line_is_skipped() {
        let text = "{\"v\":1,\"ev\":\"start\",\"pid\":1}\n\
                    {\"ev\":\"test\",\"nodeid\":\"tests/t.py::a\",\"when\":\"call\",\"pid\":1}\n\
                    {\"ev\":\"test\",\"nodeid\":\"tests/t.p";
        assert_eq!(
            RunResult::parse(text).unwrap().failures,
            vec!["tests/t.py::a"]
        );
    }

    #[test]
    fn no_session_or_unknown_schema_falls_back() {
        assert_eq!(RunResult::parse(""), None);
        assert_eq!(RunResult::parse("not json\n"), None);
        assert_eq!(
            RunResult::parse("{\"ev\":\"dur\",\"nodeid\":\"t::a\",\"pid\":1}\n"),
            None,
            "events without a session start are not trusted"
        );
        assert_eq!(
            RunResult::parse("{\"v\":2,\"ev\":\"start\",\"pid\":1}\n"),
            None
        );
    }

    #[test]
    fn missing_file_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(RunResult::read(&tmp.path().join("absent.jsonl")), None);
    }
}
