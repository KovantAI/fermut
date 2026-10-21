//! `fermut install-skills` — copy the bundled agent skill into a project.
//!
//! The skill (`fermut-mutation-testing`) lives in `plugins/fermut/skills/` and
//! ships two ways: as a Claude Code plugin served from the repo's marketplace,
//! and embedded in this binary. The embedded copy is what this subcommand
//! writes, so a `pip install fermut` user gets a skill that names exactly the
//! flags and subcommands of the binary they installed. Needs no interpreter.
//!
//! Default target is `.claude/skills/` under the current directory (Claude
//! Code's project skills, committed so teammates get them too); `--user` writes
//! `~/.claude/skills/`, `--agents` writes `.agents/skills/` (Codex and other
//! Agent Skills readers), and `--dir` names any directory. An installed skill
//! whose files differ from the bundled copy is left alone unless `--force`;
//! `--force` overwrites the shipped files but never deletes files the user
//! added to a skill directory.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// One shipped file: `(skill, path relative to the skill dir, contents)`.
type SkillFile = (&'static str, &'static str, &'static [u8]);

macro_rules! skill_file {
    ($skill:literal, $path:literal) => {
        (
            $skill,
            $path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/plugins/fermut/skills/",
                $skill,
                "/",
                $path
            )),
        )
    };
}

/// Every file the skills need at runtime. `evals/` is skill-development only
/// and stays out. `bundle_matches_plugin_dir` (test) fails if this list drifts
/// from `plugins/fermut/skills/`.
const FILES: &[SkillFile] = &[skill_file!("fermut-mutation-testing", "SKILL.md")];

#[derive(clap::Args, Debug)]
pub(crate) struct InstallSkillsArgs {
    /// Install for every project: `~/.claude/skills/` (or `~/.agents/skills/`
    /// with `--agents`).
    #[arg(long)]
    pub(crate) user: bool,

    /// Write `.agents/skills/` instead of `.claude/skills/`, for Codex and
    /// other agents that read the Agent Skills layout.
    #[arg(long)]
    pub(crate) agents: bool,

    /// Install into DIR instead (each skill lands in `DIR/<skill>/`).
    /// Overrides `--user` / `--agents`.
    #[arg(long, value_name = "DIR")]
    pub(crate) dir: Option<PathBuf>,

    /// Overwrite installed skills that differ from the bundled copy. Files
    /// you added to a skill directory are kept.
    #[arg(long)]
    pub(crate) force: bool,
}

