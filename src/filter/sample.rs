//! Random sampling filter.
//!
//! Keeps mutants with probability `ratio`. Decision is deterministic per
//! `(seed, mutant.id)` — same seed picks the same subset on every run, so
//! `--sample 0.2` lets you ratchet down test cost without losing
//! reproducibility within a CI matrix.

use anyhow::Result;
use sha2::{Digest, Sha256};

use super::Filter;
use crate::mutator::Mutant;

pub struct SampleFilter {
    ratio: f64,
    seed: u64,
}

impl SampleFilter {
    /// `ratio` is clamped to `[0.0, 1.0]`.
    pub fn new(ratio: f64, seed: u64) -> Self {
        Self {
            ratio: ratio.clamp(0.0, 1.0),
            seed,
        }
    }
}

impl Filter for SampleFilter {
    fn name(&self) -> &'static str {
        "sample"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        if self.ratio >= 1.0 {
            return Ok(true);
        }
        if self.ratio <= 0.0 {
            return Ok(false);
        }
        Ok(score(self.seed, &mutant.id) < self.ratio)
    }
}

/// Map (seed, id) to a uniform [0.0, 1.0) value by hashing then taking the
/// top 53 bits (the f64 mantissa width) so the result is exactly representable.
fn score(seed: u64, id: &str) -> f64 {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let n = u64::from_le_bytes(bytes) >> 11;
    (n as f64) / ((1u64 << 53) as f64)
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
        }
    }

    #[test]
    fn ratio_one_admits_all() {
        let f = SampleFilter::new(1.0, 42);
        for i in 0..100 {
            assert!(f.admits(&make_mutant(&format!("id-{i}"))).unwrap());
        }
    }

    #[test]
    fn ratio_zero_admits_none() {
        let f = SampleFilter::new(0.0, 42);
        for i in 0..100 {
            assert!(!f.admits(&make_mutant(&format!("id-{i}"))).unwrap());
        }
    }

    #[test]
    fn determinism_for_same_seed() {
        let f1 = SampleFilter::new(0.5, 12345);
        let f2 = SampleFilter::new(0.5, 12345);
        for i in 0..200 {
            let m = make_mutant(&format!("id-{i}"));
            assert_eq!(f1.admits(&m).unwrap(), f2.admits(&m).unwrap());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let f1 = SampleFilter::new(0.5, 1);
        let f2 = SampleFilter::new(0.5, 2);
        let mut diffs = 0;
        for i in 0..500 {
            let m = make_mutant(&format!("id-{i}"));
            if f1.admits(&m).unwrap() != f2.admits(&m).unwrap() {
                diffs += 1;
            }
        }
        // Two random seeds should disagree roughly half the time.
        assert!(diffs > 150 && diffs < 350, "diffs = {diffs}");
    }

    #[test]
    fn approximate_ratio_holds_for_500_samples() {
        let f = SampleFilter::new(0.30, 7);
        let mut admitted = 0;
        let total = 500;
        for i in 0..total {
            if f.admits(&make_mutant(&format!("id-{i}"))).unwrap() {
                admitted += 1;
            }
        }
        // Expected = 150; allow ±50.
        assert!(
            (100..=200).contains(&admitted),
            "admitted = {admitted}, expected ~150"
        );
    }

    #[test]
    fn out_of_range_ratios_clamp() {
        let above = SampleFilter::new(2.0, 0);
        let below = SampleFilter::new(-1.0, 0);
        let m = make_mutant("x");
        assert!(above.admits(&m).unwrap());
        assert!(!below.admits(&m).unwrap());
    }
}
