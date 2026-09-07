//! `fermut migrate` — translate mutmut / cosmic-ray configs into a starter
//! `[tool.fermut]` and (for mutmut) rewrite `# pragma: no mutate` markers
//! to `# fermut: ignore`.
//!
//! The translator is intentionally narrow. mutmut and cosmic-ray both have
//! knobs without a fermut equivalent (celery, pre/post-mutation hooks,
//! `dict_synonyms`, interceptors). For every key it can't map, the migrator
//! prints a "manual review" line so the user sees what didn't translate
//! rather than silently dropping it.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use walkdir::WalkDir;

use super::init;

/// CLI inputs. Mirror of the `Cmd::Migrate` clap arm in `cli/mod.rs`.
#[derive(Debug, Clone)]
pub struct MigrateOpts {
    pub source: MigrateSource,
    pub path: PathBuf,
    /// Override the source config file. Defaults: `pyproject.toml` /
    /// `setup.cfg` for mutmut, `cosmic-ray.toml` for cosmic-ray.
    pub config: Option<PathBuf>,
    pub pyproject: bool,
    pub force: bool,
    pub dry_run: bool,
    pub no_pragma_rewrite: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MigrateSource {
    Mutmut,
    CosmicRay,
}

impl MigrateSource {
    fn label(self) -> &'static str {
        match self {
            Self::Mutmut => "mutmut",
            Self::CosmicRay => "cosmic-ray",
        }
    }
}

/// What a translator returns: a partial fermut config + everything it
/// couldn't map, with the original `key = value` text preserved for the
/// "manual review" report.
#[derive(Debug, Default)]
struct Translation {
    config: FermutConfig,
    /// `(key, reason)` pairs. Reason is a short hint shown to the user.
    unmapped: Vec<(String, String)>,
    /// Notes that aren't tied to a specific key (e.g. "celery distributor —
    /// use --shard in CI").
    notes: Vec<String>,
}

/// Subset of fermut's TOML schema this migrator can populate. Anything not
/// in here is left out of the rendered config (so the user falls back to
/// fermut defaults, which is usually what they want).
#[derive(Debug, Default)]
struct FermutConfig {
    source_root: Option<PathBuf>,
    tests: Option<PathBuf>,
    runner: Option<&'static str>,
    pytest_args: Vec<String>,
    timeout: Option<u64>,
    coverage: Option<PathBuf>,
    isolation: Option<&'static str>,
    /// Glob patterns relative to `source_root`. Populated from cosmic-ray
    /// `excluded-modules` (translated via [`normalize_exclude_pattern`]).
    exclude: Vec<String>,
}

pub fn migrate(opts: MigrateOpts) -> Result<()> {
    let project_root = init::find_project_root(&opts.path);

    println!("fermut migrate — source: {}", opts.source.label());
    println!("  project root : {}", project_root.display());

    let translation = match opts.source {
        MigrateSource::Mutmut => translate_mutmut(&project_root, opts.config.as_deref())?,
        MigrateSource::CosmicRay => translate_cosmic_ray(&project_root, opts.config.as_deref())?,
    };
    if translation.config.is_empty() {
        println!(
            "  (no recognized {} keys found; emitting an empty [tool.fermut] starter)",
            opts.source.label()
        );
    }
    println!();

    let body = render_fermut_body(&translation.config);
    let target = if opts.pyproject {
        project_root.join("pyproject.toml")
    } else {
        project_root.join("fermut.toml")
    };

    if opts.dry_run {
        println!("--dry-run: would write to {}:\n", target.display());
        if opts.pyproject {
            println!("[tool.fermut]");
        }
        print!("{}", body);
        println!();
    } else {
        init::write_config(&target, &body, opts.pyproject, opts.force)?;
        println!("wrote {}", target.display());
    }

    // Pragma rewrite — mutmut only. cosmic-ray has no inline marker
    // convention; its closest analogue is `excluded-modules`, which the
    // cosmic-ray translator auto-maps to the generated `exclude` key.
    if matches!(opts.source, MigrateSource::Mutmut) && !opts.no_pragma_rewrite {
        let scan_root = translation
            .config
            .source_root
            .clone()
            .map(|p| {
                if p.is_absolute() {
                    p
                } else {
                    project_root.join(p)
                }
            })
            .unwrap_or_else(|| project_root.clone());
        let touched = rewrite_pragmas(&scan_root, opts.dry_run)?;
        if touched.is_empty() {
            println!(
                "\npragma rewrite: no `# pragma: no mutate` markers found under {}.",
                scan_root.display()
            );
        } else {
            println!(
                "\npragma rewrite: {} file(s) {}:",
                touched.len(),
                if opts.dry_run {
                    "would change"
                } else {
                    "rewritten"
                }
            );
            for (p, n) in &touched {
                println!(
                    "  {} ({} marker{})",
                    p.display(),
                    n,
                    if *n == 1 { "" } else { "s" }
                );
            }
        }
    }

    if !translation.unmapped.is_empty() || !translation.notes.is_empty() {
        println!("\nmanual review — these did not translate:");
        for (k, why) in &translation.unmapped {
            println!("  - {k}: {why}");
        }
        for n in &translation.notes {
            println!("  - {n}");
        }
    }

    println!("\nnext steps:");
    println!("  - review the generated config, then: fermut doctor");
    println!("  - smoke test:  fermut list");
    println!("  - first run :  fermut run");

    Ok(())
}

// ---------------------------------------------------------------------------
// mutmut
// ---------------------------------------------------------------------------

