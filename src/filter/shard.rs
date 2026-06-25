//! Deterministic shard filter. With `--shard i/n` the run processes only
//! mutants whose `hash(mutant.id) % n == i - 1` — the i-th of n disjoint
//! slices.
//!
//! No coordination required between shards. Run all N in parallel (CI
//! matrix, separate hosts, anything), then `fermut merge` to combine the
//! per-shard JSON reports.

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::Filter;
use crate::mutator::Mutant;

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
        "shard"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        Ok(shard_of(&mutant.id, self.total) == self.index)
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
}
