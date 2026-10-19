//! Static survivor folding: collapse survivors a co-located survivor subsumes.
//!
//! Within one region-model site (see [`crate::mutator::region`]), survivor `d`
//! subsumes survivor `m` when `diff(d) ⊆ diff(m)`: every test that kills `d`
//! kills `m`. So `m` is not a separate test-writing target. Write the test for
//! `d` and `m` dies with it, *if the region model holds* for the operands
//! (see the caveats in `region`). At a `<` site where `>` and `>=` both
//! survive, the only target is `>`.
//!
//! Folding is presentation only: the score, the counts and the outcomes are
//! untouched. It works for every operator profile, because the default set
//! has subsumption inside it too (for `<`, `>` subsumes `>=`).
//!
//! Survivors with equal masks are an equivalence class; the lowest offset
//! (then id) is kept. Survivors without a [`SiteTag`] are never folded.
//!
//! [`SiteTag`]: crate::mutator::SiteTag

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use crate::mutator::region::subsumes;
use crate::mutator::Mutant;
use crate::report::MutantOutcome;

/// One test-writing target and the survivors it subsumes.
#[derive(Debug)]
pub struct FoldGroup<'a> {
    pub target: &'a Mutant,
    /// Survivors at the same site that a test killing `target` also kills.
    /// Sorted by offset, then id.
    pub subsumed: Vec<&'a Mutant>,
}

/// Fold `survivors` into targets. Groups come back in the order their target
/// first appears in `survivors`.
pub fn fold<'a>(survivors: &[&'a Mutant]) -> Vec<FoldGroup<'a>> {
    // Tagged survivors by site; untagged ones are their own target.
    let mut by_site: BTreeMap<(PathBuf, u32), Vec<&'a Mutant>> = BTreeMap::new();
    for m in survivors {
        if let Some(site) = m.site {
            by_site
                .entry((m.file.clone(), site.start))
                .or_default()
                .push(m);
        }
    }

    // Per site: walk survivors from the narrowest mask out. A strict subsumer
    // has fewer regions, so it is always seen first; one with an equal mask
    // is earlier by offset/id. The first target that subsumes a survivor
    // absorbs it; a survivor no target subsumes becomes a target.
    let mut parent: HashMap<&str, &str> = HashMap::new();
    for members in by_site.values_mut() {
        members.sort_by_key(|m| {
            let s = m.site.expect("grouped by site");
            (
                s.mask.count_ones(),
                u32::from(m.range.start()),
                m.id.clone(),
            )
        });
        let mut targets: Vec<&Mutant> = Vec::new();
        for &m in members.iter() {
            let mask = m.site.expect("grouped by site").mask;
            let dominator = targets.iter().find(|t| {
                let tm = t.site.expect("grouped by site").mask;
                subsumes(tm, mask) || tm == mask
            });
            match dominator {
                Some(t) => {
                    parent.insert(m.id.as_str(), t.id.as_str());
                }
                None => targets.push(m),
            }
        }
    }

    let mut subsumed: HashMap<&str, Vec<&'a Mutant>> = HashMap::new();
    for m in survivors {
        if let Some(&p) = parent.get(m.id.as_str()) {
            subsumed.entry(p).or_default().push(m);
        }
    }
    let mut seen = HashSet::new();
    survivors
        .iter()
        .filter(|m| !parent.contains_key(m.id.as_str()) && seen.insert(m.id.as_str()))
        .map(|&target| {
            let mut sub = subsumed.remove(target.id.as_str()).unwrap_or_default();
            sub.sort_by_key(|m| (u32::from(m.range.start()), m.id.clone()));
            FoldGroup {
                target,
                subsumed: sub,
            }
        })
        .collect()
}

/// The `Survived` mutants of `outcomes`, in report order.
pub fn survivors(outcomes: &[MutantOutcome]) -> Vec<&Mutant> {
    outcomes
        .iter()
        .filter_map(|o| match o {
            MutantOutcome::Survived { mutant } => Some(mutant.as_ref()),
            _ => None,
        })
        .collect()
}

/// Folding of a report's survivors, indexed for writers that walk outcomes in
/// report order: skip [`is_folded`](Self::is_folded) ids and render
/// [`subsumed`](Self::subsumed) under their target.
pub struct FoldIndex<'a> {
    subsumed: HashMap<&'a str, Vec<&'a Mutant>>,
    folded: HashSet<&'a str>,
}

