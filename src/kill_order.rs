//! Smart test ordering — historical "which test killed this kind of mutant".
//!
//! The per-mutant pytest run passes coverage-selected test node ids as
//! positional args under `-x` (stop at first failure). pytest runs them in the
//! given order, so putting a test that has *historically* killed this
//! `(file, operator)` first makes the kill — and thus the whole run — return
//! sooner. This module is the pure core: the persisted kill counts, the
//! ordering, and the pytest-output parse that learns the killer. No I/O beyond
//! an advisory JSON sidecar; losing it costs a slow run, never correctness.
//!
//! Key is `(project-relative file, operator name)` — robust to line shifts, so
//! a normal edit doesn't discard the history. Value is a per-node-id kill count;
//! ordering is count-descending, ties keeping the caller's (coverage) order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default kill-order sidecar location, beside the cache/history under
/// `<artifact_root>/.fermut/`. Overridable via `kill_order_path` in config.
pub fn default_kill_order_path(artifact_root: &Path) -> PathBuf {
    artifact_root.join(".fermut").join("kill-order.json")
}

/// `{ file : { operator : { test_nodeid : kill_count } } }`. Serialized
/// transparently, so the JSON is the bare nested map.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KillOrder(HashMap<String, HashMap<String, HashMap<String, u64>>>);

/// One learned kill: test `nodeid` killed a `operator` mutant in `file`.
/// Accumulated in a shared sink during a run, folded into the on-disk
/// [`KillOrder`] once at the end.
#[derive(Debug, Clone)]
pub struct KillRecord {
    pub file: String,
    pub operator: String,
    pub nodeid: String,
}

impl KillOrder {
    /// Load the sidecar. Any problem (missing, unreadable, malformed) yields an
    /// empty store — the history is advisory, so a bad file must never fail a
    /// run, only forfeit ordering until the next save rewrites it.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).context("serializing kill-order")?;
        std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Increment the kill count for `(file, operator, nodeid)`.
    pub fn record(&mut self, file: &str, operator: &str, nodeid: &str) {
        *self
            .0
            .entry(file.to_string())
            .or_default()
            .entry(operator.to_string())
            .or_default()
            .entry(nodeid.to_string())
            .or_insert(0) += 1;
    }

    /// Fold a batch of records (from a run's shared sink) into the store.
    pub fn apply(&mut self, records: &[KillRecord]) {
        for r in records {
            self.record(&r.file, &r.operator, &r.nodeid);
        }
    }

    /// Reorder `ids` so historically-frequent killers for `(file, operator)`
    /// come first. Stable: equal counts (including the many zeros for ids with
    /// no history) keep their input order, so a cold store is a no-op and a
    /// warm one only lifts proven killers. Never adds or drops an id.
    pub fn order(&self, ids: &[String], file: &str, operator: &str) -> Vec<String> {
        let counts = self.0.get(file).and_then(|ops| ops.get(operator));
        let mut indexed: Vec<(usize, &String)> = ids.iter().enumerate().collect();
        indexed.sort_by(|a, b| {
            let ca = counts.and_then(|c| c.get(a.1)).copied().unwrap_or(0);
            let cb = counts.and_then(|c| c.get(b.1)).copied().unwrap_or(0);
            // Count descending; ties broken by original index (stable).
            cb.cmp(&ca).then_with(|| a.0.cmp(&b.0))
        });
        indexed.into_iter().map(|(_, id)| id.clone()).collect()
    }
}

