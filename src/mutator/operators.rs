//! Operator catalog: every mutation kind fermut knows + lexeme swap tables.
//!
//! [`Operator`] is the closed enum the visitor emits. The `*_SWAPS` tables
//! drive the table-driven operators (arith, compare, boundary, bool, aug-
//! assign, constant) — adding an entry adds a mutation without touching the
//! visitor. Operators outside that pattern (return-to-None, decorator drop,
//! slice bounds, etc.) have bespoke handlers in `visitor.rs`.

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Operator {
    ArithOpSwap,
    CompareOpSwap,
    BoolOpSwap,
    ConstantReplace,
    BoundaryShift,
    AugAssignSwap,
    UnaryOpSwap,
    NumberShift,
    ReturnValueToNone,
    BreakContinueSwap,
    NotInsertion,
    RemoveDecorator,
    DefaultArgToNone,
    LambdaBodyToNone,
    SliceBoundDrop,
    SliceStepMutate,
    AssignValueToNone,
    NumberToZero,
    NumberToNeg,
    StringToEmpty,
    StringSentinel,
    BytesSentinel,
    KeywordArgDrop,
    DictItemDrop,
    ArgToNone,
    NoneToValue,
    AwaitDrop,
    AsyncForToSync,
    AsyncWithToSync,
    MatchGuardNegate,

    // Minimal-profile operators — the region-model dominators the default set
    // lacks (see `region`). Always emitted by the visitor; the `--operators`
    // profile filter keeps them only under `minimal` / `full`.
    RelationalToEquality,
    CompareToConst,
    BoolOperandDrop,
    BoolOpToConst,
    /// The remaining region-model forms of a site, so the subsumption gate can
    /// check all 7 ROR mutants. Kept only under the hidden `ror-all` profile.
    RegionRest,

    // Experimental — opt-in via `--experimental`.
    // Higher noise: more equivalent mutants, more likely to break runtime
    // semantics in ways the test suite can't usefully detect.
    ExceptionClassSwap,
    BareExcept,
    ZeroIterationForLoop,
    OneIterationForLoop,
    RaiseFromDrop,
    // Type-annotation operators. Annotations are rarely enforced at runtime
    // (Python evaluates but does not check them), so most survive — unless the
    // project runtime-validates types (pydantic, dataclasses, beartype), where
    // they light up real gaps. Experimental for that reason.
    NumericTypeSwap,
    OptionalTypeDrop,
    ContainerTypeSwap,

    // Parity — opt-in via `--parity`. Exist to broaden overlap with other
    // mutation tools (mutmut) for cross-tool comparison, NOT for normal
    // scoring. Very high noise: many equivalent/trivial mutants. Off by
    // default; never counted in a default run's score.
    ExprToNone,
    PositionalDrop,
    StringCaseSwap,
}

