//! `fermut init` — config wizard.
//!
//! Detects project layout (source/tests dirs), available tools (pytest,
//! unittest, ty, ruff), and repo size, then writes a `fermut.toml` (or
//! `[tool.fermut]` block in `pyproject.toml`) with sensible defaults.
//! Optionally drops a PR-gate GitHub Actions workflow.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

const GHA_PR_GATE: &str = include_str!("../../../examples/github-actions/pr-gate.yml");

/// All the inputs `fermut init` needs. Kept in a struct so the dispatch
/// site in `cli/mod.rs` can build it from clap and we can test the writer
/// without going through the binary.
#[derive(Debug, Clone)]
pub struct InitOpts {
    pub path: PathBuf,
    pub pyproject: bool,
    pub force: bool,
    pub with_gha: bool,
    pub with_coverage: bool,
    pub profile: Option<Profile>,
    pub dry_run: bool,
}

/// Curated config presets tuned for a specific way of running fermut. Each
/// profile is a thin overlay on top of the auto-detected config: detection
/// still picks source/tests/runner/ty, the profile just opinionates the
/// scenario-specific knobs (operator subset, diff/coverage wiring,
/// timeouts, hypothesis seed).
///
/// To switch profiles re-run `fermut init --profile <name> --force`.
/// Picking the wrong profile is non-destructive — the generated TOML is
/// plain text, hand-edit anything you don't like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Pre-merge CI gate. Diff-restricted, coverage-narrowed, small op set,
    /// short timeout, pinned Hypothesis seed for reproducible survivor lists.
    PrGate,
    /// Cron / nightly full sweep. Every op (including experimental),
    /// generous timeout, no diff filter — catches drift between gates.
    Nightly,
    /// Local dev loop. Sub-samples mutants for fast feedback, short timeout,
    /// no coverage so it works before you've generated `coverage.json`.
    Local,
    /// Library-author workflow. Every stable op, no experimental noise,
    /// pinned Hypothesis seed for reproducibility.
    Library,
}

impl Profile {
    pub const ALL: &'static [Profile] = &[
        Profile::PrGate,
        Profile::Nightly,
        Profile::Local,
        Profile::Library,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::PrGate => "pr-gate",
            Self::Nightly => "nightly",
            Self::Local => "local",
            Self::Library => "library",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::PrGate => "Pre-merge CI gate: diff-only + coverage + narrow ops. Fastest.",
            Self::Nightly => "Cron full sweep: every op + experimental, no diff filter.",
            Self::Local => "Dev loop: 25% sample of mutants, short timeout, no coverage.",
            Self::Library => "Library: all stable ops, no experimental, pinned Hypothesis seed.",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.name() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoSize {
    Small,
    Medium,
    Large,
}

impl RepoSize {
    fn classify(py_file_count: usize) -> Self {
        match py_file_count {
            0..=50 => Self::Small,
            51..=500 => Self::Medium,
            _ => Self::Large,
        }
    }

    fn recommended_ops(self) -> Option<Vec<&'static str>> {
        match self {
            // Small repos: run every stable op; signal-to-noise is fine.
            Self::Small => None,
            Self::Medium => None,
            // Large repos: PR-gate subset keeps wall-clock reasonable.
            Self::Large => Some(vec![
                "arith-op-swap",
                "compare-op-swap",
                "boundary-shift",
                "return-value-to-none",
            ]),
        }
    }
}

#[derive(Debug)]
struct Detection {
    project_root: PathBuf,
    source_root: PathBuf,
    tests: Option<PathBuf>,
    runner: &'static str,
    ty_available: bool,
    coverage: CoverageDetection,
    package_manager: PackageManager,
    py_file_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageDetection {
    /// `coverage` or `pytest-cov` is declared in project deps, or the
    /// `coverage` binary is on PATH, or a `.coveragerc` / `[tool.coverage.*]`
    /// table exists. Safe to wire `coverage = "coverage.json"` into config.
    Present,
    Absent,
}

/// Heuristic guess at the project's package manager, used only to print a
/// matching install command in the "next steps" output. Not used to mutate
/// the project — we never run the install ourselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageManager {
    Uv,
    Poetry,
    Pdm,
    Pipenv,
    Pip,
}

