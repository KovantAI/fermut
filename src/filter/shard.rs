//! Deterministic shard filter. With `--shard i/n` the run processes only
//! mutants whose `hash(mutant.id) % n == i - 1` — the i-th of n disjoint
//! slices.
//!
//! No coordination required between shards. Run all N in parallel (CI
//! matrix, separate hosts, anything), then `fermut merge` to combine the
//! per-shard JSON reports.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::Filter;
use crate::engine::TIME_BUDGET_FILTER;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;

/// Filter name a sharded run stamps on every mutant *outside* its slice. The
/// merge step keys its placeholder detection off this exact string, so the two
/// must never drift — hence one const shared by both.
pub const SHARD_FILTER_NAME: &str = "shard";

pub struct ShardFilter {
    /// 0-based internally; CLI parses 1-based.
    index: u32,
    total: u32,
}

impl ShardFilter {
    /// `index` is 1-based; range `1..=total`.
    pub fn new(index: u32, total: u32) -> Self {
        assert!(total > 0, "shard total must be >= 1");
        assert!(index >= 1 && index <= total, "shard index out of range");
        Self {
            index: index - 1,
            total,
        }
    }
}

impl Filter for ShardFilter {
    fn name(&self) -> &'static str {
        SHARD_FILTER_NAME
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        Ok(shard_of(&mutant.id, self.total) == self.index)
    }
}

/// Is `o` an unresolved mutant: the `shard` placeholder for another slice, or
/// a mutant its owning shard never started before `--max-time`? `fermut merge`
/// lets any real verdict replace either, and `--mutants-from` re-runs them.
pub fn is_unresolved(o: &MutantOutcome) -> bool {
    matches!(o, MutantOutcome::Skipped { filter, .. }
        if filter == SHARD_FILTER_NAME || filter == TIME_BUDGET_FILTER)
}

/// `--mutants-from REPORT`: admits only the mutants `REPORT` left unresolved.
/// Named like [`ShardFilter`] on purpose: everything else is outside this
/// run's slice, so its `shard` placeholder merges under the report's verdicts.
pub struct MutantsFromFilter {
    ids: HashSet<String>,
}

impl MutantsFromFilter {
    pub fn load(report: &Path) -> Result<Self> {
        let report = crate::report::load(report)
            .with_context(|| format!("reading --mutants-from {}", report.display()))?;
        Ok(Self::from_outcomes(&report.outcomes))
    }

    fn from_outcomes(outcomes: &[MutantOutcome]) -> Self {
        let ids = outcomes
            .iter()
            .filter(|o| is_unresolved(o))
            .map(|o| o.mutant().id.clone())
            .collect();
        Self { ids }
    }
}

impl Filter for MutantsFromFilter {
    fn name(&self) -> &'static str {
        SHARD_FILTER_NAME
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        Ok(self.ids.contains(&mutant.id))
    }
}

fn shard_of(id: &str, total: u32) -> u32 {
    let mut h = Sha256::new();
    h.update(id.as_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let n = u64::from_le_bytes(bytes);
    (n % total as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn make_mutant(id: &str) -> Mutant {
        Mutant {
            id: id.into(),
            file: PathBuf::from("test.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    #[test]
    fn shards_partition_input() {
        let total = 4u32;
        let filters: Vec<ShardFilter> = (1..=total).map(|i| ShardFilter::new(i, total)).collect();
        let mut counts = [0usize; 4];
        let total_mutants = 1_000;
        for i in 0..total_mutants {
            let m = make_mutant(&format!("id-{i}"));
            let mut admitted = 0;
            for (s, f) in filters.iter().enumerate() {
                if f.admits(&m).unwrap() {
                    counts[s] += 1;
                    admitted += 1;
                }
            }
            assert_eq!(
                admitted, 1,
                "mutant {i} matched {admitted} shards, expected 1"
            );
        }
        let sum: usize = counts.iter().sum();
        assert_eq!(sum, total_mutants);
        // Roughly uniform: each shard ~ 250 ± 50.
        for c in &counts {
            assert!(
                (200..=300).contains(c),
                "shard count {c} outside expected 200..=300"
            );
        }
    }

    #[test]
    fn single_shard_admits_everything() {
        let f = ShardFilter::new(1, 1);
        for i in 0..100 {
            assert!(f.admits(&make_mutant(&format!("id-{i}"))).unwrap());
        }
    }

    #[test]
    fn shard_selection_is_deterministic() {
        let f1 = ShardFilter::new(2, 5);
        let f2 = ShardFilter::new(2, 5);
        for i in 0..200 {
            let m = make_mutant(&format!("id-{i}"));
            assert_eq!(f1.admits(&m).unwrap(), f2.admits(&m).unwrap());
        }
    }

    #[test]
    fn mutants_from_admits_only_unresolved_mutants_and_reads_as_a_shard_placeholder() {
        let outcomes = vec![
            MutantOutcome::killed(make_mutant("killed")),
            MutantOutcome::skipped(make_mutant("budget"), TIME_BUDGET_FILTER),
            MutantOutcome::skipped(make_mutant("other-slice"), SHARD_FILTER_NAME),
            MutantOutcome::skipped(make_mutant("ruff"), "ruff"),
        ];
        let f = MutantsFromFilter::from_outcomes(&outcomes);
        let admitted: Vec<_> = ["killed", "budget", "other-slice", "ruff", "new"]
            .into_iter()
            .filter(|id| f.admits(&make_mutant(id)).unwrap())
            .collect();
        assert_eq!(admitted, ["budget", "other-slice"]);
        assert_eq!(
            f.name(),
            SHARD_FILTER_NAME,
            "merge must read its skips as placeholders"
        );
    }
}