impl Operator {
    pub fn name(self) -> &'static str {
        match self {
            Self::ArithOpSwap => "arith-op-swap",
            Self::CompareOpSwap => "compare-op-swap",
            Self::BoolOpSwap => "bool-op-swap",
            Self::ConstantReplace => "constant-replace",
            Self::BoundaryShift => "boundary-shift",
            Self::AugAssignSwap => "aug-assign-swap",
            Self::UnaryOpSwap => "unary-op-swap",
            Self::NumberShift => "number-shift",
            Self::ReturnValueToNone => "return-value-to-none",
            Self::BreakContinueSwap => "break-continue-swap",
            Self::NotInsertion => "not-insertion",
            Self::RemoveDecorator => "remove-decorator",
            Self::DefaultArgToNone => "default-arg-to-none",
            Self::LambdaBodyToNone => "lambda-body-to-none",
            Self::SliceBoundDrop => "slice-bound-drop",
            Self::SliceStepMutate => "slice-step-mutate",
            Self::AssignValueToNone => "assign-value-to-none",
            Self::NumberToZero => "number-to-zero",
            Self::NumberToNeg => "number-to-neg",
            Self::StringToEmpty => "string-to-empty",
            Self::StringSentinel => "string-sentinel",
            Self::BytesSentinel => "bytes-sentinel",
            Self::KeywordArgDrop => "keyword-arg-drop",
            Self::DictItemDrop => "dict-item-drop",
            Self::ArgToNone => "arg-to-none",
            Self::NoneToValue => "none-to-value",
            Self::AwaitDrop => "await-drop",
            Self::AsyncForToSync => "async-for-to-sync",
            Self::AsyncWithToSync => "async-with-to-sync",
            Self::MatchGuardNegate => "match-guard-negate",
            Self::RelationalToEquality => "ror-equality",
            Self::CompareToConst => "ror-const",
            Self::BoolOperandDrop => "bool-operand-drop",
            Self::BoolOpToConst => "bool-const",
            Self::RegionRest => "region-rest",
            Self::ExceptionClassSwap => "exp:exception-class-swap",
            Self::BareExcept => "exp:bare-except",
            Self::ZeroIterationForLoop => "exp:zero-iteration-for-loop",
            Self::OneIterationForLoop => "exp:one-iteration-for-loop",
            Self::RaiseFromDrop => "exp:raise-from-drop",
            Self::NumericTypeSwap => "exp:numeric-type-swap",
            Self::OptionalTypeDrop => "exp:optional-type-drop",
            Self::ContainerTypeSwap => "exp:container-type-swap",
            Self::ExprToNone => "parity:expr-to-none",
            Self::PositionalDrop => "parity:positional-drop",
            Self::StringCaseSwap => "parity:string-case-swap",
        }
    }

    pub fn is_experimental(self) -> bool {
        matches!(
            self,
            Self::ExceptionClassSwap
                | Self::BareExcept
                | Self::ZeroIterationForLoop
                | Self::OneIterationForLoop
                | Self::RaiseFromDrop
                | Self::NumericTypeSwap
                | Self::OptionalTypeDrop
                | Self::ContainerTypeSwap
        )
    }

    /// Parity operators exist only to broaden overlap with other mutation
    /// tools for comparison. Off unless `--parity`; never in a default score.
    pub fn is_parity(self) -> bool {
        matches!(
            self,
            Self::ExprToNone | Self::PositionalDrop | Self::StringCaseSwap
        )
    }

    /// Emitted only for the `minimal` / `full` operator profiles: the
    /// region-model dominators the default catalogue lacks.
    pub fn is_minimal_only(self) -> bool {
        matches!(
            self,
            Self::RelationalToEquality
                | Self::CompareToConst
                | Self::BoolOperandDrop
                | Self::BoolOpToConst
        )
    }

    /// Gated by the operator profile rather than on by default: the
    /// minimal-only operators plus [`Operator::RegionRest`].
    pub fn is_profile_gated(self) -> bool {
        self.is_minimal_only() || self == Self::RegionRest
    }

    pub fn all() -> &'static [Operator] {
        &[
            Operator::ArithOpSwap,
            Operator::CompareOpSwap,
            Operator::BoolOpSwap,
            Operator::ConstantReplace,
            Operator::BoundaryShift,
            Operator::AugAssignSwap,
            Operator::UnaryOpSwap,
            Operator::NumberShift,
            Operator::ReturnValueToNone,
            Operator::BreakContinueSwap,
            Operator::NotInsertion,
            Operator::RemoveDecorator,
            Operator::DefaultArgToNone,
            Operator::LambdaBodyToNone,
            Operator::SliceBoundDrop,
            Operator::SliceStepMutate,
            Operator::AssignValueToNone,
            Operator::NumberToZero,
            Operator::NumberToNeg,
            Operator::StringToEmpty,
            Operator::StringSentinel,
            Operator::BytesSentinel,
            Operator::KeywordArgDrop,
            Operator::DictItemDrop,
            Operator::ArgToNone,
            Operator::NoneToValue,
            Operator::AwaitDrop,
            Operator::AsyncForToSync,
            Operator::AsyncWithToSync,
            Operator::MatchGuardNegate,
            Operator::RelationalToEquality,
            Operator::CompareToConst,
            Operator::BoolOperandDrop,
            Operator::BoolOpToConst,
            Operator::RegionRest,
            Operator::ExceptionClassSwap,
            Operator::BareExcept,
            Operator::ZeroIterationForLoop,
            Operator::OneIterationForLoop,
            Operator::RaiseFromDrop,
            Operator::NumericTypeSwap,
            Operator::OptionalTypeDrop,
            Operator::ContainerTypeSwap,
            Operator::ExprToNone,
            Operator::PositionalDrop,
            Operator::StringCaseSwap,
        ]
    }

    /// Look up an operator by its canonical name. Accepts the bare name with
    /// or without the `exp:` / `parity:` prefix used by gated operators.
    pub fn from_name(s: &str) -> Option<Operator> {
        fn strip(n: &str) -> &str {
            n.strip_prefix("exp:")
                .or_else(|| n.strip_prefix("parity:"))
                .unwrap_or(n)
        }
        let key = strip(s.trim());
        Self::all().iter().copied().find(|o| strip(o.name()) == key)
    }
}

/// Binary-op swaps (arithmetic + bitwise + shift). Uses one operator label.
pub const ARITH_SWAPS: &[(&str, &str)] = &[
    ("+", "-"),
    ("-", "+"),
    ("*", "/"),
    ("/", "*"),
    ("//", "/"),
    ("%", "*"),
    ("**", "*"),
    ("&", "|"),
    ("|", "&"),
    ("^", "&"),
    ("<<", ">>"),
    (">>", "<<"),
];

