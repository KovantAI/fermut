//! `fermut doctor` — diagnose project + environment.
//!
//! Walks the same project discovery as `run`/`init` and checks each
//! required prerequisite: Python interpreter, configured test runner,
//! `coverage` + `pytest-cov` (per-test contexts), `ty` (pre-filter),
//! plus the optional `gh` (for `pr-comment`). Emits one line per
//! check with a status (`ok` / `warn` / `fail`) and a short
//! remediation hint when it isn't `ok`.
//!
//! Exit code: `1` when any `fail` was reported, `0` otherwise. Pass
//! `--strict` to also fail on warnings (useful in CI to gate on a clean
//! environment).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use clap::Args;

use crate::config::loader::{ConfigSource, LoadedConfig};

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Where to start the project-root walk. Defaults to cwd.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Treat warnings as failures (exit non-zero on any warn).
    #[arg(long)]
    pub strict: bool,
}

pub fn run(args: DoctorArgs) -> Result<()> {
    doctor(DoctorOpts {
        path: args.path,
        strict: args.strict,
    })
}

#[derive(Debug, Clone)]
pub struct DoctorOpts {
    pub path: PathBuf,
    pub strict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
    /// A check that doesn't apply because it's configured off (e.g. `ty` when
    /// `ty_filter = false`). Shown rather than omitted so the printed list
    /// reconciles with the "required tools" docs; never affects exit code.
    Skip,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "ok  ",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            hint: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Skip,
            detail: detail.into(),
            hint: None,
        }
    }
}

pub fn doctor(opts: DoctorOpts) -> Result<()> {
    let checks = collect_checks(&opts.path);
    print_checks(&checks);
    let counts = tally(&checks);
    let skip_suffix = if counts.skip > 0 {
        format!(", {} skip", counts.skip)
    } else {
        String::new()
    };
    println!(
        "\nsummary: {} ok, {} warn, {} fail{}",
        counts.ok, counts.warn, counts.fail, skip_suffix
    );

    let bad = counts.fail > 0 || (opts.strict && counts.warn > 0);
    if bad {
        std::process::exit(1);
    }
    Ok(())
}

/// Run the same checks as the `doctor` subcommand but return them as a JSON
/// value instead of printing. Shared with the MCP server's `fermut_doctor`
/// tool. `healthy` is `true` when no check failed (warnings don't flip it,
/// matching `doctor`'s default non-strict exit code).
pub(crate) fn diagnose(path: &Path) -> serde_json::Value {
    let checks = collect_checks(path);
    let counts = tally(&checks);
    let check_values: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "status": c.status.label(),
                "detail": c.detail,
                "hint": c.hint,
            })
        })
        .collect();
    serde_json::json!({
        "checks": check_values,
        "summary": {
            "ok": counts.ok,
            "warn": counts.warn,
            "fail": counts.fail,
            "skip": counts.skip,
        },
        "healthy": counts.fail == 0,
    })
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Tally {
    ok: usize,
    warn: usize,
    fail: usize,
    skip: usize,
}

fn tally(checks: &[Check]) -> Tally {
    let mut t = Tally::default();
    for c in checks {
        match c.status {
            Status::Ok => t.ok += 1,
            Status::Warn => t.warn += 1,
            Status::Fail => t.fail += 1,
            Status::Skip => t.skip += 1,
        }
    }
    t
}

fn print_checks(checks: &[Check]) {
    let name_width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        println!(
            "  [{}] {:<width$}  {}",
            c.status.glyph(),
            c.name,
            c.detail,
            width = name_width
        );
        if let Some(hint) = &c.hint {
            println!(
                "           {:<width$}  hint: {}",
                "",
                hint,
                width = name_width
            );
        }
    }
}