fn translate_mutmut(project_root: &Path, explicit: Option<&Path>) -> Result<Translation> {
    let (path, table) = match explicit {
        Some(p) => load_table_at(p)?,
        None => find_mutmut_table(project_root)?,
    };
    println!("  source config: {}", path.display());

    let mut out = Translation::default();
    for (key, value) in &table {
        translate_mutmut_key(key, value, &mut out);
    }
    Ok(out)
}

/// mutmut config can live in three places:
///   1. `pyproject.toml` under `[tool.mutmut]`
///   2. `setup.cfg` under `[mutmut]` (INI)
///   3. an explicit file passed via `--config`
///
/// (1) and (3) are TOML; (2) is INI. We return a normalized
/// `BTreeMap<String, String>` of raw values for downstream key handling.
fn find_mutmut_table(project_root: &Path) -> Result<(PathBuf, BTreeMap<String, String>)> {
    let pyproject = project_root.join("pyproject.toml");
    if pyproject.is_file() {
        let text = fs::read_to_string(&pyproject)
            .with_context(|| format!("reading {}", pyproject.display()))?;
        let root: toml::Value =
            toml::from_str(&text).with_context(|| format!("parsing {}", pyproject.display()))?;
        if let Some(tbl) = root
            .get("tool")
            .and_then(|t| t.get("mutmut"))
            .and_then(|m| m.as_table())
        {
            return Ok((pyproject, toml_table_to_strings(tbl)));
        }
    }
    let setup_cfg = project_root.join("setup.cfg");
    if setup_cfg.is_file() {
        let text = fs::read_to_string(&setup_cfg)
            .with_context(|| format!("reading {}", setup_cfg.display()))?;
        if let Some(tbl) = parse_ini_section(&text, "mutmut") {
            return Ok((setup_cfg, tbl));
        }
    }
    Err(anyhow!(
        "no mutmut config found at {} (looked for [tool.mutmut] in pyproject.toml and [mutmut] in setup.cfg). Pass --config <path> to override.",
        project_root.display()
    ))
}

fn load_table_at(path: &Path) -> Result<(PathBuf, BTreeMap<String, String>)> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // Try TOML first, then INI [mutmut] / [cosmic-ray].
    if let Ok(value) = toml::from_str::<toml::Value>(&text) {
        if let Some(tbl) = value.as_table() {
            // If the file is itself a single [tool.mutmut]-style nested table, dig.
            // Only `[tool.mutmut]` — never silently read `[tool.fermut]`, which
            // would feed valid fermut keys back through translate_mutmut_key and
            // emit every one as "unknown mutmut key".
            if let Some(inner) = tbl
                .get("tool")
                .and_then(|t| t.get("mutmut"))
                .and_then(|x| x.as_table())
            {
                return Ok((path.to_path_buf(), toml_table_to_strings(inner)));
            }
            return Ok((path.to_path_buf(), toml_table_to_strings(tbl)));
        }
    }
    if let Some(tbl) = parse_ini_section(&text, "mutmut") {
        return Ok((path.to_path_buf(), tbl));
    }
    Err(anyhow!("could not parse {} as TOML or INI", path.display()))
}

fn translate_mutmut_key(key: &str, raw: &str, out: &mut Translation) {
    // mutmut tolerates both underscored and hyphenated forms in some
    // tools that wrap it. Normalize for matching but preserve the
    // original spelling in the unmapped report.
    let norm = key.replace('-', "_");
    match norm.as_str() {
        "paths_to_mutate" | "source_paths" => {
            // Accept "src/", "src/, lib/", or a TOML list rendered as `["src/"]`.
            // Pick the first non-empty path; the rest are surfaced as a note.
            let first = first_path(raw);
            if let Some(p) = first {
                out.config.source_root = Some(p);
            }
            if extra_paths_present(raw) {
                out.notes.push(format!(
                    "`{key}` had multiple entries ({raw}); fermut takes one source root — review and add inline `# fermut: ignore-file` markers for the rest if needed.",
                ));
            }
        }
        "tests_dir" => {
            out.config.tests = Some(PathBuf::from(unquote(raw)));
        }
        "pytest_add_cli_args_test_selection" => {
            if let Some(p) = first_path(raw) {
                out.config.tests = Some(p);
            }
            if extra_paths_present(raw) {
                out.notes.push(format!(
                    "`pytest_add_cli_args_test_selection` had multiple entries ({raw}); fermut takes one tests path — append the rest to `pytest_args` if you need them on the pytest command line.",
                ));
            }
        }
        "pytest_add_cli_args" => match parse_string_list(raw) {
            Some(args) => out.config.pytest_args.extend(args),
            None => out.unmapped.push((
                key.into(),
                format!(
                    "could not parse `pytest_add_cli_args` value `{raw}`; expected a list of strings. Manual review."
                ),
            )),
        },
        "runner" => {
            apply_runner_command(&unquote(raw), key, out);
        }
        "use_coverage" | "mutate_only_covered_lines" => {
            if as_bool(raw) == Some(true) {
                out.config.coverage = Some(PathBuf::from("coverage.json"));
            }
        }
        // Known mutmut keys without an equivalent — surface them so the
        // user knows what we dropped, with the *original* hyphen/underscore
        // spelling preserved.
        "backup" => out.unmapped.push((
            key.into(),
            "fermut writes per-worker mirrors; nothing to back up. Drop this key.".into(),
        )),
        "dict_synonyms" => out.unmapped.push((
            key.into(),
            "fermut matches names syntactically; no synonym table. Manual review.".into(),
        )),
        "also_copy" => out.unmapped.push((
            key.into(),
            "fermut copies the source tree into each worker mirror automatically. Drop this key."
                .into(),
        )),
        "pre_mutation" | "post_mutation" => out.unmapped.push((
            key.into(),
            "fermut has no per-mutant hook. Fold into test setup or pytest plugin args.".into(),
        )),
        "simple_output" => out.unmapped.push((
            key.into(),
            "use `--format json` for machine output, or default human output.".into(),
        )),
        "mypy" | "type_check_command" => out.unmapped.push((
            key.into(),
            "replaced by the built-in `ty` filter (on by default; `--no-ty-filter` to disable)."
                .into(),
        )),
        _ => out.unmapped.push((
            key.into(),
            format!("unknown mutmut key — value was `{raw}`. Manual review."),
        )),
    }
}

