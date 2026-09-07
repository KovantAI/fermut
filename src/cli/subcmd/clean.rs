//! `fermut clean` — wipe the result cache.
//!
//! We deliberately keep the history log intact —
//! historical mutation scores are observational data, not throwaway state.
//! If a user really wants to nuke everything they can `rm -rf .fermut/`
//! themselves.
//!
//! The history filename is configurable (via `--history-path` on
//! `fermut run` or `history_path` in the config file), so the caller
//! resolves the actual path and passes it in — we can't assume the
//! default `history.jsonl`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Where to look for `.fermut/`. Defaults to cwd.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Path of the history log to preserve. Overrides any value
    /// resolved from the config file. Defaults to
    /// `<path>/.fermut/history.jsonl`.
    #[arg(long)]
    pub history_path: Option<PathBuf>,
}

pub fn run(args: CleanArgs) -> Result<()> {
    let resolved = crate::cli::gate::resolve_history_path(&args.path, args.history_path)?;
    clean_cache(&args.path, &resolved)
}

pub fn clean_cache(path: &Path, history_path: &Path) -> Result<()> {
    let cache_dir = path.join(".fermut");
    if !cache_dir.exists() {
        println!("nothing to clean at {}", cache_dir.display());
        return Ok(());
    }

    // Canonicalize once so comparisons survive symlinks / `..` segments.
    // `history_path` may not exist yet (first-run clean); fall back to the
    // lexical path so we still skip a name match when the file is absent.
    let keep_canon = history_path.canonicalize().ok();
    let keep_lexical = history_path;

    let mut removed = Vec::new();
    let mut kept = false;
    for entry in
        std::fs::read_dir(&cache_dir).with_context(|| format!("reading {}", cache_dir.display()))?
    {
        let entry = entry?;
        let p = entry.path();
        let same = match (&keep_canon, p.canonicalize().ok()) {
            (Some(a), Some(b)) => a == &b,
            _ => p == keep_lexical,
        };
        if same {
            kept = true;
            continue;
        }
        if p.is_dir() {
            std::fs::remove_dir_all(&p).with_context(|| format!("removing {}", p.display()))?;
        } else {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
        removed.push(p);
    }

    if removed.is_empty() {
        println!("nothing to clean at {}", cache_dir.display());
    } else {
        for p in &removed {
            println!("removed {}", p.display());
        }
    }
    if !kept {
        // Tidy up an empty `.fermut/` so the user doesn't see a stray dir.
        let _ = std::fs::remove_dir(&cache_dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preserves_default_history_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".fermut");
        fs::create_dir_all(&cache_dir).unwrap();
        let history = cache_dir.join("history.jsonl");
        fs::write(&history, "{}\n").unwrap();
        fs::write(cache_dir.join("cache.bin"), "x").unwrap();

        clean_cache(tmp.path(), &history).unwrap();

        assert!(history.exists());
        assert!(!cache_dir.join("cache.bin").exists());
    }

    /// Regression: custom history filename inside `.fermut/` used to be
    /// silently deleted because `clean_cache` hardcoded `history.jsonl`.
    #[test]
    fn preserves_custom_history_filename_inside_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".fermut");
        fs::create_dir_all(&cache_dir).unwrap();
        let custom = cache_dir.join("myproject-history.jsonl");
        fs::write(&custom, "{}\n").unwrap();
        fs::write(cache_dir.join("cache.bin"), "x").unwrap();

        clean_cache(tmp.path(), &custom).unwrap();

        assert!(custom.exists());
        assert!(!cache_dir.join("cache.bin").exists());
    }

    #[test]
    fn missing_cache_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let history = tmp.path().join(".fermut").join("history.jsonl");
        clean_cache(tmp.path(), &history).unwrap();
        assert!(!tmp.path().join(".fermut").exists());
    }

    #[test]
    fn removes_empty_cache_dir_when_no_history_present() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".fermut");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("cache.bin"), "x").unwrap();
        let history = cache_dir.join("history.jsonl");

        clean_cache(tmp.path(), &history).unwrap();

        assert!(!cache_dir.exists());
    }

    #[test]
    fn history_outside_cache_dir_is_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join(".fermut");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("cache.bin"), "x").unwrap();
        let outside = tmp.path().join("elsewhere-history.jsonl");
        fs::write(&outside, "{}\n").unwrap();

        clean_cache(tmp.path(), &outside).unwrap();

        assert!(outside.exists());
        assert!(!cache_dir.exists());
    }
}