fn collect_checks(start: &Path) -> Vec<Check> {
    let mut out = Vec::new();

    // ---- config discovery ----
    let loaded = LoadedConfig::load(start).ok();
    out.push(match &loaded {
        Some(c) if c.source == ConfigSource::FermutToml => {
            Check::ok("config", format!("fermut.toml at {}", c.base_dir.display()))
        }
        Some(c) if c.source == ConfigSource::Pyproject => Check::ok(
            "config",
            format!("[tool.fermut] in {}/pyproject.toml", c.base_dir.display()),
        ),
        _ => Check::warn(
            "config",
            "no fermut.toml or [tool.fermut] found",
            "run `fermut init` to generate one",
        ),
    });

    // ---- virtualenv discovery ----
    // Resolve the project's venv (the same walk `run`/`coverage` use) so every
    // tool probe below can look inside it, not just on PATH. This is what lets
    // `fermut doctor` see uv-managed tools without the caller activating the
    // venv or prepending `.venv/bin` to PATH.
    let scope = loaded
        .as_ref()
        .map(|c| c.base_dir.clone())
        .unwrap_or_else(|| start.to_path_buf());
    let venv_python = crate::runner::resolve_python(&scope, None);
    let venv_bin: Option<PathBuf> = venv_python
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    out.push(match &venv_bin {
        Some(bin) => Check::ok("venv", format!("probing tools in {}", bin.display())),
        None => Check::skip("venv", "none found — probing tools on PATH only"),
    });
    let vbin = venv_bin.as_deref();

    // ---- python ----
    out.push(check_python(vbin));

    // ---- test runner ----
    let runner = loaded
        .as_ref()
        .and_then(|c| c.file.runner)
        .unwrap_or(crate::config::RunnerKind::Pytest);
    out.push(check_runner(runner, vbin));

    // ---- coverage + pytest-cov (required; per-test coverage filter) ----
    out.push(check_coverage_binary(vbin));
    out.push(check_pytest_cov(vbin));

    // ---- coverage.json (only meaningful when wired in config) ----
    let coverage_wired = loaded.as_ref().and_then(|c| c.file.coverage.clone());
    if let Some(rel_path) = coverage_wired {
        let base = loaded
            .as_ref()
            .map(|c| c.base_dir.as_path())
            .unwrap_or(start);
        out.push(check_coverage_file(base, &rel_path));
    }

    // ---- ty (required by default; only skipped if explicitly disabled) ----
    let ty_on = loaded
        .as_ref()
        .and_then(|c| c.file.ty_filter)
        .unwrap_or(true);
    if ty_on {
        out.push(check_tool(
            "ty",
            &["ty", "--version"],
            "type-aware mutant pre-filter",
            "uv tool install ty",
            /* required */ true,
            vbin,
        ));
    } else {
        // Surface it as skipped instead of dropping the row — otherwise the
        // output (where `ty` is listed as required) silently lacks a `ty`
        // line and the user can't tell if it passed or wasn't checked.
        out.push(Check::skip("ty", "disabled (ty_filter = false)"));
    }

    // ---- ruff (only when explicitly enabled) ----
    if loaded.as_ref().and_then(|c| c.file.ruff_filter) == Some(true) {
        out.push(check_tool(
            "ruff",
            &["ruff", "--version"],
            "lint pre-filter",
            "uv tool install ruff",
            /* required */ true,
            vbin,
        ));
    }

    // ---- gh (always optional, used by `fermut pr-comment`) ----
    out.push(check_tool(
        "gh",
        &["gh", "--version"],
        "needed for `fermut pr-comment`",
        "install GitHub CLI: https://cli.github.com",
        /* required */ false,
        vbin,
    ));

    // ---- common gotchas ----
    out.extend(check_gotchas(loaded.as_ref()));

    out
}

fn check_python(venv_bin: Option<&Path>) -> Check {
    let candidates = ["python3", "python"];
    let probed: Vec<(&str, Option<String>)> = candidates
        .into_iter()
        .map(|bin| (bin, tool_version(&[bin, "--version"], venv_bin)))
        .collect();
    pick_python(&probed)
}

fn pick_python(probed: &[(&str, Option<String>)]) -> Check {
    let mut first_problem: Option<Check> = None;
    let mut any_found = false;
    for (bin, version) in probed {
        let Some(version) = version else {
            continue;
        };
        any_found = true;
        match parse_python_version(version) {
            Some(parsed) if parsed >= (3, 10) => {
                return Check::ok("python", format!("{bin} {version}"));
            }
            Some(_) => {
                if first_problem.is_none() {
                    first_problem = Some(Check::fail(
                        "python",
                        format!("{bin} {version} (3.10 is the supported minimum)"),
                        "upgrade Python or pass a newer interpreter via PATH — \
                         older versions may still run but are unsupported",
                    ));
                }
            }
            None => {
                if first_problem.is_none() {
                    first_problem = Some(Check::warn(
                        "python",
                        format!("{bin} {version} (could not parse version)"),
                        "ensure `python --version` prints `Python X.Y.Z`",
                    ));
                }
            }
        }
    }
    if any_found {
        return first_problem.expect("any_found implies a problem was recorded");
    }
    Check::fail(
        "python",
        "no `python` or `python3` on PATH",
        "install Python ≥ 3.10",
    )
}

