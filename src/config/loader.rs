//! Config-file discovery and parse.
//!
//! Walks the source path's ancestors looking for `fermut.toml` (preferred)
//! or `pyproject.toml` containing `[tool.fermut]`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use super::file::{FileConfig, PyprojectRoot};

/// Env var: when set to a truthy value, skip the walk-up config search and
/// behave as if no `fermut.toml` / `pyproject.toml` were present.
///
/// Threat model: running `fermut` inside a freshly cloned, untrusted repo
/// loads whatever `fermut.toml` the author committed at the project root
/// (or any ancestor). The config can't execute code directly, but it can
/// redirect `tests_path`, `pytest_args`, `cache_path`, etc. Users who
/// audit before running can flip `FERMUT_NO_CONFIG=1` to opt out of the
/// implicit discovery and rely on explicit CLI flags only.
const NO_CONFIG_ENV: &str = "FERMUT_NO_CONFIG";

fn no_config_requested() -> bool {
    matches!(
        std::env::var(NO_CONFIG_ENV)
            .ok()
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase()),
        Some(ref v) if matches!(v.as_str(), "1" | "true" | "yes" | "on")
    )
}

/// A loaded `FileConfig` together with the directory it was rooted in.
/// Relative paths inside the config file resolve against `base_dir`.
pub struct LoadedConfig {
    pub file: FileConfig,
    pub base_dir: PathBuf,
    pub source: ConfigSource,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigSource {
    None,
    FermutToml,
    Pyproject,
}

impl LoadedConfig {
    /// Walk `start` and its ancestors looking for `fermut.toml`, then any
    /// `pyproject.toml` containing `[tool.fermut]`. Returns an empty config
    /// anchored at `start` if neither is found.
    pub fn load(start: &Path) -> Result<Self> {
        let abs = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        if no_config_requested() {
            info!(
                "{NO_CONFIG_ENV} is set; skipping fermut.toml / pyproject.toml \
                 discovery and using defaults + CLI flags only"
            );
            return Ok(Self {
                file: FileConfig::default(),
                base_dir: abs,
                source: ConfigSource::None,
            });
        }
        for dir in abs.ancestors() {
            let fermut_path = dir.join("fermut.toml");
            if fermut_path.is_file() {
                let raw = std::fs::read_to_string(&fermut_path)
                    .with_context(|| format!("reading {}", fermut_path.display()))?;
                let file: FileConfig = toml::from_str(&raw)
                    .with_context(|| format!("parsing {}", fermut_path.display()))?;
                // Log discovered path so users see which config governs
                // this run. Critical when running in an unfamiliar repo
                // — the walk-up can pick up a config from an ancestor
                // directory the user didn't realize controlled fermut.
                info!(path = %fermut_path.display(), "config: loaded fermut.toml");
                return Ok(Self {
                    file,
                    base_dir: dir.to_path_buf(),
                    source: ConfigSource::FermutToml,
                });
            }
            let pyproject_path = dir.join("pyproject.toml");
            if pyproject_path.is_file() {
                let raw = std::fs::read_to_string(&pyproject_path)
                    .with_context(|| format!("reading {}", pyproject_path.display()))?;
                let root: PyprojectRoot = toml::from_str(&raw)
                    .with_context(|| format!("parsing {}", pyproject_path.display()))?;
                if let Some(file) = root.tool.fermut {
                    info!(
                        path = %pyproject_path.display(),
                        "config: loaded [tool.fermut] from pyproject.toml"
                    );
                    return Ok(Self {
                        file,
                        base_dir: dir.to_path_buf(),
                        source: ConfigSource::Pyproject,
                    });
                }
            }
        }
        Ok(Self {
            file: FileConfig::default(),
            base_dir: abs,
            source: ConfigSource::None,
        })
    }

    /// Resolve `p` against the config base dir. Absolute paths are returned
    /// unchanged.
    pub fn resolve_path(&self, p: PathBuf) -> PathBuf {
        if p.is_absolute() {
            p
        } else {
            self.base_dir.join(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_empty_when_no_config_found() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = LoadedConfig::load(tmp.path()).unwrap();
        assert!(matches!(loaded.source, ConfigSource::None));
        assert!(loaded.file.source_root.is_none());
    }

    #[test]
    fn load_picks_fermut_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "jobs = 7\n").unwrap();
        let loaded = LoadedConfig::load(tmp.path()).unwrap();
        assert!(matches!(loaded.source, ConfigSource::FermutToml));
        assert_eq!(loaded.file.jobs, Some(7));
    }

    #[test]
    fn load_falls_back_to_pyproject() {
        let tmp = tempfile::tempdir().unwrap();
        let pyproject = "[tool.fermut]\njobs = 9\n";
        std::fs::write(tmp.path().join("pyproject.toml"), pyproject).unwrap();
        let loaded = LoadedConfig::load(tmp.path()).unwrap();
        assert!(matches!(loaded.source, ConfigSource::Pyproject));
        assert_eq!(loaded.file.jobs, Some(9));
    }

    #[test]
    fn fermut_toml_wins_over_pyproject() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "jobs = 1\n").unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[tool.fermut]\njobs = 2\n",
        )
        .unwrap();
        let loaded = LoadedConfig::load(tmp.path()).unwrap();
        assert_eq!(loaded.file.jobs, Some(1));
    }

    #[test]
    fn load_walks_up_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("fermut.toml"), "jobs = 3\n").unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let loaded = LoadedConfig::load(&nested).unwrap();
        assert_eq!(loaded.file.jobs, Some(3));
        assert_eq!(
            loaded.base_dir.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_path_handles_absolute_and_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = LoadedConfig {
            file: FileConfig::default(),
            base_dir: tmp.path().to_path_buf(),
            source: ConfigSource::None,
        };
        let resolved = loaded.resolve_path(PathBuf::from("src/calculator.py"));
        assert_eq!(resolved, tmp.path().join("src/calculator.py"));
        #[cfg(windows)]
        let abs = PathBuf::from("C:\\tmp\\absolute.py");
        #[cfg(not(windows))]
        let abs = PathBuf::from("/tmp/absolute.py");
        assert!(abs.is_absolute());
        assert_eq!(loaded.resolve_path(abs.clone()), abs);
    }
}
