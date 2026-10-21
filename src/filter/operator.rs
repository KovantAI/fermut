//! Operator allow/deny filter. Wraps `--ops` (allowlist) and `--skip-ops`
//! (denylist) into a single filter; deny always wins over allow. Cheap
//! in-memory check, slotted right after the experimental gate.

use std::collections::HashSet;

use anyhow::Result;

use super::Filter;
use crate::mutator::{Mutant, Operator};

/// Allowlist / denylist over operator kinds.
/// - If `allow` is `Some`, only operators in that set pass.
/// - Any operator in `deny` is rejected (denylist wins over allowlist).
pub struct OperatorFilter {
    allow: Option<HashSet<Operator>>,
    deny: HashSet<Operator>,
}

impl OperatorFilter {
    pub fn new(allow: Option<HashSet<Operator>>, deny: HashSet<Operator>) -> Self {
        Self { allow, deny }
    }
}

impl Filter for OperatorFilter {
    fn name(&self) -> &'static str {
        "operator"
    }

    fn admits(&self, m: &Mutant) -> Result<bool> {
        if self.deny.contains(&m.operator) {
            return Ok(false);
        }
        if let Some(allow) = &self.allow {
            return Ok(allow.contains(&m.operator));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            site: None,
        }
    }

    #[test]
    fn no_filters_admits_everything() {
        let f = OperatorFilter::new(None, HashSet::new());
        assert!(f.admits(&make_mutant(Operator::ArithOpSwap)).unwrap());
        assert!(f.admits(&make_mutant(Operator::BoolOpSwap)).unwrap());
    }

    #[test]
    fn allowlist_admits_only_listed() {
        let allow: HashSet<_> = [Operator::ArithOpSwap].into_iter().collect();
        let f = OperatorFilter::new(Some(allow), HashSet::new());
        assert!(f.admits(&make_mutant(Operator::ArithOpSwap)).unwrap());
        assert!(!f.admits(&make_mutant(Operator::BoolOpSwap)).unwrap());
    }

    #[test]
    fn denylist_rejects_listed() {
        let deny: HashSet<_> = [Operator::NumberShift].into_iter().collect();
        let f = OperatorFilter::new(None, deny);
        assert!(f.admits(&make_mutant(Operator::ArithOpSwap)).unwrap());
        assert!(!f.admits(&make_mutant(Operator::NumberShift)).unwrap());
    }

    #[test]
    fn deny_wins_over_allow() {
        let allow: HashSet<_> = [Operator::ArithOpSwap, Operator::NumberShift]
            .into_iter()
            .collect();
        let deny: HashSet<_> = [Operator::NumberShift].into_iter().collect();
        let f = OperatorFilter::new(Some(allow), deny);
        assert!(f.admits(&make_mutant(Operator::ArithOpSwap)).unwrap());
        assert!(!f.admits(&make_mutant(Operator::NumberShift)).unwrap());
    }
}