fn parse_python_version(s: &str) -> Option<(u32, u32)> {
    // Output is `Python 3.12.3` (or similar). Be permissive.
    let trimmed = s.trim().trim_start_matches("Python").trim();
    let mut parts = trimmed.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

fn check_runner(runner: crate::config::RunnerKind, venv_bin: Option<&Path>) -> Check {
    match runner {
        crate::config::RunnerKind::Pytest => check_tool(
            "pytest",
            &["pytest", "--version"],
            "configured runner",
            "pipx install pytest  (or `uv add --dev pytest`)",
            /* required */ true,
            venv_bin,
        ),
        crate::config::RunnerKind::Rstest => check_tool(
            "rstest",
            &["rstest", "--version"],
            "configured runner",
            "pipx install rstest  (or `uv add --dev rstest`)",
            /* required */ true,
            venv_bin,
        ),
        crate::config::RunnerKind::Unittest => {
            // Stdlib; presence implies Python is present.
            Check::ok("runner", "unittest (stdlib)")
        }
    }
}

fn check_coverage_binary(venv_bin: Option<&Path>) -> Check {
    check_tool(
        "coverage",
        &["coverage", "--version"],
        "needed to generate coverage.json with per-test contexts",
        "uv add --dev coverage  (or pip install coverage)",
        /* required */ true,
        venv_bin,
    )
}

fn check_pytest_cov(venv_bin: Option<&Path>) -> Check {
    // pytest-cov isn't a binary; probe the Python interpreter that fermut
    // would actually launch tests against — the venv's `python` when one is
    // discovered (venv_bin prepends it to PATH), else `python3` on PATH.
    let probe = [
        "python3",
        "-c",
        "import pytest_cov; print(pytest_cov.__version__)",
    ];
    match tool_version(&probe, venv_bin) {
        Some(v) => Check::ok(
            "pytest-cov",
            format!("pytest-cov {v} — per-test coverage contexts"),
        ),
        None => Check::fail(
            "pytest-cov",
            "not importable from python3 (per-test coverage contexts)",
            "uv add --dev pytest-cov  (or pip install pytest-cov)",
        ),
    }
}

fn check_coverage_file(base: &Path, rel: &Path) -> Check {
    let path = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    };
    if !path.is_file() {
        return Check::warn(
            "coverage.json",
            format!("{} not found", path.display()),
            "produce it before each run:  pytest --cov=src --cov-context=test && coverage json -o coverage.json --show-contexts",
        );
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if text.contains("\"contexts\"") {
                Check::ok(
                    "coverage.json",
                    format!("{} (has per-test contexts)", path.display()),
                )
            } else {
                Check::fail(
                    "coverage.json",
                    format!("{} has no per-test contexts", path.display()),
                    "regenerate with `pytest --cov=src --cov-context=test` and `coverage json --show-contexts` (requires pytest-cov)",
                )
            }
        }
        Err(e) => Check::fail(
            "coverage.json",
            format!("{}: {e}", path.display()),
            "regenerate the coverage report",
        ),
    }
}

fn check_tool(
    name: &'static str,
    cmd: &[&str],
    purpose: &str,
    install_hint: &str,
    required: bool,
    venv_bin: Option<&Path>,
) -> Check {
    match tool_version(cmd, venv_bin) {
        Some(v) => Check::ok(name, format!("{v} — {purpose}")),
        None if required => Check::fail(name, format!("not on PATH ({purpose})"), install_hint),
        None => Check::warn(name, format!("not on PATH ({purpose})"), install_hint),
    }
}

/// Probe a tool's `--version` (or similar). When `venv_bin` is set — the
/// project's virtualenv `bin/` — it is prepended to the child's PATH, so a bare
/// `pytest`/`coverage`/`python` resolves to the venv's copy first and falls
/// back to PATH only when the venv lacks it. This is why `fermut doctor` no
/// longer needs the caller to activate the venv (or prepend `.venv/bin` to PATH
/// by hand) to see uv-managed tools.
fn tool_version(cmd: &[&str], venv_bin: Option<&Path>) -> Option<String> {
    let (head, tail) = cmd.split_first()?;
    let mut command = Command::new(head);
    command.args(tail);
    if let Some(bin) = venv_bin {
        let existing = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin.to_path_buf()];
        paths.extend(std::env::split_paths(&existing));
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("PATH", joined);
        }
    }
    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    // Some tools (e.g. older `python`) print version to stderr.
    let text = if stdout.is_empty() { stderr } else { stdout };
    if text.is_empty() {
        None
    } else {
        Some(text.lines().next().unwrap_or(&text).to_string())
    }
}