// ---------------------------------------------------------------------------
// cosmic-ray
// ---------------------------------------------------------------------------

fn translate_cosmic_ray(project_root: &Path, explicit: Option<&Path>) -> Result<Translation> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            let candidate = project_root.join("cosmic-ray.toml");
            if !candidate.is_file() {
                return Err(anyhow!(
                    "no cosmic-ray.toml under {}. Pass --config <path> to override.",
                    project_root.display()
                ));
            }
            candidate
        }
    };
    println!("  source config: {}", path.display());

    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let root: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    // cosmic-ray uses a top-level `[cosmic-ray]` table plus nested
    // `[cosmic-ray.distributor]`, `[cosmic-ray.execution-engine]`, etc.
    let cr = root
        .get("cosmic-ray")
        .and_then(|x| x.as_table())
        .ok_or_else(|| {
            anyhow!(
                "{} has no [cosmic-ray] table; this doesn't look like a cosmic-ray config.",
                path.display()
            )
        })?;

    let mut out = Translation::default();
    for (key, value) in cr {
        translate_cosmic_ray_key(key, value, &mut out);
    }
    // cosmic-ray `excluded-modules` patterns are project-root-relative
    // (matched against the same path space as `module-path`). fermut
    // `exclude` patterns are `source_root`-relative. Reconcile after both
    // keys have landed so order in the source TOML doesn't matter.
    normalize_excludes_against_source_root(&mut out);
    Ok(out)
}

/// Strip the `source_root` prefix from every pending exclude pattern.
/// Patterns that don't sit under `source_root` (or that contain a glob
/// segment in the prefix area) are left verbatim and surfaced as a note,
/// because fermut would otherwise silently fail to match them.
fn normalize_excludes_against_source_root(out: &mut Translation) {
    let Some(root) = out.config.source_root.clone() else {
        return;
    };
    let prefix = root.to_string_lossy().trim_end_matches('/').to_string();
    if prefix.is_empty() {
        return;
    }
    let mut warnings: Vec<String> = Vec::new();
    for pat in &mut out.config.exclude {
        if let Some(rest) = pat.strip_prefix(&format!("{prefix}/")) {
            *pat = rest.to_string();
        } else if pat == &prefix {
            // Pattern matches source_root itself — meaningless under fermut's
            // collection root (would exclude everything). Surface it.
            warnings.push(format!(
                "excluded-modules pattern `{pat}` equals source_root; review manually."
            ));
        } else if !pat.starts_with(&prefix) {
            warnings.push(format!(
                "excluded-modules pattern `{pat}` does not sit under source_root `{prefix}/`; kept verbatim. Review if it still matches what you intended."
            ));
        }
    }
    out.notes.extend(warnings);
}

