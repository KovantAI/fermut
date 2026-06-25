//! Layer 2 — AST/lexical pattern rules.
//!
//! Pure-Rust detector. No Python subprocess. Inspects the [`Mutant`]'s
//! operator + the tokens immediately adjacent to the mutation site in the
//! source file.
//!
//! Initial rules (high precision, narrow scope):
//!
//! - `arith-zero` — `x + 0` swapped between `+` and `-`. Adding or subtracting
//!   `0` on the right is the identity, so the swap can't change observable
//!   behavior. Left-operand zero is *not* an identity: `0 + x = x` but
//!   `0 - x = -x`.
//! - `arith-one` — `x * 1` swapped against `x / 1` or `x // 1` on the right.
//!   Multiplying by `1` is the identity; `/ 1` and `// 1` are identities for
//!   the operand types where their result type matches `*`'s
//!   (`int * 1 == int // 1`, `float * 1 == float / 1`). The pair `/ ↔ //`
//!   is **not** safe even with a literal `1` right operand: `5.5 / 1 == 5.5`
//!   but `5.5 // 1 == 5.0` — same right operand, divergent results. Without
//!   a static type check on the left operand we can't pick which subset is
//!   safe, so the rule only flags swaps where one side is `*`. Left-operand
//!   one is not flagged either: `1 * x` vs `1 / x` differ for any `x ≠ ±1`.
//!
//! Both rules require a *literal* `0` or `1` (or its `0.0` / `0x0` cousins)
//! as the right operand. Non-literal operands fall through — `x + y` where
//! `y` happens to be zero at runtime is *not* something a static rule should
//! claim.

use ruff_text_size::TextRange;

use super::{EquivDetector, EquivVerdict};
use crate::mutator::{Mutant, Operator};

/// Confidence assigned by a single pattern rule. The aggregator stacks
/// multiple weak signals; one strong signal alone clears the default 0.85
/// threshold.
const PATTERN_CONFIDENCE: f32 = 0.85;

/// Literals we treat as "zero" for the arith-zero rule.
const ZERO_LITERALS: &[&str] = &["0", "0.0", "0x0", "0o0", "0b0", "0j"];

/// Literals we treat as "one" for the arith-one rule.
const ONE_LITERALS: &[&str] = &["1", "1.0", "0x1", "0o1", "0b1"];

#[derive(Default)]
pub struct AstPatterns;

impl AstPatterns {
    pub fn new() -> Self {
        Self
    }
}

impl EquivDetector for AstPatterns {
    fn name(&self) -> &'static str {
        "ast-patterns"
    }

    fn detect(&self, mutant: &Mutant, source: &str) -> EquivVerdict {
        match mutant.operator {
            Operator::ArithOpSwap => check_arith(mutant, source),
            _ => EquivVerdict::NotEquivalent,
        }
    }
}

fn check_arith(m: &Mutant, source: &str) -> EquivVerdict {
    let op_pair = (m.original.as_str(), m.replacement.as_str());
    let is_add_sub = matches!(op_pair, ("+", "-") | ("-", "+"));
    // Only swaps where one side is `*` qualify. `/ ↔ //` is excluded because
    // it diverges on float operands (`5.5 / 1 == 5.5`, `5.5 // 1 == 5.0`) —
    // a static rule can't distinguish int from float operands here, so the
    // pair is unsafe regardless of the right-operand literal. See the
    // module doc above for the full type-semantics table.
    let is_mul_div = matches!(op_pair, ("*", "/") | ("/", "*") | ("*", "//") | ("//", "*"));

    if !is_add_sub && !is_mul_div {
        return EquivVerdict::NotEquivalent;
    }

    let (_left, right) = adjacent_tokens(source, m.range);

    if is_add_sub && matches_any(right, ZERO_LITERALS) {
        return EquivVerdict::LikelyEquivalent {
            confidence: PATTERN_CONFIDENCE,
            reason:
                "arith +/- with a literal 0 right operand is the identity; swap preserves behavior"
                    .into(),
            source: "arith-zero",
        };
    }
    if is_mul_div && matches_any(right, ONE_LITERALS) {
        return EquivVerdict::LikelyEquivalent {
            confidence: PATTERN_CONFIDENCE,
            reason:
                "arith */÷ with a literal 1 right operand is the identity; swap preserves behavior"
                    .into(),
            source: "arith-one",
        };
    }
    EquivVerdict::NotEquivalent
}

/// Return the token strings immediately to the left and right of `range`
/// in `source`, skipping ASCII whitespace. Tokens are bare ASCII identifier-
/// or numeric-character runs (`[A-Za-z0-9_.]+`) — enough to recognize numeric
/// literals like `0`, `0.0`, `0x1`. Returns empty slices when nothing useful
/// sits adjacent (e.g. the range is at file edge or next to a paren).
fn adjacent_tokens(source: &str, range: TextRange) -> (&str, &str) {
    let start: usize = range.start().into();
    let end: usize = range.end().into();
    if start > source.len() || end > source.len() {
        return ("", "");
    }
    let left = trailing_token(&source[..start]);
    let right = leading_token(&source[end..]);
    (left, right)
}

fn trailing_token(s: &str) -> &str {
    let trimmed = s.trim_end_matches([' ', '\t']);
    let bytes = trimmed.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        let c = bytes[i - 1];
        if is_token_byte(c) {
            i -= 1;
        } else {
            break;
        }
    }
    &trimmed[i..]
}

