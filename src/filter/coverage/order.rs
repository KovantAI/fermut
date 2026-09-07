//! Cold-start test ordering by coverage breadth. Given the tests that cover a
//! mutant, reorder them so the most focused test — the likeliest `-x` killer —
//! runs first. Permutation-only: never changes the kill/survive verdict, only
//! which test pytest tries first.

use std::collections::HashMap;
use std::path::Path;

use super::CoverageContexts;

impl CoverageContexts {
    /// Per-test coverage breadth (number of covered `(file, line)` cells),
    /// keyed by test node id. Empty when there are no contexts. The smart-order
    /// cold-start prior reads this to prefer more-targeted tests.
    pub fn breadth_map(&self) -> &HashMap<String, u32> {
        &self.test_breadth
    }

    /// Reorder coverage-selected test node ids so the test most focused on
    /// `file` — the one covering the fewest of *its* lines — runs first, with
    /// global breadth as the tie-breaker. Under pytest's `-x` a focused unit
    /// test is the likelier killer, so trying it first lets the mutant die (and
    /// the run return) sooner. This is the cold-start ordering prior: it needs no
    /// run history, only the coverage already loaded, so it helps on run 1 and on
    /// freshly-changed `--since` lines.
    ///
    /// Scoping to `file` (the mutated file) is what makes the signal correct: a
    /// broad integration test that heavily exercises the mutated function ranks
    /// by *its lines in this file*, not its repo-wide footprint, so it no longer
    /// sinks below a narrow test that merely grazes one line here.
    ///
    /// Borrows the input — the returned refs point back into `ids`; the caller
    /// keeps `ids` alive. `sort_by_key` is stable, so ties (and ids absent from
    /// the breadth maps, treated as maximally broad → sorted last) keep the
    /// caller's order. **Only permutes** the set — never adds or drops an id —
    /// so the kill/survive verdict is unchanged; only which test pytest tries
    /// first.
    pub fn order_by_breadth_in<'a>(&self, file: &Path, ids: &'a [String]) -> Vec<&'a String> {
        let per_file = self.file_test_breadth.get(&self.resolve_key(file));
        let mut ordered: Vec<&String> = ids.iter().collect();
        ordered.sort_by_key(|id| {
            let local = per_file
                .and_then(|m| m.get(*id))
                .copied()
                .unwrap_or(u32::MAX);
            let global = self.test_breadth.get(*id).copied().unwrap_or(u32::MAX);
            (local, global)
        });
        ordered
    }

    /// Global-breadth-only ordering: rank by repo-wide `(file, line)` cell count,
    /// ignoring which file the mutant is in. Retained as the breadth primitive
    /// and for callers with no file to scope by; prefer
    /// [`order_by_breadth_in`](Self::order_by_breadth_in) whenever a mutated file
    /// is known. Permutation-only, same as the scoped form.
    pub fn order_by_breadth<'a>(&self, ids: &'a [String]) -> Vec<&'a String> {
        let mut ordered: Vec<&String> = ids.iter().collect();
        ordered.sort_by_key(|id| self.test_breadth.get(*id).copied().unwrap_or(u32::MAX));
        ordered
    }
}