fn check_gotchas(loaded: Option<&LoadedConfig>) -> Vec<Check> {
    let Some(loaded) = loaded else {
        return Vec::new();
    };
    let mut out = Vec::new();

    // Hypothesis seed: if the project depends on hypothesis but no seed is
    // pinned, survivor lists can flicker between runs.
    if project_uses_hypothesis(&loaded.base_dir) && loaded.file.hypothesis_seed.is_none() {
        out.push(Check::warn(
            "hypothesis",
            "project uses Hypothesis but no `hypothesis_seed` is pinned",
            "set `hypothesis_seed = <N>` in fermut.toml (or pass --hypothesis-seed)",
        ));
    }

    // Tests directory sanity.
    let tests_rel = loaded
        .file
        .tests
        .as_deref()
        .map(|p| loaded.base_dir.join(p));
    if let Some(t) = tests_rel {
        if !t.is_dir() {
            out.push(Check::fail(
                "tests",
                format!("{} does not exist", t.display()),
                "create the directory or fix `tests = ...` in the config",
            ));
        }
    }

    // Stale legacy mutation-tool artifacts. These directories contain
    // copies of the source + tests with duplicate basenames, which
    // breaks pytest collection (`ModuleNotFoundError: No module named
    // 'foo.bar_test'`) before fermut even gets a chance to run.
    if let Some(check) = check_legacy_mutation_artifacts(&loaded.base_dir) {
        out.push(check);
    }

    out
}

fn check_legacy_mutation_artifacts(base: &Path) -> Option<Check> {
    // Known artifact directories from prior mutation tools.
    //
    //  - `mutants/`         — mutmut 2.x / 3.x output (contains a `tests/`
    //                         subtree that pytest will collect by default).
    //  - `.mutmut-cache/`   — mutmut session DB; harmless on its own but
    //                         signals leftover state.
    //  - `cosmic-ray-data/` — cosmic-ray work dir.
    //  - `.cache/mutpy/`    — mutpy session dir.
    const ARTIFACTS: &[&str] = &[
        "mutants",
        ".mutmut-cache",
        "cosmic-ray-data",
        ".cache/mutpy",
    ];

    let found: Vec<&str> = ARTIFACTS
        .iter()
        .copied()
        .filter(|name| base.join(name).exists())
        .collect();
    if found.is_empty() {
        return None;
    }

    // Severity hinges on whether the artifact actually breaks pytest
    // collection. `mutants/tests` is the common offender — collect-time
    // ModuleNotFoundError. Other artifacts are noisy but not fatal.
    let breaks_pytest = found
        .iter()
        .any(|name| base.join(name).join("tests").is_dir());

    let detail = format!(
        "found legacy mutation-tool artifact(s): {}",
        found.join(", ")
    );
    let hint = if breaks_pytest {
        "delete the directory or pass `--ignore=<dir>` to pytest \
         (e.g. `pytest --ignore=mutants`) — duplicate test modules will \
         break collection otherwise"
    } else {
        "delete to avoid accidental inclusion in coverage or mutation runs"
    };

    Some(Check::warn("legacy-artifacts", detail, hint))
}