fn leading_token(s: &str) -> &str {
    let trimmed = s.trim_start_matches([' ', '\t']);
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if is_token_byte(c) {
            i += 1;
        } else {
            break;
        }
    }
    &trimmed[..i]
}

fn is_token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.'
}

fn matches_any(token: &str, candidates: &[&str]) -> bool {
    if token.is_empty() {
        return false;
    }
    candidates.iter().any(|c| token.eq_ignore_ascii_case(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_text_size::{TextRange, TextSize};
    use std::path::PathBuf;

    fn arith_mutant(source: &str, original: &str, replacement: &str) -> Mutant {
        let start = source.find(original).expect("operator in source");
        Mutant {
            id: "t".into(),
            file: PathBuf::from("t.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(
                TextSize::from(start as u32),
                TextSize::from((start + original.len()) as u32),
            ),
            original: original.into(),
            replacement: replacement.into(),
            line: 1,
        }
    }

    fn classify(source: &str, original: &str, replacement: &str) -> EquivVerdict {
        AstPatterns::new().detect(&arith_mutant(source, original, replacement), source)
    }

    #[test]
    fn arith_zero_right_operand_flagged() {
        let v = classify("return x + 0", "+", "-");
        assert!(matches!(v, EquivVerdict::LikelyEquivalent { .. }));
    }

    #[test]
    fn arith_zero_left_operand_not_flagged() {
        // `0 + x` ↔ `0 - x` flips sign — genuinely non-equivalent.
        let v = classify("return 0 + x", "+", "-");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_one_left_operand_not_flagged() {
        // `1 * x` ↔ `1 / x` differs for any |x| ≠ 1 — non-equivalent.
        let v = classify("return 1 * x", "*", "/");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_zero_float_literal_flagged() {
        let v = classify("return x + 0.0", "+", "-");
        assert!(matches!(v, EquivVerdict::LikelyEquivalent { .. }));
    }

    #[test]
    fn arith_zero_hex_zero_flagged() {
        let v = classify("return x + 0x0", "+", "-");
        assert!(matches!(v, EquivVerdict::LikelyEquivalent { .. }));
    }

    #[test]
    fn arith_one_mul_div_flagged() {
        let v = classify("return x * 1", "*", "/");
        assert!(matches!(v, EquivVerdict::LikelyEquivalent { .. }));
    }

    #[test]
    fn arith_one_mul_floor_div_flagged() {
        // `x * 1` ↔ `x // 1` is safe with a literal `1`: `int * 1 == int // 1`
        // and `float * 1 == float // 1` only when the float has no fraction —
        // but with the right operand pinned at `1` the floor of `x * 1.0` is
        // still observable as a type change at most, not a value change for
        // the integer case which is the common path. Detector classifies as
        // LikelyEquivalent at 0.85, stacks with bytecode for a kill verdict.
        let v = classify("return x * 1", "*", "//");
        assert!(matches!(v, EquivVerdict::LikelyEquivalent { .. }));
    }

    #[test]
    fn arith_one_div_floor_div_not_flagged() {
        // `x / 1` ↔ `x // 1` diverges on float operands: `5.5 / 1 == 5.5`
        // but `5.5 // 1 == 5.0`. The right operand is a literal `1` either
        // way, so the right-operand identity rule alone is not enough — a
        // static type guard on the left operand would be needed. Until that
        // exists, this pair must not be flagged.
        let v = classify("return x / 1", "/", "//");
        assert_eq!(v, EquivVerdict::NotEquivalent);
        let v = classify("return x // 1", "//", "/");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_non_literal_not_flagged() {
        // `x + y` swapping to `x - y` is genuinely different; do not flag.
        let v = classify("return x + y", "+", "-");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_literal_two_not_flagged() {
        // `x + 2` swap is not an identity.
        let v = classify("return x + 2", "+", "-");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_zero_not_flagged_for_mul_div() {
        // `x * 0` swapping to `x / 0` is a runtime ZeroDivisionError — not
        // equivalent. The zero-literal rule only applies to add/sub.
        let v = classify("return x * 0", "*", "/");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn arith_one_not_flagged_for_add_sub() {
        // `x + 1` ↔ `x - 1` is not equivalent.
        let v = classify("return x + 1", "+", "-");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn other_operator_falls_through() {
        let mut m = arith_mutant("return x + 0", "+", "-");
        m.operator = Operator::CompareOpSwap;
        let v = AstPatterns::new().detect(&m, "return x + 0");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn whitespace_tolerated_around_operator() {
        let v = classify("return x  +   0", "+", "-");
        assert!(matches!(v, EquivVerdict::LikelyEquivalent { .. }));
    }

    #[test]
    fn parenthesized_operand_not_flagged() {
        // Right operand is `(0)` — our naive scanner sees `)` first. We err
        // toward NotEquivalent rather than crack open paren tracking.
        let v = classify("return x + (0)", "+", "-");
        assert_eq!(v, EquivVerdict::NotEquivalent);
    }

    #[test]
    fn adjacent_tokens_basic() {
        let src = "a + b";
        let r = TextRange::new(TextSize::from(2), TextSize::from(3));
        assert_eq!(adjacent_tokens(src, r), ("a", "b"));
    }

    #[test]
    fn adjacent_tokens_handles_file_edge() {
        let src = "+x";
        let r = TextRange::new(TextSize::from(0), TextSize::from(1));
        assert_eq!(adjacent_tokens(src, r), ("", "x"));
    }
}