/// Dispatch handler: resolve the target directory, install, and exit 1 when a
/// differing skill was left alone so scripts notice the stale copy.
pub(crate) fn run(args: InstallSkillsArgs) -> Result<()> {
    let cwd = crate::cli::current_project_root();
    let target = target_dir(&cwd, args.dir.as_deref(), args.user, args.agents)?;
    let mut out = std::io::stdout().lock();
    let skipped = install(&mut out, &target, args.force)?;
    if skipped > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Where `install-skills` writes, from its flags (`--dir` wins, then `--user`,
/// then `--agents`; the default is `cwd`'s `.claude/skills/`).
fn target_dir(cwd: &Path, dir: Option<&Path>, user: bool, agents: bool) -> Result<PathBuf> {
    if let Some(dir) = dir {
        return Ok(dir.to_path_buf());
    }
    let sub = if agents { ".agents" } else { ".claude" };
    let base = if user {
        home_dir().context("cannot locate the home directory (HOME / USERPROFILE unset)")?
    } else {
        cwd.to_path_buf()
    };
    Ok(base.join(sub).join("skills"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Names of the bundled skills, in install order.
fn skill_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = FILES.iter().map(|(s, _, _)| *s).collect();
    names.dedup();
    names
}

/// State of one skill under the target directory.
#[derive(Debug, PartialEq, Eq)]
enum Status {
    Missing,
    UpToDate,
    Differs,
}

fn status(target: &Path, skill: &str) -> Status {
    let root = target.join(skill);
    if !root.exists() {
        return Status::Missing;
    }
    let same = FILES
        .iter()
        .filter(|(s, _, _)| *s == skill)
        .all(|(_, rel, body)| std::fs::read(root.join(rel)).is_ok_and(|on_disk| on_disk == *body));
    if same {
        Status::UpToDate
    } else {
        Status::Differs
    }
}

/// Install the bundled skills into `target`. Returns how many skills were
/// skipped because they differ and `force` was not given.
fn install(out: &mut impl std::io::Write, target: &Path, force: bool) -> Result<usize> {
    if target.exists() && !target.is_dir() {
        bail!("{} exists and is not a directory", target.display());
    }
    let version = env!("CARGO_PKG_VERSION");
    let mut skipped = 0;
    for skill in skill_names() {
        let root = target.join(skill);
        let verb = match status(target, skill) {
            Status::UpToDate => {
                writeln!(out, "  {skill}: up to date")?;
                continue;
            }
            Status::Differs if !force => {
                writeln!(
                    out,
                    "  {skill}: differs from the fermut {version} copy, left alone \
                     (--force to overwrite)"
                )?;
                skipped += 1;
                continue;
            }
            Status::Differs => "updated",
            Status::Missing => "installed",
        };
        for (_, rel, body) in FILES.iter().filter(|(s, _, _)| *s == skill) {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        }
        writeln!(out, "  {skill}: {verb}")?;
    }
    writeln!(
        out,
        "fermut {version} skills in {}. Claude Code picks up project and user \
         skills live; if they don't show up, start a new session.",
        target.display()
    )?;
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/fermut")
    }

    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    #[test]
    fn bundle_matches_plugin_dir() {
        // Every shipped file is embedded, and nothing is embedded that the
        // plugin no longer ships. evals/ and eval scratch workspaces are
        // skill-development only.
        let base = plugin_dir().join("skills");
        let mut on_disk = Vec::new();
        for entry in std::fs::read_dir(&base).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() && !name.ends_with("-workspace") {
                walk(&path, &base, &mut on_disk);
            }
        }
        on_disk.retain(|p| p.split('/').nth(1) != Some("evals"));
        on_disk.retain(|p| !p.ends_with(".DS_Store"));
        on_disk.sort();
        let mut embedded: Vec<String> = FILES
            .iter()
            .map(|(s, rel, _)| format!("{s}/{rel}"))
            .collect();
        embedded.sort();
        assert_eq!(embedded, on_disk, "update FILES in install_skills.rs");
    }

    #[test]
    fn plugin_version_matches_package() {
        // Claude Code only ships a plugin update when `version` changes, so it
        // must be bumped with every release.
        let manifest = plugin_dir().join(".claude-plugin/plugin.json");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap();
        assert_eq!(
            doc["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "bump `version` in plugins/fermut/.claude-plugin/plugin.json"
        );
    }

    #[test]
    fn install_writes_then_reports_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        assert_eq!(install(&mut out, dir.path(), false).unwrap(), 0);
        for (skill, rel, body) in FILES {
            assert_eq!(
                std::fs::read(dir.path().join(skill).join(rel)).unwrap(),
                *body
            );
        }
        for skill in skill_names() {
            assert_eq!(status(dir.path(), skill), Status::UpToDate);
        }
        assert_eq!(install(&mut out, dir.path(), false).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("fermut-mutation-testing: installed"),
            "{text}"
        );
        assert!(
            text.contains("fermut-mutation-testing: up to date"),
            "{text}"
        );
    }

    #[test]
    fn edited_skill_is_kept_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        install(&mut out, dir.path(), false).unwrap();
        let skill_md = dir.path().join("fermut-mutation-testing/SKILL.md");
        let extra = dir.path().join("fermut-mutation-testing/notes.md");
        std::fs::write(&skill_md, "local edit").unwrap();
        std::fs::write(&extra, "mine").unwrap();

        assert_eq!(install(&mut out, dir.path(), false).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), "local edit");

        assert_eq!(install(&mut out, dir.path(), true).unwrap(), 0);
        assert_eq!(
            status(dir.path(), "fermut-mutation-testing"),
            Status::UpToDate
        );
        // --force overwrites shipped files only; the user's own file survives.
        assert_eq!(std::fs::read_to_string(&extra).unwrap(), "mine");
    }

    #[test]
    fn target_that_is_a_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("skills");
        std::fs::write(&file, "x").unwrap();
        let err = install(&mut Vec::new(), &file, false).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn target_dir_precedence() {
        let cwd = Path::new("/proj");
        let explicit = Path::new("/some/where");
        assert_eq!(
            target_dir(cwd, Some(explicit), true, true).unwrap(),
            explicit
        );
        assert_eq!(
            target_dir(cwd, None, false, false).unwrap(),
            cwd.join(".claude/skills")
        );
        assert_eq!(
            target_dir(cwd, None, false, true).unwrap(),
            cwd.join(".agents/skills")
        );
    }
}
