//! Pretty-JSON serialization of a `Report`.

use std::path::Path;

use anyhow::{Context, Result};

use crate::report::Report;

impl Report {
    pub fn write_json(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self).context("serializing JSON")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