fn project_uses_hypothesis(base: &Path) -> bool {
    let pyproject = base.join("pyproject.toml");
    if let Ok(text) = std::fs::read_to_string(&pyproject) {
        if text.contains("hypothesis") {
            return true;
        }
    }
    for lock in ["uv.lock", "poetry.lock", "pdm.lock", "Pipfile.lock"] {
        if let Ok(text) = std::fs::read_to_string(base.join(lock)) {
            if text.contains("hypothesis") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn tool_version_finds_a_tool_only_in_the_venv_bin() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // A fake tool that exists ONLY in the venv bin, not on PATH.
        let tool = bin.join("fermut-faketool");
        std::fs::write(&tool, "#!/bin/sh\necho 'faketool 9.9.9'\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Without the venv bin, the bare name is not on PATH → not found.
        assert!(tool_version(&["fermut-faketool"], None).is_none());
        // With the venv bin prepended, it resolves.
        let v = tool_version(&["fermut-faketool"], Some(&bin));
        assert_eq!(v.as_deref(), Some("faketool 9.9.9"));
    }

    #[test]
    fn parse_python_version_handles_common_formats() {
        assert_eq!(parse_python_version("Python 3.12.3"), Some((3, 12)));
        assert_eq!(parse_python_version("Python 3.10.14+"), Some((3, 10)));
        assert_eq!(parse_python_version("3.11.0"), Some((3, 11)));
        assert_eq!(parse_python_version("garbage"), None);
    }

    #[test]
    fn tally_counts_by_status() {
        let checks = vec![
            Check::ok("a", "x"),
            Check::ok("b", "x"),
            Check::warn("c", "x", "y"),
            Check::fail("d", "x", "y"),
            Check::skip("e", "off"),
        ];
        let t = tally(&checks);
        assert_eq!(
            t,
            Tally {
                ok: 2,
                warn: 1,
                fail: 1,
                skip: 1,
            }
        );
    }

    #[test]
    fn skip_status_never_counts_as_failure() {
        // A skipped check must not push fail/warn — doctor stays green.
        let t = tally(&[Check::skip("ty", "disabled (ty_filter = false)")]);
        assert_eq!(t.fail, 0);
        assert_eq!(t.warn, 0);
        assert_eq!(t.skip, 1);
    }

    #[test]
    fn check_coverage_file_flags_missing_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("coverage.json");
        std::fs::write(&p, "{\"meta\": {}}").unwrap();
        let c = check_coverage_file(tmp.path(), Path::new("coverage.json"));
        assert_eq!(c.status, Status::Fail);
    }

    #[test]
    fn check_coverage_file_accepts_contexts() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("coverage.json");
        std::fs::write(&p, "{\"files\": {\"x.py\": {\"contexts\": {}}}}").unwrap();
        let c = check_coverage_file(tmp.path(), Path::new("coverage.json"));
        assert_eq!(c.status, Status::Ok);
    }

    #[test]
    fn check_coverage_file_warns_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let c = check_coverage_file(tmp.path(), Path::new("coverage.json"));
        assert_eq!(c.status, Status::Warn);
    }

    #[test]
    fn pick_python_falls_through_when_first_candidate_too_old() {
        let probed = vec![
            ("python3", Some("Python 3.9.18".to_string())),
            ("python", Some("Python 3.11.7".to_string())),
        ];
        let c = pick_python(&probed);
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("python "), "got: {}", c.detail);
        assert!(c.detail.contains("3.11.7"), "got: {}", c.detail);
    }

    #[test]
    fn pick_python_falls_through_when_first_candidate_unparseable() {
        let probed = vec![
            ("python3", Some("weird custom build".to_string())),
            ("python", Some("Python 3.12.1".to_string())),
        ];
        let c = pick_python(&probed);
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("3.12.1"), "got: {}", c.detail);
    }

    #[test]
    fn pick_python_reports_fail_when_all_candidates_too_old() {
        let probed = vec![
            ("python3", Some("Python 3.9.18".to_string())),
            ("python", Some("Python 3.8.10".to_string())),
        ];
        let c = pick_python(&probed);
        assert_eq!(c.status, Status::Fail);
        // First problem wins: python3.
        assert!(c.detail.contains("python3"), "got: {}", c.detail);
    }

    #[test]
    fn pick_python_reports_fail_when_no_candidates_on_path() {
        let probed: Vec<(&str, Option<String>)> = vec![("python3", None), ("python", None)];
        let c = pick_python(&probed);
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("no `python`"), "got: {}", c.detail);
    }

    #[test]
    fn pick_python_skips_missing_candidate_and_uses_next() {
        let probed = vec![
            ("python3", None),
            ("python", Some("Python 3.11.0".to_string())),
        ];
        let c = pick_python(&probed);
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("3.11.0"), "got: {}", c.detail);
    }

    #[test]
    fn project_uses_hypothesis_detects_pyproject_mention() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\ndependencies = [\"hypothesis\"]\n",
        )
        .unwrap();
        assert!(project_uses_hypothesis(tmp.path()));
    }

    #[test]
    fn legacy_artifacts_silent_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(check_legacy_mutation_artifacts(tmp.path()).is_none());
    }

    #[test]
    fn legacy_artifacts_warns_on_mutmut_output() {
        let tmp = tempfile::tempdir().unwrap();
        // mutmut's `mutants/tests/` is the collection-breaker.
        std::fs::create_dir_all(tmp.path().join("mutants/tests")).unwrap();
        let c = check_legacy_mutation_artifacts(tmp.path()).expect("should detect");
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("mutants"));
        assert!(c.hint.as_deref().unwrap().contains("--ignore="));
    }

    #[test]
    fn legacy_artifacts_warns_on_cache_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mutmut-cache")).unwrap();
        let c = check_legacy_mutation_artifacts(tmp.path()).expect("should detect");
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains(".mutmut-cache"));
    }
}