pub const COMPARE_SWAPS: &[(&str, &str)] = &[
    ("==", "!="),
    ("!=", "=="),
    ("<", ">="),
    (">", "<="),
    ("<", ">"),
    (">", "<"),
    ("<=", ">="),
    (">=", "<="),
    ("is", "is not"),
    ("is not", "is"),
    ("in", "not in"),
    ("not in", "in"),
];

pub const BOUNDARY_SWAPS: &[(&str, &str)] = &[("<", "<="), ("<=", "<"), (">", ">="), (">=", ">")];

pub const BOOL_SWAPS: &[(&str, &str)] = &[("and", "or"), ("or", "and")];

pub const AUG_ASSIGN_SWAPS: &[(&str, &str)] = &[
    ("+=", "-="),
    ("-=", "+="),
    ("*=", "/="),
    ("/=", "*="),
    ("//=", "/="),
    ("%=", "*="),
    ("**=", "*="),
    ("&=", "|="),
    ("|=", "&="),
    ("^=", "&="),
    ("<<=", ">>="),
    (">>=", "<<="),
];

pub const CONSTANT_SWAPS: &[(&str, &str)] =
    &[("True", "False"), ("False", "True"), ("\"\"", "\"fermut\"")];

/// Builtin container-type swaps for annotations (`list[int]` → `tuple[int]`).
/// Restricted to always-in-scope, subscriptable builtins (3.9+) so a swapped
/// annotation never introduces an undefined name — no import assumptions. Used
/// only in annotation position by `ContainerTypeSwap`.
pub const CONTAINER_TYPE_SWAPS: &[(&str, &str)] = &[
    ("list", "tuple"),
    ("tuple", "list"),
    ("set", "frozenset"),
    ("frozenset", "set"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn name_is_unique_per_variant() {
        let mut seen = HashSet::new();
        for op in Operator::all() {
            assert!(seen.insert(op.name()), "duplicate name: {}", op.name());
        }
    }

    #[test]
    fn all_covers_every_variant() {
        // Cheap proxy: kick `name()` on every variant via `all()` and assert
        // none panic and the count matches the expected total (30 stable + 5
        // profile-gated + 8 experimental + 3 parity). Update when
        // adding/removing operators.
        assert_eq!(Operator::all().len(), 46);
        let gated = Operator::all()
            .iter()
            .filter(|o| o.is_profile_gated())
            .count();
        assert_eq!(gated, 5);
        let experimental_count = Operator::all()
            .iter()
            .filter(|o| o.is_experimental())
            .count();
        assert_eq!(experimental_count, 8);
        let parity_count = Operator::all().iter().filter(|o| o.is_parity()).count();
        assert_eq!(parity_count, 3);
    }

    #[test]
    fn from_name_roundtrip() {
        for op in Operator::all() {
            assert_eq!(Operator::from_name(op.name()), Some(*op));
            // Also without the exp: prefix.
            let bare = op.name().strip_prefix("exp:").unwrap_or(op.name());
            assert_eq!(Operator::from_name(bare), Some(*op));
        }
    }

    /// Stable operator names (no `exp:` / `parity:` prefix), from the enum.
    fn stable_op_names() -> HashSet<String> {
        Operator::all()
            .iter()
            .filter(|o| !o.is_experimental() && !o.is_parity() && !o.is_profile_gated())
            .map(|o| o.name().to_string())
            .collect()
    }

    /// Every number that immediately precedes the word "stable" in `text`
    /// (e.g. "26 stable operators" → 26). Used to catch prose like
    /// "24 stable operators" drifting from the enum.
    fn stable_count_claims(text: &str) -> Vec<usize> {
        let b = text.as_bytes();
        let needle = b" stable";
        let mut out = Vec::new();
        let mut i = 0;
        while i + needle.len() <= b.len() {
            if &b[i..i + needle.len()] == needle {
                let mut j = i; // index of the space before "stable"
                while j > 0 && b[j - 1].is_ascii_digit() {
                    j -= 1;
                }
                if j < i {
                    out.push(std::str::from_utf8(&b[j..i]).unwrap().parse().unwrap());
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }

    /// Guard against the docs drifting from the operator catalogue — the exact
    /// failure the "24 stable" bug was (enum said 26, five docs + a comparison
    /// table said 24). Binds three doc surfaces to `Operator::all()`:
    ///   1. `stable.md`'s table must list EXACTLY the stable operators.
    ///   2. Every "N stable" prose claim across docs must equal the real count.
    ///   3. The landscape comparison row's count cell must equal it too.
    #[test]
    fn docs_stay_in_sync_with_operator_catalogue() {
        use walkdir::WalkDir;

        let stable = stable_op_names();
        let stable_count = stable.len();
        let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/docs");

        // 1. stable.md table == enum stable set.
        let stable_md = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/reference/operators/stable.md"
        ))
        .expect("read stable.md");
        let documented: HashSet<String> = stable_md
            .lines()
            .filter_map(|l| {
                // Table rows look like: `| \`op-name\` | example |`
                let l = l.trim_start();
                let rest = l.strip_prefix("| `")?;
                let end = rest.find('`')?;
                Some(rest[..end].to_string())
            })
            .filter(|name| stable.contains(name) || !name.contains(' '))
            .collect();
        assert_eq!(
            documented,
            stable,
            "stable.md table is out of sync with the Operator enum.\n  \
             missing from doc: {:?}\n  extra in doc: {:?}",
            stable.difference(&documented).collect::<Vec<_>>(),
            documented.difference(&stable).collect::<Vec<_>>(),
        );

        // 2. Every "N stable" prose claim across all docs.
        for entry in WalkDir::new(docs).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for n in stable_count_claims(&text) {
                assert_eq!(
                    n,
                    stable_count,
                    "`{n} stable` in {} disagrees with the enum ({stable_count} stable). \
                     Update the doc (or the enum).",
                    path.display(),
                );
            }
        }

        // 3. Landscape comparison table: the row labelled
        //    "Operator catalogue (stable)" — its first integer cell.
        let landscape = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/concepts/landscape.md"
        ))
        .expect("read landscape.md");
        let row = landscape
            .lines()
            .find(|l| l.contains("Operator catalogue (stable)"))
            .expect("landscape.md has an 'Operator catalogue (stable)' row");
        let first_int: usize = row
            .split(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .expect("a number in the catalogue row")
            .parse()
            .unwrap();
        assert_eq!(
            first_int, stable_count,
            "landscape.md comparison table says {first_int} stable operators; enum has {stable_count}"
        );
    }

    /// Leading `X.Y.Z` semver at the start of `s`, if any.
    fn leading_semver(s: &str) -> Option<String> {
        let tok: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let parts: Vec<&str> = tok.split('.').collect();
        if parts.len() >= 3
            && parts[..3]
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            Some(format!("{}.{}.{}", parts[0], parts[1], parts[2]))
        } else {
            None
        }
    }

    /// Guard against doc version strings drifting from the crate version — the
    /// `0.1.0` vs `0.2.2` bug. Scans docs for `fermut <semver>` / `fermut is at
    /// <semver>` and asserts each equals `CARGO_PKG_VERSION`. Bare semvers
    /// (mutmut 3.6.0, typer 0.26.7, …) are ignored — only versions attached to
    /// "fermut" count.
    #[test]
    fn docs_fermut_version_matches_crate() {
        use walkdir::WalkDir;
        let version = env!("CARGO_PKG_VERSION");
        let docs = concat!(env!("CARGO_MANIFEST_DIR"), "/docs");
        for entry in WalkDir::new(docs).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for (i, _) in text.match_indices("fermut") {
                let rest = text[i + "fermut".len()..].trim_start();
                let rest = rest
                    .strip_prefix("is at ")
                    .map(str::trim_start)
                    .unwrap_or(rest);
                if let Some(v) = leading_semver(rest) {
                    assert_eq!(
                        v,
                        version,
                        "{} says `fermut {v}` but the crate is {version}",
                        path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn from_name_rejects_unknown() {
        assert_eq!(Operator::from_name("bogus"), None);
        assert_eq!(Operator::from_name(""), None);
    }

    #[test]
    fn is_experimental_matches_exp_prefix() {
        for op in Operator::all() {
            let prefixed = op.name().starts_with("exp:");
            assert_eq!(
                op.is_experimental(),
                prefixed,
                "operator {:?} prefix/flag mismatch",
                op
            );
        }
    }

    #[test]
    fn is_parity_matches_parity_prefix() {
        for op in Operator::all() {
            assert_eq!(
                op.is_parity(),
                op.name().starts_with("parity:"),
                "operator {:?} parity prefix/flag mismatch",
                op
            );
        }
    }

    #[test]
    fn swap_tables_have_no_identity_pairs() {
        for (orig, repl) in ARITH_SWAPS
            .iter()
            .chain(COMPARE_SWAPS)
            .chain(BOUNDARY_SWAPS)
            .chain(BOOL_SWAPS)
            .chain(AUG_ASSIGN_SWAPS)
            .chain(CONSTANT_SWAPS)
            .chain(CONTAINER_TYPE_SWAPS)
        {
            assert_ne!(orig, repl, "identity swap entry {orig:?} -> {repl:?}");
        }
    }
}