impl<'a> FoldIndex<'a> {
    pub fn new(outcomes: &'a [MutantOutcome]) -> Self {
        let mut subsumed = HashMap::new();
        let mut folded = HashSet::new();
        for g in fold(&survivors(outcomes)) {
            if g.subsumed.is_empty() {
                continue;
            }
            folded.extend(g.subsumed.iter().map(|m| m.id.as_str()));
            subsumed.insert(g.target.id.as_str(), g.subsumed);
        }
        Self { subsumed, folded }
    }

    /// True for a survivor rendered under its target instead of on its own.
    pub fn is_folded(&self, id: &str) -> bool {
        self.folded.contains(id)
    }

    /// Survivors folded under target `id` (empty for anything else).
    pub fn subsumed(&self, id: &str) -> &[&'a Mutant] {
        self.subsumed.get(id).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::visitor::collect;
    use std::path::Path;

    /// Survivors of `src` whose replacement is in `repls`.
    fn survivors_of(src: &str, repls: &[&str]) -> Vec<Mutant> {
        collect(Path::new("t.py"), src)
            .unwrap()
            .into_iter()
            .filter(|m| m.site.is_some() && repls.contains(&m.replacement.as_str()))
            .collect()
    }

    fn targets(ms: &[Mutant]) -> Vec<(String, Vec<String>)> {
        let refs: Vec<&Mutant> = ms.iter().collect();
        fold(&refs)
            .into_iter()
            .map(|g| {
                (
                    g.target.replacement.clone(),
                    g.subsumed.iter().map(|m| m.replacement.clone()).collect(),
                )
            })
            .collect()
    }

    const LT: &str = "def f(a, b):\n    if a < b:\n        return 1\n";

    #[test]
    fn gt_subsumes_ge_at_a_lt_site() {
        let ms = survivors_of(LT, &[">", ">="]);
        assert_eq!(targets(&ms), vec![(">".into(), vec![">=".into()])]);
    }

    #[test]
    fn boundary_survivor_absorbs_the_always_different_one() {
        let ms = survivors_of(LT, &["<=", ">="]);
        assert_eq!(targets(&ms), vec![("<=".into(), vec![">=".into()])]);
    }

    #[test]
    fn incomparable_survivors_stay_separate_targets() {
        // `<=` (zero) and `>` (neg, pos) share no containment.
        let ms = survivors_of(LT, &["<=", ">"]);
        let t = targets(&ms);
        assert_eq!(t.len(), 2);
        assert!(t.iter().all(|(_, sub)| sub.is_empty()));
    }

    #[test]
    fn different_sites_never_fold_together() {
        let src =
            "def f(a, b, c):\n    if a < b:\n        return 1\n    if b < c:\n        return 2\n";
        let ms = survivors_of(src, &[">", ">="]);
        // Two sites, each folding its own `>=` under its own `>`.
        let t = targets(&ms);
        assert_eq!(t.len(), 2);
        assert!(t
            .iter()
            .all(|(r, sub)| r == ">" && sub == &vec![">=".to_string()]));
    }

    #[test]
    fn untagged_survivors_are_never_folded() {
        let ms: Vec<Mutant> = collect(Path::new("t.py"), "x = a + b\n")
            .unwrap()
            .into_iter()
            .collect();
        let refs: Vec<&Mutant> = ms.iter().collect();
        let groups = fold(&refs);
        assert_eq!(groups.len(), ms.len());
        assert!(groups.iter().all(|g| g.subsumed.is_empty()));
    }

    #[test]
    fn index_hides_folded_ids_and_lists_them_under_the_target() {
        use std::sync::Arc;
        let ms = survivors_of(LT, &[">", ">="]);
        let outcomes: Vec<MutantOutcome> = ms
            .iter()
            .map(|m| MutantOutcome::survived(Arc::new(m.clone())))
            .collect();
        let idx = FoldIndex::new(&outcomes);
        let gt = ms.iter().find(|m| m.replacement == ">").unwrap();
        let ge = ms.iter().find(|m| m.replacement == ">=").unwrap();
        assert!(idx.is_folded(&ge.id));
        assert!(!idx.is_folded(&gt.id));
        assert_eq!(idx.subsumed(&gt.id).len(), 1);
        assert!(idx.subsumed(&ge.id).is_empty());
    }
}