fn translate_cosmic_ray_key(key: &str, value: &toml::Value, out: &mut Translation) {
    match key {
        "module-path" | "module_path" => {
            if let Some(s) = value.as_str() {
                out.config.source_root = Some(PathBuf::from(s));
            }
        }
        "timeout" => {
            // cosmic-ray timeouts are floats (seconds). Round up to whole seconds.
            if let Some(f) = value.as_float() {
                out.config.timeout = Some(f.ceil() as u64);
            } else if let Some(i) = value.as_integer() {
                out.config.timeout = Some(i.max(0) as u64);
            }
        }
        "test-command" | "test_command" => {
            if let Some(s) = value.as_str() {
                apply_runner_command(s, key, out);
            }
        }
        "excluded-modules" | "excluded_modules" => {
            // cosmic-ray accepts a single string or an array; collect both
            // shapes. Patterns are stored verbatim here and normalized
            // against `source_root` after the full table has been walked
            // (see `normalize_excludes_against_source_root`).
            if let Some(arr) = value.as_array() {
                for v in arr {
                    if let Some(s) = v.as_str() {
                        out.config.exclude.push(s.to_string());
                    }
                }
            } else if let Some(s) = value.as_str() {
                out.config.exclude.push(s.to_string());
            }
        }
        "python-version" | "python_version" => {
            out.unmapped.push((
                key.into(),
                "fermut autodetects the active interpreter; no per-config override.".into(),
            ));
        }
        "distributor" => {
            let name = value
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("(unset)");
            match name {
                "local" => out.notes.push(
                    "distributor = local — fermut runs locally by default; no equivalent needed."
                        .into(),
                ),
                _ => out.notes.push(format!(
                    "distributor = `{name}` (e.g. http/celery) — replace with `--shard i/n` in a CI matrix. See Integrations → Sharded full sweep."
                )),
            }
        }
        "execution-engine" | "execution_engine" => {
            let name = value
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("(unset)");
            if name == "celery4" || name == "celery" {
                out.notes.push(
                    "execution-engine = celery — fermut has no broker; use `--shard` for matrix parallelism.".into(),
                );
            } else if name == "local" {
                out.notes
                    .push("execution-engine = local — fermut runs locally by default.".into());
            } else {
                out.notes.push(format!(
                    "execution-engine = `{name}` — manual review; fermut has no celery/http engine analogue."
                ));
            }
        }
        "cloning" => {
            let method = value
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("copy");
            out.config.isolation = Some(match method {
                "copy" => "copy",
                "symlink" | "hardlink" => "hardlink",
                _ => "auto",
            });
        }
        "interceptors" => {
            out.unmapped.push((
                key.into(),
                "no `spor`/interceptor analogue. Use `# fermut: ignore` markers or `--skip-ops`."
                    .into(),
            ));
        }
        "badge" => {
            out.unmapped.push((
                key.into(),
                "no built-in badge generator. Use `fermut run --markdown` + `--trend` for a PR-friendly summary.".into(),
            ));
        }
        other => out.unmapped.push((
            other.into(),
            format!(
                "unrecognized [cosmic-ray] key — value was `{}`. Manual review.",
                value
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

impl FermutConfig {
    fn is_empty(&self) -> bool {
        self.source_root.is_none()
            && self.tests.is_none()
            && self.runner.is_none()
            && self.pytest_args.is_empty()
            && self.timeout.is_none()
            && self.coverage.is_none()
            && self.isolation.is_none()
            && self.exclude.is_empty()
    }
}

/// Parse a shell-style runner command (`"python -m pytest -x -q"`) into
/// `runner` + `pytest_args`. When neither pytest nor unittest is recognized,
/// push an unmapped entry under `source_key` so the command surfaces under
/// "manual review" instead of being silently dropped.
fn apply_runner_command(cmd: &str, source_key: &str, out: &mut Translation) {
    let trimmed = cmd.trim().trim_matches('"');
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let lower: Vec<String> = parts.iter().map(|s| s.to_ascii_lowercase()).collect();

    // Find the first token that names a test runner.
    let (kind, idx) = if let Some(i) = lower.iter().position(|t| t.ends_with("pytest")) {
        ("pytest", i)
    } else if let Some(i) = lower.iter().position(|t| t == "unittest") {
        ("unittest", i)
    } else {
        out.unmapped.push((
            source_key.into(),
            format!(
                "unrecognized runner `{trimmed}` — fermut only knows pytest and unittest. Set `runner` + `pytest_args` manually if applicable."
            ),
        ));
        return;
    };
    out.config.runner = Some(kind);
    // Pass through everything *after* the runner token as pytest_args
    // (works for unittest too — args forward to it the same way).
    // `extend` so multiple sources (e.g. mutmut 1.x `runner` plus
    // mutmut 3.x `pytest_add_cli_args` in the same block) compose.
    out.config
        .pytest_args
        .extend(parts[idx + 1..].iter().map(|s| s.to_string()));
}

fn render_fermut_body(c: &FermutConfig) -> String {
    let mut out = String::new();
    if let Some(p) = &c.source_root {
        out.push_str(&format!("source_root = \"{}\"\n", p.display()));
    }
    if let Some(p) = &c.tests {
        out.push_str(&format!("tests = \"{}\"\n", p.display()));
    }
    if let Some(r) = c.runner {
        out.push_str(&format!("runner = \"{r}\"\n"));
    }
    if !c.pytest_args.is_empty() {
        out.push_str("pytest_args = [");
        for (i, a) in c.pytest_args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('"');
            out.push_str(a);
            out.push('"');
        }
        out.push_str("]\n");
    }
    if let Some(t) = c.timeout {
        out.push_str(&format!("timeout = {t}\n"));
    }
    if let Some(p) = &c.coverage {
        out.push_str(&format!("coverage = \"{}\"\n", p.display()));
    }
    if let Some(iso) = c.isolation {
        out.push_str(&format!("isolation = \"{iso}\"\n"));
    }
    if !c.exclude.is_empty() {
        out.push_str("exclude = [");
        for (i, p) in c.exclude.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('"');
            out.push_str(p);
            out.push('"');
        }
        out.push_str("]\n");
    }
    out
}

/// Rewrite `# pragma: no mutate` → `# fermut: ignore` in every `.py` file
/// under `root`. Returns the list of (path, replacement-count) pairs that
/// changed. In `--dry-run` the files are not written.
fn rewrite_pragmas(root: &Path, dry_run: bool) -> Result<Vec<(PathBuf, usize)>> {
    let mut touched = Vec::new();
    if !root.exists() {
        return Ok(touched);
    }
    for entry in WalkDir::new(root)
        .into_iter()
        // Skip hidden subdirs (e.g. `.venv`, `.git`) but never reject the
        // walk root itself — tempdirs commonly look hidden (`.tmpXXX`).
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension() != Some(OsStr::new("py")) {
            continue;
        }
        let text = match fs::read_to_string(entry.path()) {
            Ok(t) => t,
            // Skip non-UTF8 / unreadable files quietly — they can't contain
            // a Python pragma comment that we'd recognize anyway.
            Err(_) => continue,
        };
        if !text.contains("# pragma: no mutate") {
            continue;
        }
        let next = text.replace("# pragma: no mutate", "# fermut: ignore");
        let n = text.matches("# pragma: no mutate").count();
        if !dry_run {
            fs::write(entry.path(), &next)
                .with_context(|| format!("writing {}", entry.path().display()))?;
        }
        touched.push((entry.path().to_path_buf(), n));
    }
    Ok(touched)
}

fn is_hidden(name: &OsStr) -> bool {
    name.to_str()
        .map(|s| s.starts_with('.') && s != ".")
        .unwrap_or(false)
}

/// Render every value of a TOML table as its TOML string form (`"src/"`,
/// `["a", "b"]`, `60`, `true`). The translator handles the per-key shape
/// downstream; the string view keeps the unmapped-key report readable.
fn toml_table_to_strings(table: &toml::value::Table) -> BTreeMap<String, String> {
    table
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect()
}

/// Tiny line-based INI section extractor. Only handles
/// `key = value` / `key: value` lines; comments via `#` or `;`. Values
/// can span the same line (multi-line continuation common in setup.cfg
/// is folded into one space-joined value).
fn parse_ini_section(text: &str, section: &str) -> Option<BTreeMap<String, String>> {
    let header = format!("[{section}]");
    let mut in_section = false;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut last_key: Option<String> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        let stripped = line.trim_start();
        if stripped.starts_with('#') || stripped.starts_with(';') || stripped.is_empty() {
            continue;
        }
        if stripped.starts_with('[') && stripped.ends_with(']') {
            in_section = stripped == header;
            last_key = None;
            continue;
        }
        if !in_section {
            continue;
        }
        // Continuation line: indented and we have a prior key.
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(k) = &last_key {
                if let Some(v) = out.get_mut(k) {
                    v.push(' ');
                    v.push_str(stripped);
                }
                continue;
            }
        }
        // `key = value` or `key : value`.
        let sep = line.find('=').or_else(|| line.find(':'));
        if let Some(i) = sep {
            let key = line[..i].trim().to_string();
            let val = line[i + 1..].trim().to_string();
            last_key = Some(key.clone());
            out.insert(key, val);
        }
    }
    if !in_section && out.is_empty() {
        // We may have entered + left the section without emitting keys
        // (empty `[mutmut]`), but if nothing matched, fall through and
        // return None so the caller can try the next config source.
        return None;
    }
    Some(out)
}

