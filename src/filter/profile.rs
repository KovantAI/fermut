//! Operator-profile gate (`--operators`). The visitor always emits the
//! region-model operators (see `mutator::region`); this front-of-chain filter
//! keeps the set the profile asks for, so `fermut list --operators full` and
//! the subsumption gate can see every candidate while a default run is
//! unchanged.

use anyhow::Result;

use super::Filter;
use crate::config::OperatorProfile;
use crate::mutator::{Mutant, Operator};

pub struct ProfileFilter {
    profile: OperatorProfile,
}

impl ProfileFilter {
    pub fn new(profile: OperatorProfile) -> Self {
        Self { profile }
    }
}

impl Filter for ProfileFilter {
    fn name(&self) -> &'static str {
        "profile"
    }

    fn admits(&self, m: &Mutant) -> Result<bool> {
        let op = m.operator;
        Ok(match self.profile {
            OperatorProfile::Default => !op.is_profile_gated(),
            OperatorProfile::Full => op != Operator::RegionRest,
            // At a modelled site keep only the dominators; everything else
            // (untagged sites, other operators) is untouched.
            OperatorProfile::Minimal => {
                op != Operator::RegionRest && m.site.is_none_or(|s| s.minimal)
            }
            OperatorProfile::RorAll => true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::visitor::collect;
    use std::path::Path;

    fn kept(profile: OperatorProfile, src: &str) -> Vec<String> {
        let f = ProfileFilter::new(profile);
        let mut v: Vec<String> = collect(Path::new("t.py"), src)
            .unwrap()
            .into_iter()
            .filter(|m| f.admits(m).unwrap())
            .filter(|m| m.site.is_some())
            .map(|m| m.replacement)
            .collect();
        v.sort();
        v
    }

    const LT: &str = "def f(a, b):\n    if a < b:\n        return 1\n";
    const AND: &str = "def f(a, b):\n    if a and b:\n        return 1\n";

    #[test]
    fn default_profile_keeps_todays_compare_and_bool_mutants() {
        assert_eq!(kept(OperatorProfile::Default, LT), vec!["<=", ">", ">="]);
        assert_eq!(kept(OperatorProfile::Default, AND), vec!["or"]);
    }

    #[test]
    fn minimal_profile_keeps_only_the_dominators() {
        assert_eq!(
            kept(OperatorProfile::Minimal, LT),
            vec!["!=", "<=", "False"]
        );
        assert_eq!(
            kept(OperatorProfile::Minimal, AND),
            vec!["(a)", "(b)", "False"]
        );
    }

    #[test]
    fn full_is_the_union_and_ror_all_has_all_seven() {
        assert_eq!(
            kept(OperatorProfile::Full, LT),
            vec!["!=", "<=", ">", ">=", "False"]
        );
        assert_eq!(
            kept(OperatorProfile::RorAll, LT),
            vec!["!=", "<=", "==", ">", ">=", "False", "True"]
        );
        assert_eq!(
            kept(OperatorProfile::RorAll, AND),
            vec!["(a)", "(b)", "False", "True", "or"]
        );
    }

    #[test]
    fn untagged_mutants_pass_every_profile() {
        // Chained compare and a 3-operand chain aren't modelled: their swaps
        // survive `minimal` unchanged.
        let src = "def f(a, b, c):\n    if a < b < c:\n        return a or b or c\n";
        for p in [
            OperatorProfile::Default,
            OperatorProfile::Minimal,
            OperatorProfile::Full,
        ] {
            let f = ProfileFilter::new(p);
            let n = collect(Path::new("t.py"), src)
                .unwrap()
                .into_iter()
                .filter(|m| {
                    matches!(
                        m.operator,
                        Operator::CompareOpSwap | Operator::BoundaryShift | Operator::BoolOpSwap
                    )
                })
                .filter(|m| f.admits(m).unwrap())
                .count();
            assert_eq!(n, 8, "{p:?}");
        }
    }
}