/// The node id of the first `FAILED <nodeid>` line in pytest `-rf` output — the
/// test that killed the mutant (under `-x` the first failure is the only one).
/// `None` when there is no failure line (a survivor, or output we can't read).
/// pytest prints `FAILED <nodeid> - <reason>`; we take the first token after
/// `FAILED `, so the trailing reason is ignored. The nodeid itself may contain
/// spaces (parametrized ids like `test_f[a b]`), so we split on the ` - ` reason
/// separator rather than whitespace; a line with no separator is all nodeid.
pub fn parse_first_failed(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("FAILED ")?;
        let nodeid = rest.split_once(" - ").map_or(rest, |(id, _)| id).trim();
        (!nodeid.is_empty()).then(|| nodeid.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_first_failed ---

    #[test]
    fn parse_takes_first_failed_nodeid_dropping_reason() {
        let out = "F\n\
                   =short test summary=\n\
                   FAILED tests/test_x.py::test_add - AssertionError: 3 != 4\n";
        assert_eq!(
            parse_first_failed(out).as_deref(),
            Some("tests/test_x.py::test_add")
        );
    }

    #[test]
    fn parse_none_when_no_failure() {
        assert_eq!(parse_first_failed("1 passed in 0.01s\n"), None);
        assert_eq!(parse_first_failed(""), None);
    }

    #[test]
    fn parse_takes_the_first_of_several() {
        let out = "FAILED tests/a.py::t1 - E\nFAILED tests/b.py::t2 - E\n";
        assert_eq!(parse_first_failed(out).as_deref(), Some("tests/a.py::t1"));
    }

    #[test]
    fn parse_ignores_a_bare_or_malformed_line() {
        // A line that merely contains FAILED but isn't the summary form.
        assert_eq!(parse_first_failed("this FAILED somewhere\n"), None);
        // `FAILED` with nothing after → no nodeid.
        assert_eq!(parse_first_failed("FAILED \n"), None);
    }

    #[test]
    fn parse_keeps_parametrized_nodeid_with_spaces() {
        // Parametrized ids can contain spaces; the ` - ` reason separator, not
        // whitespace, bounds the nodeid. A truncated key would never match the
        // coverage-selected id, silently defeating ordering for param tests.
        let out = "FAILED tests/t.py::test_add[a b] - AssertionError\n";
        assert_eq!(
            parse_first_failed(out).as_deref(),
            Some("tests/t.py::test_add[a b]")
        );
        // No reason on the line → whole rest is the nodeid.
        assert_eq!(
            parse_first_failed("FAILED tests/t.py::test_x[a b]\n").as_deref(),
            Some("tests/t.py::test_x[a b]")
        );
    }

    // --- order ---

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn order_is_noop_on_empty_history() {
        let ko = KillOrder::default();
        let input = ids(&["t::a", "t::b", "t::c"]);
        assert_eq!(ko.order(&input, "src/f.py", "arith-op-swap"), input);
    }

    #[test]
    fn order_lifts_the_frequent_killer_first() {
        let mut ko = KillOrder::default();
        // t::c killed twice, t::a once, t::b never.
        ko.record("src/f.py", "arith-op-swap", "t::c");
        ko.record("src/f.py", "arith-op-swap", "t::c");
        ko.record("src/f.py", "arith-op-swap", "t::a");
        let out = ko.order(&ids(&["t::a", "t::b", "t::c"]), "src/f.py", "arith-op-swap");
        assert_eq!(out, ids(&["t::c", "t::a", "t::b"]));
    }

    #[test]
    fn parsed_param_killer_matches_and_lifts_the_selected_id() {
        // End-to-end guard for the space-in-nodeid fix: the id parsed from
        // pytest's `-rf` line must be byte-identical to the coverage-selected
        // id, so recording it actually lifts that id next run. A whitespace-
        // truncating parse would record `test_x.py::t[a` and never match.
        let selected = ids(&["tests/t.py::t[a b]", "tests/t.py::t[c d]"]);
        let killer = parse_first_failed("FAILED tests/t.py::t[c d] - AssertionError\n").unwrap();
        assert!(
            selected.contains(&killer),
            "parsed killer {killer:?} must match a selected id"
        );
        let mut ko = KillOrder::default();
        ko.record("src/f.py", "op", &killer);
        let out = ko.order(&selected, "src/f.py", "op");
        assert_eq!(out, ids(&["tests/t.py::t[c d]", "tests/t.py::t[a b]"]));
    }

    #[test]
    fn order_ties_and_unknowns_keep_input_order() {
        let mut ko = KillOrder::default();
        ko.record("src/f.py", "op", "t::b"); // only t::b has history (count 1)
                                             // t::a and t::c tie at 0 → keep input order; t::b (1) lifts to front.
        let out = ko.order(&ids(&["t::a", "t::b", "t::c"]), "src/f.py", "op");
        assert_eq!(out, ids(&["t::b", "t::a", "t::c"]));
    }

    #[test]
    fn order_is_always_a_permutation() {
        // The verdict-invariance guarantee: ordering only permutes the id set,
        // never adds or drops one. Since pytest `-x` exits non-zero iff *any*
        // selected test fails — independent of order — an identical set means an
        // identical kill/survive verdict. Reordering can only change *speed*.
        let mut ko = KillOrder::default();
        ko.record("src/f.py", "op", "t::c");
        ko.record("src/f.py", "op", "t::c");
        ko.record("src/f.py", "op", "t::a");
        for input in [
            vec![],
            ids(&["t::a"]),
            ids(&["t::a", "t::b", "t::c"]),
            ids(&["t::c", "t::b", "t::a", "t::d"]),
            ids(&["dup", "dup"]), // duplicates preserved
        ] {
            let out = ko.order(&input, "src/f.py", "op");
            let mut a = input.clone();
            let mut b = out.clone();
            a.sort();
            b.sort();
            assert_eq!(
                a, b,
                "order must be a permutation of {input:?}, got {out:?}"
            );
        }
    }

    #[test]
    fn order_scopes_by_file_and_operator() {
        let mut ko = KillOrder::default();
        ko.record("src/other.py", "op", "t::a"); // different file
        ko.record("src/f.py", "other-op", "t::a"); // different operator
                                                   // Neither entry applies to (src/f.py, op) → order unchanged.
        let input = ids(&["t::a", "t::b"]);
        assert_eq!(ko.order(&input, "src/f.py", "op"), input);
    }

    // --- KillOrder load/save/apply ---

    #[test]
    fn save_load_roundtrip_and_apply_accumulates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".fermut").join("kill-order.json");
        let mut ko = KillOrder::default();
        ko.apply(&[
            KillRecord {
                file: "src/f.py".into(),
                operator: "op".into(),
                nodeid: "t::a".into(),
            },
            KillRecord {
                file: "src/f.py".into(),
                operator: "op".into(),
                nodeid: "t::a".into(),
            },
        ]);
        ko.save(&path).unwrap();

        let loaded = KillOrder::load(&path);
        // Counts survived the roundtrip: t::a killed twice → sorts first.
        let out = loaded.order(&ids(&["t::z", "t::a"]), "src/f.py", "op");
        assert_eq!(out, ids(&["t::a", "t::z"]));
    }

    #[test]
    fn load_missing_or_corrupt_is_empty_never_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing file.
        let missing = KillOrder::load(&tmp.path().join("nope.json"));
        assert!(missing.order(&ids(&["t::a"]), "f", "op") == ids(&["t::a"]));
        // Corrupt file.
        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        let corrupt = KillOrder::load(&bad);
        assert_eq!(corrupt.order(&ids(&["t::a"]), "f", "op"), ids(&["t::a"]));
    }
}