fn first_path(raw: &str) -> Option<PathBuf> {
    // Accept TOML list form, comma-separated form, or a bare quoted path.
    let v = raw.trim();
    if v.starts_with('[') {
        // toml::Value reparse to get the first element.
        if let Ok(value) = toml::from_str::<toml::Value>(&format!("v = {v}")) {
            if let Some(arr) = value.get("v").and_then(|x| x.as_array()) {
                return arr.first().and_then(|x| x.as_str()).map(PathBuf::from);
            }
        }
        return None;
    }
    // Strip surrounding quotes *before* splitting so `"src/, lib/"`
    // tokenizes as `src/` + `lib/`, not `"src/` + `lib/"`.
    let inner = unquote(v);
    let first = inner.split(',').next()?.trim();
    if first.is_empty() {
        return None;
    }
    Some(PathBuf::from(unquote(first)))
}

/// Parse a TOML list of strings (`["-x", "-q"]`) into `Vec<String>`.
/// Returns `None` if the value is not a list of strings — callers surface
/// that as a manual-review entry.
fn parse_string_list(raw: &str) -> Option<Vec<String>> {
    let v = raw.trim();
    if !v.starts_with('[') {
        return None;
    }
    let parsed: toml::Value = toml::from_str(&format!("v = {v}")).ok()?;
    let arr = parsed.get("v")?.as_array()?;
    arr.iter()
        .map(|x| x.as_str().map(String::from))
        .collect::<Option<Vec<_>>>()
}

fn extra_paths_present(raw: &str) -> bool {
    let v = raw.trim();
    if v.starts_with('[') {
        if let Ok(value) = toml::from_str::<toml::Value>(&format!("v = {v}")) {
            if let Some(arr) = value.get("v").and_then(|x| x.as_array()) {
                return arr.len() > 1;
            }
        }
        return false;
    }
    unquote(v)
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .count()
        > 1
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        return t[1..t.len() - 1].to_string();
    }
    t.to_string()
}