impl PackageManager {
    fn install_coverage_cmd(self) -> &'static str {
        match self {
            Self::Uv => "uv add --dev coverage",
            Self::Poetry => "poetry add --group dev coverage",
            Self::Pdm => "pdm add -dG dev coverage",
            Self::Pipenv => "pipenv install --dev coverage",
            Self::Pip => "pip install coverage",
        }
    }
}

pub fn init(opts: InitOpts) -> Result<()> {
    let detection = detect(&opts.path)?;
    // Wire `coverage = "coverage.json"` when coverage is already part of
    // the project, OR when the user explicitly asked for it. The latter
    // assumes they'll install coverage themselves — we print the command.
    let wire_coverage_base = detection.coverage == CoverageDetection::Present || opts.with_coverage;
    let toml_body = render_config(&detection, wire_coverage_base, opts.profile);
    // What the rendered config actually contains, after profile overrides.
    let wire_coverage = match opts.profile {
        Some(Profile::PrGate) => true,
        Some(Profile::Nightly) | Some(Profile::Local) => false,
        _ => wire_coverage_base,
    };
    let size = RepoSize::classify(detection.py_file_count);

    print_detection_summary(&detection, &opts, size);
    write_outputs(&detection, &toml_body, &opts)?;
    print_next_steps(&detection, wire_coverage);

    Ok(())
}

/// Print the "fermut init — detected:" block: resolved roots, runner, ty,
/// coverage, package manager, repo size, and the effective profile.
fn print_detection_summary(detection: &Detection, opts: &InitOpts, size: RepoSize) {
    println!("fermut init — detected:");
    println!("  project root : {}", detection.project_root.display());
    println!(
        "  source root  : {}",
        rel(&detection.source_root, &detection.project_root)
    );
    println!(
        "  tests        : {}",
        detection
            .tests
            .as_ref()
            .map(|p| rel(p, &detection.project_root))
            .unwrap_or_else(|| "(none found)".into())
    );
    println!("  runner       : {}", detection.runner);
    println!(
        "  ty filter    : {}",
        if detection.ty_available {
            "on"
        } else {
            "off (ty not on PATH)"
        }
    );
    println!(
        "  coverage     : {}",
        coverage_status(detection.coverage, opts.with_coverage, opts.profile)
    );
    println!("  pkg manager  : {:?}", detection.package_manager);
    println!("  python files : {} ({:?})", detection.py_file_count, size);
    println!(
        "  profile      : {}",
        match opts.profile {
            Some(p) => format!("{} — {}", p.name(), p.description()),
            None => format!("auto (size-based: {size:?})"),
        }
    );
    println!();
}

/// Write the rendered config (fermut.toml or pyproject) and, when requested,
/// the GHA workflow — or, under `--dry-run`, just print what would be written.
fn write_outputs(detection: &Detection, toml_body: &str, opts: &InitOpts) -> Result<()> {
    let target = if opts.pyproject {
        detection.project_root.join("pyproject.toml")
    } else {
        detection.project_root.join("fermut.toml")
    };

    if opts.dry_run {
        println!("--dry-run: would write to {}:\n", target.display());
        println!("{}", toml_body);
    } else {
        write_config(&target, toml_body, opts.pyproject, opts.force)?;
        println!("wrote {}", target.display());
    }

    if opts.with_gha {
        let gha_path = detection.project_root.join(".github/workflows/fermut.yml");
        if opts.dry_run {
            println!(
                "--dry-run: would write GHA workflow to {}",
                gha_path.display()
            );
        } else {
            write_gha(&gha_path, opts.force)?;
            println!("wrote {}", gha_path.display());
        }
    }

    Ok(())
}

/// Print the actionable "next steps:" tail — missing tests/ty warnings, the
/// coverage-generation recipe, and the smoke-test / first-run commands.
fn print_next_steps(detection: &Detection, wire_coverage: bool) {
    println!();
    println!("next steps:");
    if detection.tests.is_none() {
        println!("  - no tests directory detected; set `tests = \"path/to/tests\"` in the config");
    }
    if !detection.ty_available {
        println!("  - install ty for the type-aware mutant pre-filter:  uv tool install ty");
    }
    if wire_coverage {
        println!(
            "  - generate coverage with per-test contexts before each run:\n      pytest --cov=src --cov-context=test\n      coverage json -o coverage.json --show-contexts\n    (requires pytest-cov; `coverage run --context=LABEL` won't produce per-test contexts)"
        );
        if detection.coverage == CoverageDetection::Absent {
            println!(
                "  - install coverage first (no dep detected):  {}",
                detection.package_manager.install_coverage_cmd()
            );
        }
    } else {
        println!(
            "  - coverage selection narrows per-mutant test sets; enable with --with-coverage,\n    or install first:  {}",
            detection.package_manager.install_coverage_cmd()
        );
    }
    println!(
        "  - smoke test:  fermut list {}",
        rel(&detection.source_root, &detection.project_root)
    );
    println!(
        "  - first run :  fermut run {}",
        rel(&detection.source_root, &detection.project_root)
    );
}

