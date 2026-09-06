//! TOML schema for `fermut.toml` and `[tool.fermut]` in `pyproject.toml`.
//!
//! All fields are optional; the runtime [`Config`](super::Config) layers CLI
//! args on top of these defaults. Unknown keys are rejected so typos surface
//! immediately.

use std::path::PathBuf;

use serde::Deserialize;

use super::{CacheScope, IsolationMode, RunnerKind};

/// Mirror of the TOML schema. Every field optional so partial files work.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub source_root: Option<PathBuf>,
    pub tests: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub timeout: Option<u64>,
    pub ty_filter: Option<bool>,
    pub ruff_filter: Option<bool>,
    pub experimental: Option<bool>,
    pub parity: Option<bool>,
    pub ops: Option<Vec<String>>,
    pub skip_ops: Option<Vec<String>>,
    pub diff_only: Option<String>,
    pub since: Option<String>,
    pub coverage: Option<PathBuf>,
    pub hypothesis_seed: Option<u64>,
    pub pytest_args: Option<Vec<String>>,
    pub cache: Option<bool>,
    pub cache_path: Option<PathBuf>,
    pub history: Option<bool>,
    pub history_path: Option<PathBuf>,
    pub sample: Option<f64>,
    pub sample_seed: Option<u64>,
    pub shard: Option<String>,
    pub runner: Option<RunnerKind>,
    /// Interpreter (path) or virtualenv (dir) to run pytest with. When set,
    /// fermut invokes `<python> -m pytest` instead of a bare `pytest` from
    /// PATH. A relative value resolves against the config's base dir.
    pub python: Option<PathBuf>,
    pub isolation: Option<IsolationMode>,
    pub equiv_detect: Option<bool>,
    pub cache_scope: Option<CacheScope>,
    pub fail_under: Option<f64>,
    /// Run the unmutated suite once before mutating and abort if it isn't
    /// green. Default true; set false to skip (e.g. CI already ran the suite).
    pub verify_baseline: Option<bool>,
    /// Wall-clock cap (seconds) for the baseline run. Default 300.
    pub baseline_timeout: Option<u64>,
    /// Wall-clock ceiling (seconds) on the per-mutant testing phase. Mutants
    /// are evaluated highest-value first and the remainder is skipped
    /// (`time-budget`) once the deadline passes. Unset = run the whole
    /// catalogue. Set via `--max-time`.
    pub max_time: Option<u64>,
    /// Glob patterns matched against paths relative to `source_root`. Files
    /// (and directories) matching any pattern are pruned during mutation
    /// collection. Examples: `"alembic/**"`, `"tests/integration/**"`,
    /// `"**/migrations/*.py"`.
    pub exclude: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
pub(super) struct PyprojectRoot {
    #[serde(default)]
    pub(super) tool: PyprojectTool,
}

#[derive(Default, Deserialize)]
pub(super) struct PyprojectTool {
    #[serde(default)]
    pub(super) fermut: Option<FileConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_fermut_toml() {
        let toml_src = r#"
            source_root = "src"
            tests = "tests"
            jobs = 4
            timeout = 60
            ty_filter = false
            experimental = true
            ops = ["arith-op-swap", "boundary-shift"]
            skip_ops = ["number-shift"]
            diff_only = "origin/main"
            coverage = "coverage.json"
            isolation = "hardlink"
        "#;
        let cfg: FileConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.source_root.as_deref(), Some(Path::new("src")));
        assert_eq!(cfg.jobs, Some(4));
        assert_eq!(cfg.timeout, Some(60));
        assert_eq!(cfg.ty_filter, Some(false));
        assert_eq!(cfg.experimental, Some(true));
        assert_eq!(
            cfg.ops.as_deref(),
            Some(["arith-op-swap".to_string(), "boundary-shift".to_string()].as_slice())
        );
        assert_eq!(cfg.diff_only.as_deref(), Some("origin/main"));
        assert_eq!(cfg.isolation, Some(IsolationMode::Hardlink));
    }

    #[test]
    fn parses_pyproject_tool_fermut() {
        let toml_src = r#"
            [tool.fermut]
            jobs = 8
            experimental = true
        "#;
        let root: PyprojectRoot = toml::from_str(toml_src).unwrap();
        let f = root.tool.fermut.unwrap();
        assert_eq!(f.jobs, Some(8));
        assert_eq!(f.experimental, Some(true));
    }

    #[test]
    fn rejects_unknown_keys() {
        let toml_src = r#"bogus = 1"#;
        assert!(toml::from_str::<FileConfig>(toml_src).is_err());
    }

    #[test]
    fn parses_exclude_list() {
        let toml_src = r#"
            exclude = ["alembic/**", "tests/integration/**", "**/migrations/*.py"]
        "#;
        let cfg: FileConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(
            cfg.exclude.as_deref(),
            Some(
                [
                    "alembic/**".to_string(),
                    "tests/integration/**".to_string(),
                    "**/migrations/*.py".to_string(),
                ]
                .as_slice()
            )
        );
    }
}