fn as_bool(raw: &str) -> Option<bool> {
    match raw.trim() {
        "true" | "True" | "1" | "yes" | "on" => Some(true),
        "false" | "False" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_table_at_does_not_treat_tool_fermut_as_mutmut() {
        // Regression: load_table_at used to fall back to [tool.fermut] when
        // [tool.mutmut] was absent. A user passing `--config pyproject.toml`
        // with only fermut config would have valid fermut keys (source_root,
        // runner, ...) routed through translate_mutmut_key and reported as
        // "unknown mutmut key".
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(
            &p,
            "[tool.fermut]\nsource_root = \"src/\"\nrunner = \"pytest\"\n",
        )
        .unwrap();
        let (_, tbl) = load_table_at(&p).unwrap();
        assert!(!tbl.contains_key("source_root"));
        assert!(!tbl.contains_key("runner"));
    }

    #[test]
    fn ini_parser_extracts_section() {
        let text = "\
[other]
x = 1

[mutmut]
paths_to_mutate = src/
tests_dir = tests
runner = python -m pytest -x -q
use_coverage = True

[third]
y = 2
";
        let tbl = parse_ini_section(text, "mutmut").unwrap();
        assert_eq!(tbl.get("paths_to_mutate").map(String::as_str), Some("src/"));
        assert_eq!(tbl.get("tests_dir").map(String::as_str), Some("tests"));
        assert_eq!(
            tbl.get("runner").map(String::as_str),
            Some("python -m pytest -x -q")
        );
        assert_eq!(tbl.get("use_coverage").map(String::as_str), Some("True"));
    }

    #[test]
    fn ini_parser_skips_when_section_absent() {
        let text = "[other]\nx = 1\n";
        assert!(parse_ini_section(text, "mutmut").is_none());
    }

    #[test]
    fn runner_command_picks_pytest_and_keeps_args() {
        let mut tx = Translation::default();
        apply_runner_command("python -m pytest -x -q", "runner", &mut tx);
        assert_eq!(tx.config.runner, Some("pytest"));
        assert_eq!(tx.config.pytest_args, vec!["-x", "-q"]);
        assert!(tx.unmapped.is_empty());
    }

    #[test]
    fn runner_command_picks_unittest() {
        let mut tx = Translation::default();
        apply_runner_command("python -m unittest discover", "runner", &mut tx);
        assert_eq!(tx.config.runner, Some("unittest"));
        assert_eq!(tx.config.pytest_args, vec!["discover"]);
        assert!(tx.unmapped.is_empty());
    }

    #[test]
    fn unrecognized_runner_surfaces_under_manual_review() {
        // tox / nose2 / cargo test etc. used to vanish silently. They must
        // land in `unmapped` with the original key so the CLI prints them.
        for cmd in ["tox -e py311", "nose2", "cargo test"] {
            let mut tx = Translation::default();
            apply_runner_command(cmd, "runner", &mut tx);
            assert!(tx.config.runner.is_none(), "{cmd} should not set runner");
            assert!(tx.config.pytest_args.is_empty());
            assert_eq!(tx.unmapped.len(), 1, "{cmd} should be unmapped");
            let (k, why) = &tx.unmapped[0];
            assert_eq!(k, "runner");
            assert!(why.contains(cmd), "reason should quote the command: {why}");
        }
    }

    #[test]
    fn cosmic_ray_unrecognized_test_command_surfaces() {
        let mut tx = Translation::default();
        let v = toml::Value::String("tox -e mutate".into());
        translate_cosmic_ray_key("test-command", &v, &mut tx);
        assert!(tx.config.runner.is_none());
        assert_eq!(tx.unmapped.len(), 1);
        assert_eq!(tx.unmapped[0].0, "test-command");
    }

    #[test]
    fn mutmut_keys_translate_to_fermut_config() {
        let mut tx = Translation::default();
        translate_mutmut_key("paths_to_mutate", "\"src/\"", &mut tx);
        translate_mutmut_key("tests_dir", "\"tests\"", &mut tx);
        translate_mutmut_key("runner", "\"python -m pytest -x -q\"", &mut tx);
        translate_mutmut_key("use_coverage", "true", &mut tx);
        assert_eq!(tx.config.source_root, Some(PathBuf::from("src/")));
        assert_eq!(tx.config.tests, Some(PathBuf::from("tests")));
        assert_eq!(tx.config.runner, Some("pytest"));
        assert_eq!(tx.config.pytest_args, vec!["-x", "-q"]);
        assert_eq!(tx.config.coverage, Some(PathBuf::from("coverage.json")));
        assert!(tx.unmapped.is_empty());
    }

    #[test]
    fn mutmut_3_keys_translate_to_fermut_config() {
        let mut tx = Translation::default();
        translate_mutmut_key("source_paths", "[\"src/\"]", &mut tx);
        translate_mutmut_key(
            "pytest_add_cli_args_test_selection",
            "[\"tests/\"]",
            &mut tx,
        );
        translate_mutmut_key(
            "pytest_add_cli_args",
            "[\"-x\", \"-q\", \"-m\", \"not slow\"]",
            &mut tx,
        );
        translate_mutmut_key("mutate_only_covered_lines", "true", &mut tx);
        assert_eq!(tx.config.source_root, Some(PathBuf::from("src/")));
        assert_eq!(tx.config.tests, Some(PathBuf::from("tests/")));
        assert_eq!(tx.config.pytest_args, vec!["-x", "-q", "-m", "not slow"]);
        assert_eq!(tx.config.coverage, Some(PathBuf::from("coverage.json")));
        assert!(tx.unmapped.is_empty(), "unmapped: {:?}", tx.unmapped);
    }

    #[test]
    fn mutmut_3_type_check_command_surfaces_as_replaced_by_ty() {
        let mut tx = Translation::default();
        translate_mutmut_key(
            "type_check_command",
            "[\"mypy\", \"--strict\", \"src\"]",
            &mut tx,
        );
        let (k, why) = tx.unmapped.first().expect("should be unmapped");
        assert_eq!(k, "type_check_command");
        assert!(why.contains("ty"), "reason should mention ty: {why}");
    }

    #[test]
    fn mutmut_3_multi_test_path_emits_note() {
        let mut tx = Translation::default();
        translate_mutmut_key(
            "pytest_add_cli_args_test_selection",
            "[\"tests/\", \"integration/\"]",
            &mut tx,
        );
        assert_eq!(tx.config.tests, Some(PathBuf::from("tests/")));
        assert!(
            tx.notes
                .iter()
                .any(|n| n.contains("pytest_add_cli_args_test_selection")),
            "notes: {:?}",
            tx.notes
        );
    }

    #[test]
    fn mutmut_mixed_1x_and_3x_block_merges_additively() {
        // Some projects partially port keys to 3.x while still carrying
        // 1.x ones. Both spellings of the same knob should land in the
        // same fermut output, and `runner` + `pytest_add_cli_args` must
        // compose into a single `pytest_args` list rather than clobber.
        let mut tx = Translation::default();
        // Sorted iteration order matches what migrate.rs feeds in via
        // BTreeMap. `pytest_add_cli_args` < `runner` alphabetically.
        translate_mutmut_key("paths_to_mutate", "\"src/\"", &mut tx);
        translate_mutmut_key("pytest_add_cli_args", "[\"-q\"]", &mut tx);
        translate_mutmut_key("runner", "\"python -m pytest -x\"", &mut tx);
        translate_mutmut_key("source_paths", "[\"lib/\"]", &mut tx);
        // 3.x source_paths wins (later in sort order).
        assert_eq!(tx.config.source_root, Some(PathBuf::from("lib/")));
        // Runner picked, and both arg sources contributed.
        assert_eq!(tx.config.runner, Some("pytest"));
        assert_eq!(tx.config.pytest_args, vec!["-q", "-x"]);
        assert!(tx.unmapped.is_empty(), "unmapped: {:?}", tx.unmapped);
    }

    #[test]
    fn mutmut_3_pytest_args_non_list_surfaces_manual_review() {
        // Bare string is malformed for a list-valued key; must surface
        // under manual review rather than silently dropping pytest config.
        let mut tx = Translation::default();
        translate_mutmut_key("pytest_add_cli_args", "\"-x -q\"", &mut tx);
        assert!(tx.config.pytest_args.is_empty());
        assert_eq!(tx.unmapped.len(), 1);
        assert_eq!(tx.unmapped[0].0, "pytest_add_cli_args");
    }

    #[test]
    fn mutmut_unmapped_keys_surface_with_reason() {
        let mut tx = Translation::default();
        translate_mutmut_key("backup", "true", &mut tx);
        translate_mutmut_key("dict_synonyms", "[\"a\", \"b\"]", &mut tx);
        translate_mutmut_key("pre_mutation", "\"./reset.sh\"", &mut tx);
        translate_mutmut_key("nonsense_key", "1", &mut tx);
        let keys: Vec<&str> = tx.unmapped.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"backup"));
        assert!(keys.contains(&"dict_synonyms"));
        assert!(keys.contains(&"pre_mutation"));
        assert!(keys.contains(&"nonsense_key"));
    }

    #[test]
    fn mutmut_multi_path_emits_note() {
        let mut tx = Translation::default();
        translate_mutmut_key("paths_to_mutate", "\"src/, lib/\"", &mut tx);
        // First path wins.
        assert_eq!(tx.config.source_root, Some(PathBuf::from("src/")));
        assert!(tx.notes.iter().any(|n| n.contains("multiple")));
    }

    #[test]
    fn cosmic_ray_keys_translate() {
        let toml_src = r#"
[cosmic-ray]
module-path = "src/"
timeout = 60.0
test-command = "pytest -x -q"
excluded-modules = ["src/migrations/*"]

[cosmic-ray.distributor]
name = "local"

[cosmic-ray.cloning]
method = "copy"
"#;
        let root: toml::Value = toml::from_str(toml_src).unwrap();
        let cr = root.get("cosmic-ray").unwrap().as_table().unwrap();
        let mut tx = Translation::default();
        for (k, v) in cr {
            translate_cosmic_ray_key(k, v, &mut tx);
        }
        assert_eq!(tx.config.source_root, Some(PathBuf::from("src/")));
        assert_eq!(tx.config.timeout, Some(60));
        assert_eq!(tx.config.runner, Some("pytest"));
        assert_eq!(tx.config.pytest_args, vec!["-x", "-q"]);
        assert_eq!(tx.config.isolation, Some("copy"));
        // Pre-normalization: pattern is preserved verbatim from cosmic-ray.
        assert_eq!(tx.config.exclude, vec!["src/migrations/*".to_string()]);
        assert!(!tx.unmapped.iter().any(|(k, _)| k == "excluded-modules"));
        assert!(tx.notes.iter().any(|n| n.contains("local")));
    }

    #[test]
    fn cosmic_ray_excluded_modules_normalized_against_source_root() {
        // After the full-table walk, `excluded-modules` patterns are stripped
        // of their `source_root` prefix so they match what fermut expects
        // (globs relative to the collection root, not the project root).
        let toml_src = r#"
[cosmic-ray]
module-path = "src/"
excluded-modules = ["src/migrations/*", "src/legacy/**/*.py"]
"#;
        let project = tempdir().unwrap();
        let cfg = project.path().join("cosmic-ray.toml");
        fs::write(&cfg, toml_src).unwrap();
        let tx = translate_cosmic_ray(project.path(), Some(&cfg)).unwrap();
        assert_eq!(
            tx.config.exclude,
            vec!["migrations/*".to_string(), "legacy/**/*.py".to_string()]
        );
    }

    #[test]
    fn cosmic_ray_excluded_modules_outside_source_root_warns() {
        // A pattern that doesn't sit under source_root is kept verbatim but
        // a note is emitted so the user knows fermut may not match it.
        let toml_src = r#"
[cosmic-ray]
module-path = "src/"
excluded-modules = ["vendor/*"]
"#;
        let project = tempdir().unwrap();
        let cfg = project.path().join("cosmic-ray.toml");
        fs::write(&cfg, toml_src).unwrap();
        let tx = translate_cosmic_ray(project.path(), Some(&cfg)).unwrap();
        assert_eq!(tx.config.exclude, vec!["vendor/*".to_string()]);
        assert!(tx
            .notes
            .iter()
            .any(|n| n.contains("does not sit under source_root")));
    }

    #[test]
    fn cosmic_ray_excluded_modules_accepts_bare_string() {
        // cosmic-ray accepts either an array or a single string for
        // `excluded-modules`. Both shapes round-trip into `exclude`.
        let mut tx = Translation::default();
        tx.config.source_root = Some(PathBuf::from("src/"));
        let v = toml::Value::String("src/migrations/*".into());
        translate_cosmic_ray_key("excluded-modules", &v, &mut tx);
        normalize_excludes_against_source_root(&mut tx);
        assert_eq!(tx.config.exclude, vec!["migrations/*".to_string()]);
    }

    #[test]
    fn cosmic_ray_celery_engine_surfaces_note() {
        let toml_src = r#"
[cosmic-ray]
module-path = "src/"

[cosmic-ray.execution-engine]
name = "celery4"
"#;
        let root: toml::Value = toml::from_str(toml_src).unwrap();
        let cr = root.get("cosmic-ray").unwrap().as_table().unwrap();
        let mut tx = Translation::default();
        for (k, v) in cr {
            translate_cosmic_ray_key(k, v, &mut tx);
        }
        assert!(tx.notes.iter().any(|n| n.contains("celery")));
    }

    #[test]
    fn render_emits_only_set_keys() {
        let c = FermutConfig {
            source_root: Some(PathBuf::from("src")),
            tests: Some(PathBuf::from("tests")),
            runner: Some("pytest"),
            pytest_args: vec!["-x".into(), "-q".into()],
            timeout: Some(60),
            coverage: Some(PathBuf::from("coverage.json")),
            isolation: Some("copy"),
            exclude: vec!["migrations/**".into(), "legacy/*.py".into()],
        };
        let body = render_fermut_body(&c);
        assert!(body.contains("source_root = \"src\""));
        assert!(body.contains("tests = \"tests\""));
        assert!(body.contains("runner = \"pytest\""));
        assert!(body.contains("pytest_args = [\"-x\", \"-q\"]"));
        assert!(body.contains("timeout = 60"));
        assert!(body.contains("coverage = \"coverage.json\""));
        assert!(body.contains("isolation = \"copy\""));
        assert!(body.contains("exclude = [\"migrations/**\", \"legacy/*.py\"]"));
    }

    #[test]
    fn render_empty_config_is_empty_string() {
        let body = render_fermut_body(&FermutConfig::default());
        assert!(body.is_empty());
    }

    #[test]
    fn pragma_rewrite_replaces_and_counts() {
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("pkg");
        fs::create_dir_all(&nested).unwrap();
        let a = nested.join("a.py");
        let b = nested.join("b.py");
        fs::write(&a, "x = 1  # pragma: no mutate\ny = 2\n").unwrap();
        fs::write(
            &b,
            "z = 3  # pragma: no mutate\nw = 4  # pragma: no mutate\n",
        )
        .unwrap();
        let touched = rewrite_pragmas(tmp.path(), false).unwrap();
        assert_eq!(touched.len(), 2);
        let by_path: BTreeMap<_, _> = touched.into_iter().collect();
        assert_eq!(by_path.get(&a).copied(), Some(1));
        assert_eq!(by_path.get(&b).copied(), Some(2));
        let after_a = fs::read_to_string(&a).unwrap();
        let after_b = fs::read_to_string(&b).unwrap();
        assert!(after_a.contains("# fermut: ignore"));
        assert!(!after_a.contains("# pragma: no mutate"));
        assert_eq!(after_b.matches("# fermut: ignore").count(), 2);
    }

    #[test]
    fn pragma_rewrite_dry_run_does_not_touch_files() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("a.py");
        let original = "x = 1  # pragma: no mutate\n";
        fs::write(&p, original).unwrap();
        let touched = rewrite_pragmas(tmp.path(), true).unwrap();
        assert_eq!(touched.len(), 1);
        let now = fs::read_to_string(&p).unwrap();
        assert_eq!(now, original);
    }

    #[test]
    fn migrate_pyproject_force_replaces_existing_tool_fermut() {
        // Regression: --force used to skip the duplicate-table guard but
        // never strip the old `[tool.fermut]`, leaving two headers and a
        // TOML the parser would reject on the next fermut invocation.
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("pyproject.toml");
        fs::write(
            &p,
            "[project]\nname = \"x\"\n\n\
             [tool.mutmut]\npaths_to_mutate = \"src/\"\ntests_dir = \"tests\"\n\n\
             [tool.fermut]\njobs = 2\n",
        )
        .unwrap();
        let opts = MigrateOpts {
            source: MigrateSource::Mutmut,
            path: tmp.path().to_path_buf(),
            config: None,
            pyproject: true,
            force: true,
            dry_run: false,
            no_pragma_rewrite: true,
        };
        migrate(opts).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert_eq!(got.matches("[tool.fermut]").count(), 1);
        assert!(!got.contains("jobs = 2"));
        assert!(toml::from_str::<toml::Value>(&got).is_ok());
    }

    #[test]
    fn pragma_rewrite_skips_non_py_and_hidden_dirs() {
        let tmp = tempdir().unwrap();
        let py = tmp.path().join("a.py");
        let txt = tmp.path().join("a.txt");
        fs::write(&py, "x = 1  # pragma: no mutate\n").unwrap();
        fs::write(&txt, "x = 1  # pragma: no mutate\n").unwrap();
        let hidden = tmp.path().join(".venv");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("c.py"), "y = 2  # pragma: no mutate\n").unwrap();
        let touched = rewrite_pragmas(tmp.path(), true).unwrap();
        let paths: Vec<PathBuf> = touched.into_iter().map(|(p, _)| p).collect();
        assert!(paths.contains(&py));
        assert!(!paths.iter().any(|p| p == &txt));
        assert!(!paths.iter().any(|p| p.starts_with(&hidden)));
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct MigrateArgs {
    /// Which tool to migrate from.
    #[arg(value_enum)]
    pub(crate) from: crate::cli::MigrateSourceCli,

    /// Where to start the project-root walk. Defaults to cwd.
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

    /// Explicit source config file. Defaults: `pyproject.toml` /
    /// `setup.cfg` for mutmut, `cosmic-ray.toml` for cosmic-ray.
    #[arg(long, value_name = "PATH")]
    pub(crate) config: Option<std::path::PathBuf>,

    /// Write `[tool.fermut]` into `pyproject.toml` instead of a
    /// standalone `fermut.toml`.
    #[arg(long)]
    pub(crate) pyproject: bool,

    /// Overwrite an existing `fermut.toml` or `[tool.fermut]` block.
    #[arg(long)]
    pub(crate) force: bool,

    /// Print what would be written and which files would be rewritten,
    /// without touching the filesystem.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Skip rewriting `# pragma: no mutate` → `# fermut: ignore`
    /// (mutmut only; no-op for cosmic-ray).
    #[arg(long)]
    pub(crate) no_pragma_rewrite: bool,
}