fn detect(start: &Path) -> Result<Detection> {
    let project_root = find_project_root(start);
    let source_root = detect_source_root(&project_root);
    let tests = detect_tests(&project_root);
    let runner = if which("pytest").is_some() {
        "pytest"
    } else {
        "unittest"
    };
    let ty_available = which("ty").is_some();
    let coverage = detect_coverage(&project_root);
    let package_manager = detect_package_manager(&project_root);
    let py_file_count = count_py_files(&source_root);

    Ok(Detection {
        project_root,
        source_root,
        tests,
        runner,
        ty_available,
        coverage,
        package_manager,
        py_file_count,
    })
}

fn detect_coverage(root: &Path) -> CoverageDetection {
    detect_coverage_impl(root, which("coverage").is_some())
}

// Pure variant so tests can pin the PATH-lookup outcome instead of racing
// with whatever happens to be installed on the runner.
fn detect_coverage_impl(root: &Path, coverage_on_path: bool) -> CoverageDetection {
    if root.join(".coveragerc").is_file() {
        return CoverageDetection::Present;
    }
    if pyproject_mentions(root, &["coverage", "pytest-cov", "tool.coverage"]) {
        return CoverageDetection::Present;
    }
    if lockfile_mentions(root, &["coverage", "pytest-cov", "pytest_cov"]) {
        return CoverageDetection::Present;
    }
    if coverage_on_path {
        return CoverageDetection::Present;
    }
    CoverageDetection::Absent
}

fn detect_package_manager(root: &Path) -> PackageManager {
    if root.join("uv.lock").is_file() {
        return PackageManager::Uv;
    }
    if root.join("poetry.lock").is_file() {
        return PackageManager::Poetry;
    }
    if root.join("pdm.lock").is_file() {
        return PackageManager::Pdm;
    }
    if root.join("Pipfile").is_file() || root.join("Pipfile.lock").is_file() {
        return PackageManager::Pipenv;
    }
    PackageManager::Pip
}

/// Cheap substring scan of `pyproject.toml`. We deliberately don't parse the
/// TOML — we only need to know whether any of these names appear somewhere
/// in the file (deps, optional-deps, tool tables). False positives (e.g. a
/// comment that mentions "coverage") are acceptable here: the worst case is
/// we wire up the `coverage` filter when the user doesn't actually have it,
/// which they'll discover the first time they run fermut.
fn pyproject_mentions(root: &Path, needles: &[&str]) -> bool {
    let Ok(text) = fs::read_to_string(root.join("pyproject.toml")) else {
        return false;
    };
    needles.iter().any(|n| text.contains(n))
}

fn lockfile_mentions(root: &Path, needles: &[&str]) -> bool {
    for name in ["uv.lock", "poetry.lock", "pdm.lock", "Pipfile.lock"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            if needles.iter().any(|n| text.contains(n)) {
                return true;
            }
        }
    }
    false
}

pub(super) fn find_project_root(start: &Path) -> PathBuf {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for ancestor in start.ancestors() {
        if ancestor.join("pyproject.toml").is_file() || ancestor.join("setup.cfg").is_file() {
            return ancestor.to_path_buf();
        }
    }
    start
}

fn detect_source_root(root: &Path) -> PathBuf {
    // Prefer `src/` (PEP 518 convention), then a top-level package dir
    // (any dir containing `__init__.py`), else fall back to the root.
    // Sort entries and skip test-dir names so the pick is stable across
    // filesystems and never points at the test tree.
    if root.join("src").is_dir() {
        return root.join("src");
    }
    if let Ok(entries) = fs::read_dir(root) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if !p.is_dir() || !p.join("__init__.py").is_file() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "tests" | "test") {
                continue;
            }
            return p;
        }
    }
    root.to_path_buf()
}

