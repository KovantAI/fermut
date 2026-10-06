//! Region model for static operator subsumption (ROR + logical).
//!
//! A predicate's inputs fall into a few **regions**. For a relational compare
//! `a OP b` over a total order the regions are the sign of `a − b`: negative,
//! zero, positive. For a 2-operand logical connective in truth position they
//! are the four truth assignments of its operands: TT, TF, FT, FF. Every
//! predicate is a truth vector over its regions, and a mutant's **diff mask**
//! is the set of regions where its truth differs from the original's.
//!
//! A test can only kill a mutant by landing in a region where the mutant
//! disagrees with the original. So mutant `d` subsumes `m` (every test that
//! kills `d` kills `m`) when `diff(d) ⊆ diff(m)`. The **minimal set** for a
//! site is the candidates no other candidate strictly subsumes: one mutant per
//! region, each differing in exactly that region.
//!
//! | orig | minimal set | subsumed |
//! |---|---|---|
//! | `<` | `<=`, `!=`, `False` | `>`, `>=`, `==`, `True` |
//! | `<=` | `<`, `==`, `True` | `>`, `>=`, `!=`, `False` |
//! | `>` | `>=`, `!=`, `False` | `<`, `<=`, `==`, `True` |
//! | `>=` | `>`, `==`, `True` | `<`, `<=`, `!=`, `False` |
//! | `a and b` | `a`, `b`, `False` | `or`, `True` |
//! | `a or b` | `a`, `b`, `True` | `and`, `False` |
//!
//! ROR: Kaminski, Ammann & Offutt, *Better Predicate Testing* (2011).
//! Logical: Kaminski & Ammann's minimal logical-operator set. The table is not
//! hard-coded: [`minimal_ror`] and [`minimal_logic`] derive it from the truth
//! vectors, and a test checks the result against the table above.
//!
//! The model assumes a total order and a boolean context. Python breaks both
//! in places (sets are partially ordered, NaN compares false, numpy returns
//! arrays, `a and b` returns an operand), which is why the minimal set ships
//! as an opt-in operator profile behind an empirical gate, not as a proof.

/// Bitmask over regions. ROR: bit0 = neg, bit1 = zero, bit2 = pos.
/// Logical: bit0 = TT, bit1 = TF, bit2 = FT, bit3 = FF.
pub type DiffMask = u8;

/// Relational forms a single-op ordering compare can be mutated into, with
/// their truth vectors over (neg, zero, pos). Order is the emission order for
/// [`ror_candidates`].
const ROR_FORMS: &[(&str, u8)] = &[
    ("<", 0b001),
    ("<=", 0b011),
    (">", 0b100),
    (">=", 0b110),
    ("==", 0b010),
    ("!=", 0b101),
    ("True", 0b111),
    ("False", 0b000),
];

/// The ordering operators the region model applies to. `==`/`!=` are left
/// alone: their minimal set would add `<=`/`>=`, which raise on unorderable
/// operands such as `None`.
pub fn is_ordering(op: &str) -> bool {
    matches!(op, "<" | "<=" | ">" | ">=")
}

fn truth_ror(form: &str) -> Option<u8> {
    ROR_FORMS.iter().find(|(f, _)| *f == form).map(|(_, t)| *t)
}

/// Regions where `repl` disagrees with `orig`. `None` for an unknown form.
pub fn ror_diff(orig: &str, repl: &str) -> Option<DiffMask> {
    Some(truth_ror(orig)? ^ truth_ror(repl)?)
}

/// The 7 other forms for an ordering `orig`, in a fixed order.
pub fn ror_candidates(orig: &str) -> impl Iterator<Item = &'static str> + '_ {
    ROR_FORMS
        .iter()
        .map(|(f, _)| *f)
        .filter(move |f| *f != orig)
}

/// Logical forms of a 2-operand connective `a OP b`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogicForm {
    And,
    Or,
    Lhs,
    Rhs,
    True,
    False,
}

impl LogicForm {
    pub const ALL: [LogicForm; 6] = [
        LogicForm::And,
        LogicForm::Or,
        LogicForm::Lhs,
        LogicForm::Rhs,
        LogicForm::True,
        LogicForm::False,
    ];

    /// Truth vector over (TT, TF, FT, FF).
    fn truth(self) -> u8 {
        match self {
            LogicForm::And => 0b0001,
            LogicForm::Or => 0b0111,
            LogicForm::Lhs => 0b0011,
            LogicForm::Rhs => 0b0101,
            LogicForm::True => 0b1111,
            LogicForm::False => 0b0000,
        }
    }
}

