//! Per-mutant kill-set records for the higher-order-mutant experiment.
//!
//! Normal `fermut run` keeps only a Killed/Survived verdict per mutant — under
//! `-x` that is all pytest reveals (it stops at the first failing test). The
//! subsuming/higher-order-mutant theory needs the *kill-set* `K(m)`: the full
//! set of covering tests that fail on the mutant. An SSHOM is defined purely by
//! kill-set containment (`K(h) ⊆ K(f1) ∩ K(f2)`), so measuring whether SSHOMs
//! exist in Python requires this data.
//!
//! When `--record-kill-sets <path>` is set the pytest runner drops `-x`, runs
//! every coverage-selected test, and emits one [`KillSetRecord`] per mutant it
//! actually runs (cache hits are skipped — pair with `--no-cache`). The engine
//! drains the records after the run and writes them as JSONL to `path`; an
//! offline analysis step pairs FOMs and classifies SSHOMs from there.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// One mutant's kill-set, as recorded by a no-`-x` run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KillSetRecord {
    /// The mutant's stable id (as in the normal report), so records join back to
    /// the mutant catalogue.
    pub mutant_id: String,
    /// `source_root`-relative file path (forward-slashed), matching the
    /// kill-order key convention so records are portable across checkouts.
    pub file: String,
    /// Mutation operator name (e.g. `arith-op-swap`).
    pub operator: String,
    /// 1-based line of the mutated token.
    pub line: u32,
    /// Verdict this run produced: `killed`, `survived`, `timed_out`, or `error`.
    /// `kill_set` is only meaningful when `killed`; the others carry it empty and
    /// exist so the analysis can tell "survived (K=∅)" from "not measured".
    pub status: &'static str,
    /// Covering tests that failed on this mutant — its kill-set `K(m)`. Empty
    /// unless `status == "killed"`. Deduped, in first-seen order.
    pub kill_set: Vec<String>,
}

/// Append kill-set records to `path` as JSONL (one JSON object per line),
/// creating or truncating the file. Called once by the engine after the run.
pub fn write_jsonl(path: &Path, records: &[KillSetRecord]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating kill-set dir {}", parent.display()))?;
        }
    }
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating kill-set file {}", path.display()))?;
    let mut w = std::io::BufWriter::new(file);
    for r in records {
        let line = serde_json::to_string(r).context("serializing kill-set record")?;
        writeln!(w, "{line}").context("writing kill-set record")?;
    }
    w.flush().context("flushing kill-set file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_json_object_per_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/kill-sets.jsonl");
        let records = vec![
            KillSetRecord {
                mutant_id: "m1".into(),
                file: "src/a.py".into(),
                operator: "arith-op-swap".into(),
                line: 7,
                status: "killed",
                kill_set: vec!["tests/t.py::test_x".into(), "tests/t.py::test_y".into()],
            },
            KillSetRecord {
                mutant_id: "m2".into(),
                file: "src/a.py".into(),
                operator: "compare-op-swap".into(),
                line: 9,
                status: "survived",
                kill_set: vec![],
            },
        ];
        write_jsonl(&path, &records).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record");
        // Round-trips as JSON with the fields the analysis reads.
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["mutant_id"], "m1");
        assert_eq!(v["status"], "killed");
        assert_eq!(v["kill_set"].as_array().unwrap().len(), 2);
        let v2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v2["status"], "survived");
        assert!(v2["kill_set"].as_array().unwrap().is_empty());
    }
}