fn detect_tests(root: &Path) -> Option<PathBuf> {
    for candidate in ["tests", "test"] {
        let p = root.join(candidate);
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

fn count_py_files(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "py"))
        .count()
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{bin}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn rel(p: &Path, base: &Path) -> String {
    p.strip_prefix(base)
        .map(|r| {
            let s = r.display().to_string();
            // `strip_prefix` of equal paths yields an empty string, which
            // would serialize as `source_root = ""` — not equivalent to `.`
            // for the config reader.
            if s.is_empty() {
                ".".into()
            } else {
                s
            }
        })
        .unwrap_or_else(|_| p.display().to_string())
}

fn render_config(d: &Detection, wire_coverage: bool, profile: Option<Profile>) -> String {
    let source_root = rel(&d.source_root, &d.project_root);
    let mut out = String::new();
    if let Some(p) = profile {
        out.push_str(&format!(
            "# fermut profile: {} — {}\n# re-run `fermut init --profile <name> --force` to switch.\n\n",
            p.name(),
            p.description()
        ));
    }
    out.push_str(&format!("source_root = \"{source_root}\"\n"));
    if let Some(t) = &d.tests {
        out.push_str(&format!("tests = \"{}\"\n", rel(t, &d.project_root)));
    }
    out.push_str(&format!("runner = \"{}\"\n", d.runner));
    out.push_str(&format!("ty_filter = {}\n", d.ty_available));
    out.push_str(&format!("timeout = {}\n", profile_timeout(profile)));

    // Coverage: profile wins (pr-gate forces on, local/nightly force off);
    // detection-based wiring fills the gap otherwise.
    let coverage_on = match profile {
        Some(Profile::PrGate) => true,
        Some(Profile::Nightly) | Some(Profile::Local) => false,
        _ => wire_coverage,
    };
    if coverage_on {
        out.push_str("coverage = \"coverage.json\"\n");
    }

    if let Some(p) = profile {
        write_profile_keys(&mut out, p);
    } else if let Some(ops) = RepoSize::classify(d.py_file_count).recommended_ops() {
        write_ops(&mut out, &ops);
    }
    out
}

fn coverage_status(
    detection: CoverageDetection,
    with_coverage: bool,
    profile: Option<Profile>,
) -> String {
    match profile {
        Some(Profile::PrGate) => "wired (profile pr-gate requires coverage)".into(),
        Some(Profile::Nightly) => "off (profile nightly runs full sweep, no coverage)".into(),
        Some(Profile::Local) => "off (profile local: skip coverage for fast feedback)".into(),
        Some(Profile::Library) => match (detection, with_coverage) {
            (CoverageDetection::Present, _) => {
                "wired (profile library honors detected coverage)".into()
            }
            (CoverageDetection::Absent, true) => {
                "wired (profile library + --with-coverage; install separately)".into()
            }
            (CoverageDetection::Absent, false) => {
                "off (profile library defers to project; none found)".into()
            }
        },
        None => match (detection, with_coverage) {
            (CoverageDetection::Present, _) => "wired (coverage detected in project)".into(),
            (CoverageDetection::Absent, true) => {
                "wired (--with-coverage; install separately)".into()
            }
            (CoverageDetection::Absent, false) => "off (no coverage dependency found)".into(),
        },
    }
}

/// Per-profile timeout. Tuned so PR gates fail fast, nightly soaks slow
/// mutants, local runs stay snappy. Library default matches the global
/// 30s, just spelled out for readability.
fn profile_timeout(profile: Option<Profile>) -> u64 {
    match profile {
        Some(Profile::PrGate) | Some(Profile::Local) => 15,
        Some(Profile::Nightly) => 60,
        Some(Profile::Library) => 30,
        None => 30,
    }
}

fn write_profile_keys(out: &mut String, p: Profile) {
    match p {
        Profile::PrGate => {
            out.push_str("diff_only = \"main\"\n");
            out.push_str("hypothesis_seed = 12345\n");
            write_ops(
                out,
                &[
                    "arith-op-swap",
                    "compare-op-swap",
                    "boundary-shift",
                    "return-value-to-none",
                ],
            );
        }
        Profile::Nightly => {
            out.push_str("experimental = true\n");
            // No ops allowlist on nightly — full sweep is the point.
        }
        Profile::Local => {
            out.push_str("sample = 0.25\n");
            out.push_str("sample_seed = 0\n");
        }
        Profile::Library => {
            out.push_str("experimental = false\n");
            out.push_str("hypothesis_seed = 12345\n");
        }
    }
}

fn write_ops(out: &mut String, ops: &[&str]) {
    out.push_str("ops = [");
    for (i, op) in ops.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(op);
        out.push('"');
    }
    out.push_str("]\n");
}

