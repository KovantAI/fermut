//! Parity-operator gate. Drops any mutant whose operator is flagged
//! `is_parity()` unless `--parity` (or `parity = true` in config) is set.
//! Sits beside the experimental gate at the front of the chain so these very
//! noisy comparison-only mutations are filtered before any other filter
//! spends cycles on them.

use anyhow::Result;

use super::Filter;
use crate::mutator::Mutant;

/// Drops parity operators unless `include` is set.
pub struct ParityFilter {
    include: bool,
}

impl ParityFilter {
    pub fn new(include: bool) -> Self {
        Self { include }
    }
}

impl Filter for ParityFilter {
    fn name(&self) -> &'static str {
        "parity"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        Ok(self.include || !mutant.operator.is_parity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn make_mutant(op: Operator) -> Mutant {
        Mutant {
            id: "x".into(),
            file: PathBuf::from("test.py"),
            operator: op,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "f()".into(),
            replacement: "None".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn stable_op_always_admitted() {
        let m = make_mutant(Operator::ArithOpSwap);
        assert!(ParityFilter::new(false).admits(&m).unwrap());
        assert!(ParityFilter::new(true).admits(&m).unwrap());
    }

    #[test]
    fn parity_op_admitted_only_when_enabled() {
        let m = make_mutant(Operator::ExprToNone);
        assert!(!ParityFilter::new(false).admits(&m).unwrap());
        assert!(ParityFilter::new(true).admits(&m).unwrap());
    }
}