pub fn logic_diff(orig: LogicForm, repl: LogicForm) -> DiffMask {
    orig.truth() ^ repl.truth()
}

/// `d` subsumes `m`: every test that kills `d` kills `m`.
pub fn subsumes(d: DiffMask, m: DiffMask) -> bool {
    d != 0 && d & !m == 0
}

/// Of a site's candidate masks, keep those no other candidate strictly
/// subsumes. Equal masks don't exclude each other.
fn minimal_masks(masks: &[DiffMask]) -> Vec<DiffMask> {
    masks
        .iter()
        .copied()
        .filter(|&m| m != 0 && !masks.iter().any(|&o| o != m && subsumes(o, m)))
        .collect()
}

/// Minimal-set forms for an ordering `orig` (see the module table).
pub fn minimal_ror(orig: &str) -> Vec<&'static str> {
    let cands: Vec<(&'static str, DiffMask)> = ror_candidates(orig)
        .filter_map(|f| Some((f, ror_diff(orig, f)?)))
        .collect();
    let keep = minimal_masks(&cands.iter().map(|(_, m)| *m).collect::<Vec<_>>());
    cands
        .into_iter()
        .filter(|(_, m)| keep.contains(m))
        .map(|(f, _)| f)
        .collect()
}

/// Minimal-set forms for `orig` (`And` or `Or`).
pub fn minimal_logic(orig: LogicForm) -> Vec<LogicForm> {
    let cands: Vec<(LogicForm, DiffMask)> = LogicForm::ALL
        .into_iter()
        .filter(|f| *f != orig)
        .map(|f| (f, logic_diff(orig, f)))
        .collect();
    let keep = minimal_masks(&cands.iter().map(|(_, m)| *m).collect::<Vec<_>>());
    cands
        .into_iter()
        .filter(|(_, m)| keep.contains(m))
        .map(|(f, _)| f)
        .collect()
}

/// Is a mutant with diff `mask` at an ordering site `orig` in the minimal set?
pub fn ror_is_minimal(orig: &str, mask: DiffMask) -> bool {
    minimal_ror(orig)
        .into_iter()
        .any(|f| ror_diff(orig, f) == Some(mask))
}

pub fn logic_is_minimal(orig: LogicForm, mask: DiffMask) -> bool {
    minimal_logic(orig)
        .into_iter()
        .any(|f| logic_diff(orig, f) == mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ror_minimal_sets_match_kaminski_table() {
        let sorted = |mut v: Vec<&'static str>| {
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(minimal_ror("<")), vec!["!=", "<=", "False"]);
        assert_eq!(sorted(minimal_ror("<=")), vec!["<", "==", "True"]);
        assert_eq!(sorted(minimal_ror(">")), vec!["!=", ">=", "False"]);
        assert_eq!(sorted(minimal_ror(">=")), vec!["==", ">", "True"]);
    }

    #[test]
    fn derived_logic_minimal_sets_match_table() {
        use LogicForm::*;
        assert_eq!(minimal_logic(And), vec![Lhs, Rhs, False]);
        assert_eq!(minimal_logic(Or), vec![Lhs, Rhs, True]);
    }

    #[test]
    fn minimal_mutants_each_differ_in_exactly_one_region() {
        for orig in ["<", "<=", ">", ">="] {
            for f in minimal_ror(orig) {
                assert_eq!(ror_diff(orig, f).unwrap().count_ones(), 1, "{orig}->{f}");
            }
        }
        for orig in [LogicForm::And, LogicForm::Or] {
            for f in minimal_logic(orig) {
                assert_eq!(logic_diff(orig, f).count_ones(), 1, "{orig:?}->{f:?}");
            }
        }
    }

    #[test]
    fn subsumption_is_mask_containment() {
        // `<` site: `!=` (pos) subsumes `>` (neg, pos); `>` doesn't subsume `!=`.
        let ne = ror_diff("<", "!=").unwrap();
        let gt = ror_diff("<", ">").unwrap();
        assert!(subsumes(ne, gt));
        assert!(!subsumes(gt, ne));
        // An equivalent mutant (empty mask) subsumes nothing.
        assert!(!subsumes(0, gt));
        assert!(ror_is_minimal("<", ne));
        assert!(!ror_is_minimal("<", gt));
        assert!(!logic_is_minimal(
            LogicForm::And,
            logic_diff(LogicForm::And, LogicForm::Or)
        ));
    }

    #[test]
    fn ror_candidates_are_the_other_seven() {
        let c: Vec<_> = ror_candidates("<").collect();
        assert_eq!(c.len(), 7);
        assert!(!c.contains(&"<"));
    }
}