pub(super) fn write_config(target: &Path, body: &str, pyproject: bool, force: bool) -> Result<()> {
    if pyproject {
        write_pyproject(target, body, force)
    } else {
        write_fermut_toml(target, body, force)
    }
}

fn write_fermut_toml(target: &Path, body: &str, force: bool) -> Result<()> {
    if target.exists() && !force {
        return Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            target.display()
        ));
    }
    fs::write(target, body).with_context(|| format!("writing {}", target.display()))
}

fn write_pyproject(target: &Path, body: &str, force: bool) -> Result<()> {
    let existing = if target.exists() {
        fs::read_to_string(target).with_context(|| format!("reading {}", target.display()))?
    } else {
        String::new()
    };
    if existing.contains("[tool.fermut]") && !force {
        return Err(anyhow!(
            "{} already has [tool.fermut]; pass --force to replace",
            target.display()
        ));
    }
    let mut next = if existing.contains("[tool.fermut]") {
        strip_existing_table(&existing, "[tool.fermut]")
    } else {
        existing
    };
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str("[tool.fermut]\n");
    next.push_str(body);
    fs::write(target, next).with_context(|| format!("writing {}", target.display()))
}

/// Remove a `[header]` block and any nested `[header.child]` subtables, up to
/// the next unrelated `[...]` line or EOF. Whitespace/comment lines immediately
/// before the header are also dropped so the file doesn't grow blank lines on
/// repeated `--force` runs.
fn strip_existing_table(text: &str, header: &str) -> String {
    // `header` looks like `"[tool.fermut]"`; child headers start with
    // `"[tool.fermut."`. Strip the trailing `]` and append `.` to form the
    // child prefix.
    let child_prefix = format!("{}.", &header[..header.len() - 1]);
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == header {
            // Drop trailing whitespace lines we just emitted.
            while out.ends_with("\n\n") {
                out.pop();
            }
            // Consume the body, plus any `[header.child]` subtables and
            // their bodies, until an unrelated table header or EOF.
            while let Some(peek) = lines.peek() {
                if is_toml_table_header(peek) && !peek.trim_start().starts_with(&child_prefix) {
                    break;
                }
                lines.next();
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// True when `line` is a TOML table header (`[name]` or `[[array.name]]`),
/// allowing the trailing whitespace and `# comment` that TOML permits on
/// the same line. The naive `starts_with('[') && ends_with(']')` test
/// misidentifies `[tool.other]  # note` as not-a-header, which makes
/// `strip_existing_table` walk past the section boundary and delete lines
/// from the next table.
fn is_toml_table_header(line: &str) -> bool {
    let s = line.trim_start();
    if !s.starts_with('[') {
        return false;
    }
    // Find the matching `]`, honouring TOML's basic ("...") and literal
    // ('...') string syntaxes inside dotted keys like `[a."b.c"]`.
    let bytes = s.as_bytes();
    let mut depth: usize = 0;
    let mut in_basic = false;
    let mut in_literal = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_basic {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_basic = false;
            }
        } else if in_literal {
            if c == b'\'' {
                in_literal = false;
            }
        } else {
            match c {
                b'"' => in_basic = true,
                b'\'' => in_literal = true,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        let rest = s[i + 1..].trim_start();
                        return rest.is_empty() || rest.starts_with('#');
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn write_gha(target: &Path, force: bool) -> Result<()> {
    if target.exists() && !force {
        return Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            target.display()
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(target, GHA_PR_GATE).with_context(|| format!("writing {}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn classify_repo_size() {
        assert_eq!(RepoSize::classify(10), RepoSize::Small);
        assert_eq!(RepoSize::classify(200), RepoSize::Medium);
        assert_eq!(RepoSize::classify(10_000), RepoSize::Large);
        assert!(RepoSize::Small.recommended_ops().is_none());
        assert!(RepoSize::Large.recommended_ops().is_some());
    }

    #[test]
    fn detect_walks_up_to_pyproject() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let root = find_project_root(&nested);
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn detect_prefers_src_dir() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        assert_eq!(detect_source_root(tmp.path()), tmp.path().join("src"));
    }

    #[test]
    fn detect_falls_back_to_package_dir() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("__init__.py"), "").unwrap();
        assert_eq!(detect_source_root(tmp.path()), pkg);
    }

    #[test]
    fn detect_source_root_skips_tests_package() {
        // Project with `tests/__init__.py` alongside the real package must
        // never resolve to the test tree, regardless of `read_dir` order.
        let tmp = tempdir().unwrap();
        let tests = tmp.path().join("tests");
        let pkg = tmp.path().join("mypkg");
        fs::create_dir(&tests).unwrap();
        fs::create_dir(&pkg).unwrap();
        fs::write(tests.join("__init__.py"), "").unwrap();
        fs::write(pkg.join("__init__.py"), "").unwrap();
        assert_eq!(detect_source_root(tmp.path()), pkg);
    }

    #[test]
    fn detect_source_root_is_deterministic_across_packages() {
        // Two real package dirs: sorted order picks the lexicographically
        // first one, every run, on every filesystem.
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("aaa_pkg");
        let z = tmp.path().join("zzz_pkg");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&z).unwrap();
        fs::write(a.join("__init__.py"), "").unwrap();
        fs::write(z.join("__init__.py"), "").unwrap();
        assert_eq!(detect_source_root(tmp.path()), a);
    }

    fn fake_detection() -> Detection {
        Detection {
            project_root: PathBuf::from("/proj"),
            source_root: PathBuf::from("/proj/src"),
            tests: Some(PathBuf::from("/proj/tests")),
            runner: "pytest",
            ty_available: true,
            coverage: CoverageDetection::Absent,
            package_manager: PackageManager::Pip,
            py_file_count: 10,
        }
    }

    #[test]
    fn render_config_emits_expected_keys() {
        let body = render_config(&fake_detection(), false, None);
        assert!(body.contains("source_root = \"src\""));
        assert!(body.contains("tests = \"tests\""));
        assert!(body.contains("runner = \"pytest\""));
        assert!(body.contains("ty_filter = true"));
        assert!(!body.contains("ops = ["));
        assert!(!body.contains("coverage = "));
        assert!(!body.contains("fermut profile:"));
    }

    #[test]
    fn render_config_uses_dot_when_source_root_equals_project_root() {
        // Fallback case: no `src/`, no top-level package — source_root ==
        // project_root. Must serialize as "." not "".
        let mut d = fake_detection();
        d.source_root = d.project_root.clone();
        let body = render_config(&d, false, None);
        assert!(body.contains("source_root = \".\""));
        assert!(!body.contains("source_root = \"\""));
    }

    #[test]

    fn render_config_wires_coverage_when_requested() {
        let body = render_config(&fake_detection(), true, None);
        assert!(body.contains("coverage = \"coverage.json\""));
    }

    #[test]
    fn render_config_adds_op_subset_for_large_repos() {
        let mut d = fake_detection();
        d.tests = None;
        d.runner = "unittest";
        d.ty_available = false;
        d.py_file_count = 5_000;
        let body = render_config(&d, false, None);
        assert!(body.contains("ops = [\"arith-op-swap\""));
        assert!(!body.contains("tests = "));
    }

    #[test]
    fn render_config_pr_gate_profile() {
        let body = render_config(&fake_detection(), false, Some(Profile::PrGate));
        assert!(body.contains("# fermut profile: pr-gate"));
        assert!(body.contains("diff_only = \"main\""));
        assert!(body.contains("coverage = \"coverage.json\""));
        assert!(body.contains("hypothesis_seed = 12345"));
        assert!(body.contains("timeout = 15"));
        assert!(body.contains("ops = [\"arith-op-swap\""));
    }

    #[test]
    fn render_config_nightly_profile() {
        let body = render_config(&fake_detection(), false, Some(Profile::Nightly));
        assert!(body.contains("experimental = true"));
        assert!(body.contains("timeout = 60"));
        assert!(!body.contains("ops = ["));
        assert!(!body.contains("diff_only"));
        assert!(!body.contains("coverage = "));
    }

    #[test]
    fn render_config_local_profile() {
        let body = render_config(&fake_detection(), false, Some(Profile::Local));
        assert!(body.contains("sample = 0.25"));
        assert!(body.contains("sample_seed = 0"));
        assert!(body.contains("timeout = 15"));
        assert!(!body.contains("coverage = "));
    }

    #[test]
    fn render_config_library_profile() {
        let body = render_config(&fake_detection(), false, Some(Profile::Library));
        assert!(body.contains("experimental = false"));
        assert!(body.contains("hypothesis_seed = 12345"));
        assert!(body.contains("timeout = 30"));
    }

    #[test]
    fn pr_gate_profile_forces_coverage_even_without_detection() {
        // Detection says coverage absent, no --with-coverage; profile should
        // still wire it because the pr-gate scenario depends on it.
        let mut d = fake_detection();
        d.coverage = CoverageDetection::Absent;
        let body = render_config(&d, false, Some(Profile::PrGate));
        assert!(body.contains("coverage = \"coverage.json\""));
    }

    #[test]
    fn nightly_profile_drops_coverage_even_when_detected() {
        // Nightly explicitly clears coverage even when detected, because the
        // nightly scenario wants a full sweep over the whole repo.
        let mut d = fake_detection();
        d.coverage = CoverageDetection::Present;
        let body = render_config(&d, true, Some(Profile::Nightly));
        assert!(!body.contains("coverage = "));
    }

    #[test]
    fn profile_parse_round_trip() {
        for p in Profile::ALL {
            assert_eq!(Profile::parse(p.name()), Some(*p));
        }
        assert_eq!(Profile::parse("nonsense"), None);
    }

    #[test]
    fn detect_coverage_from_pyproject_dep() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\ndependencies = [\"coverage\"]\n",
        )
        .unwrap();
        assert_eq!(detect_coverage(tmp.path()), CoverageDetection::Present);
    }

    #[test]
    fn detect_coverage_from_pytest_cov_in_lockfile() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        fs::write(tmp.path().join("uv.lock"), "name = \"pytest-cov\"\n").unwrap();
        assert_eq!(detect_coverage(tmp.path()), CoverageDetection::Present);
    }

    #[test]
    fn detect_coverage_from_coveragerc() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join(".coveragerc"), "[run]\n").unwrap();
        assert_eq!(detect_coverage(tmp.path()), CoverageDetection::Present);
    }

    #[test]
    fn detect_coverage_absent_when_nothing_matches() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        // Pin the PATH branch to false so the result reflects only the
        // project-level signals, not whatever happens to be installed.
        assert_eq!(
            detect_coverage_impl(tmp.path(), false),
            CoverageDetection::Absent
        );
    }

    #[test]
    fn detect_coverage_present_when_only_on_path() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_coverage_impl(tmp.path(), true),
            CoverageDetection::Present
        );
    }

    #[test]
    fn detect_package_manager_prefers_uv_lock() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("uv.lock"), "").unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Uv);
    }

    #[test]
    fn detect_package_manager_falls_back_to_pip() {
        let tmp = tempdir().unwrap();
        assert_eq!(detect_package_manager(tmp.path()), PackageManager::Pip);
    }

    #[test]
    fn install_coverage_cmd_per_manager() {
        assert!(PackageManager::Uv.install_coverage_cmd().starts_with("uv "));
        assert!(PackageManager::Poetry
            .install_coverage_cmd()
            .starts_with("poetry "));
        assert!(PackageManager::Pdm
            .install_coverage_cmd()
            .starts_with("pdm "));
        assert!(PackageManager::Pipenv
            .install_coverage_cmd()
            .starts_with("pipenv "));
        assert!(PackageManager::Pip
            .install_coverage_cmd()
            .starts_with("pip "));
    }

    #[test]
    fn write_fermut_toml_refuses_overwrite_without_force() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("fermut.toml");
        fs::write(&p, "existing").unwrap();
        assert!(write_fermut_toml(&p, "new", false).is_err());
        write_fermut_toml(&p, "new", true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");
    }

    #[test]
    fn write_pyproject_appends_to_clean_file() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(&p, "[project]\nname = \"x\"\n").unwrap();
        write_pyproject(&p, "jobs = 4\n", false).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert!(got.contains("[project]"));
        assert!(got.contains("[tool.fermut]"));
        assert!(got.contains("jobs = 4"));
    }

    #[test]
    fn write_pyproject_refuses_existing_tool_fermut_without_force() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(&p, "[tool.fermut]\njobs = 2\n").unwrap();
        assert!(write_pyproject(&p, "jobs = 4\n", false).is_err());
    }

    #[test]
    fn write_pyproject_force_strips_nested_subtables() {
        // [tool.fermut.operators] must be removed too, otherwise --force
        // leaves an orphaned subtable above the freshly appended block.
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(
            &p,
            "[project]\nname = \"x\"\n\n\
             [tool.fermut]\njobs = 2\n\n\
             [tool.fermut.operators]\nenabled = [\"a\"]\n\n\
             [tool.other]\nk = 1\n",
        )
        .unwrap();
        write_pyproject(&p, "jobs = 8\n", true).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert!(got.contains("[project]"));
        assert!(got.contains("[tool.other]"));
        assert!(got.contains("jobs = 8"));
        assert!(!got.contains("[tool.fermut.operators]"));
        assert!(!got.contains("enabled = [\"a\"]"));
        assert!(!got.contains("jobs = 2"));
        // Exactly one [tool.fermut] header — the appended one.
        assert_eq!(got.matches("[tool.fermut]").count(), 1);
    }

    #[test]
    fn write_pyproject_force_replaces_existing_block() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(
            &p,
            "[project]\nname = \"x\"\n\n[tool.fermut]\njobs = 2\n\n[tool.other]\nk = 1\n",
        )
        .unwrap();
        write_pyproject(&p, "jobs = 8\n", true).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert!(got.contains("[project]"));
        assert!(got.contains("[tool.other]"));
        assert!(got.contains("jobs = 8"));
        assert!(!got.contains("jobs = 2"));
    }

    #[test]
    fn is_toml_table_header_accepts_trailing_comment() {
        assert!(is_toml_table_header("[tool.other]"));
        assert!(is_toml_table_header("[tool.other]  # ours"));
        assert!(is_toml_table_header("  [tool.other]\t# trailing"));
        assert!(is_toml_table_header("[[arr.of.tables]]"));
        assert!(is_toml_table_header(r#"[tool."dotted.key"]"#));
        assert!(!is_toml_table_header("key = 1"));
        assert!(!is_toml_table_header("# just a comment"));
        assert!(!is_toml_table_header(""));
        assert!(!is_toml_table_header("[unterminated"));
    }

    #[test]
    fn write_pyproject_force_preserves_next_table_with_inline_comment() {
        // Regression: strip_existing_table used to walk past a header that
        // had a trailing `# comment`, eating lines from [tool.other].
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(
            &p,
            "[tool.fermut]\njobs = 2\n\n[tool.other]  # keep me\nkept_key = 1\n",
        )
        .unwrap();
        write_pyproject(&p, "jobs = 8\n", true).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert!(got.contains("[tool.other]"));
        assert!(got.contains("kept_key = 1"));
        assert!(!got.contains("jobs = 2"));
    }

    #[test]
    fn count_py_files_walks_recursively() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/x.py"), "").unwrap();
        fs::write(tmp.path().join("a/y.py"), "").unwrap();
        fs::write(tmp.path().join("a/z.txt"), "").unwrap();
        assert_eq!(count_py_files(tmp.path()), 2);
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct InitArgs {
    /// Where to start the project-root walk. Defaults to cwd.
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    /// Write `[tool.fermut]` into `pyproject.toml` instead of a
    /// standalone `fermut.toml`.
    #[arg(long)]
    pub(crate) pyproject: bool,

    /// Overwrite an existing `fermut.toml` or `[tool.fermut]` block.
    #[arg(long)]
    pub(crate) force: bool,

    /// Also drop a PR-gate workflow at `.github/workflows/fermut.yml`.
    #[arg(long)]
    pub(crate) with_gha: bool,

    /// Wire `coverage = "coverage.json"` into the config even when no
    /// coverage dependency is detected in the project. Use when you'll
    /// install `coverage` yourself — `init` prints the install command
    /// matching your package manager.
    #[arg(long)]
    pub(crate) with_coverage: bool,

    /// Pre-seed the config with one of fermut's curated profiles:
    /// `pr-gate` (CI gate), `nightly` (full sweep), `local` (dev loop),
    /// or `library` (lib authors). Without `--profile`, init falls back
    /// to a size-based heuristic.
    #[arg(long, value_name = "NAME")]
    pub(crate) profile: Option<String>,

    /// Print the profile catalogue (name, description, key overrides)
    /// and exit. Nothing is written.
    #[arg(long)]
    pub(crate) list_profiles: bool,

    /// Print what would be written without touching the filesystem.
    #[arg(long)]
    pub(crate) dry_run: bool,
}
