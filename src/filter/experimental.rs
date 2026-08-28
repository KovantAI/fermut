//! Experimental-operator gate. Drops any mutant whose operator is flagged
//! `is_experimental()` unless `--experimental` (or `experimental = true` in
//! config) is set. Always first in the filter chain so noisy experimental
//! mutations are filtered before any other filter spends cycles on them.

use anyhow::Result;

use super::Filter;
use crate::mutator::Mutant;

/// Drops experimental operators unless `include` is set.
pub struct ExperimentalFilter {
    include: bool,
}

impl ExperimentalFilter {
    pub fn new(include: bool) -> Self {
        Self { include }
    }
}

impl Filter for ExperimentalFilter {
    fn name(&self) -> &'static str {
        "experimental"
    }

    fn admits(&self, mutant: &Mutant) -> Result<bool> {
        Ok(self.include || !mutant.operator.is_experimental())
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
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        }
    }

    #[test]
    fn stable_op_always_admitted() {
        let off = ExperimentalFilter::new(false);
        let on = ExperimentalFilter::new(true);
        let m = make_mutant(Operator::ArithOpSwap);
        assert!(off.admits(&m).unwrap());
        assert!(on.admits(&m).unwrap());
    }

    #[test]
    fn experimental_op_admitted_only_when_enabled() {
        let off = ExperimentalFilter::new(false);
        let on = ExperimentalFilter::new(true);
        let m = make_mutant(Operator::ExceptionClassSwap);
        assert!(!off.admits(&m).unwrap());
        assert!(on.admits(&m).unwrap());
    }
}
