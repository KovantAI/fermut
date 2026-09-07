//! Lexeme helpers for the mutation visitor: operator → source-text mapping and
//! string-literal transforms (sentinel wrapping, case swap). Split out of
//! `visitor.rs` so the AST-walking `Collector` and these pure text utilities
//! stay separately legible.

use ruff_python_ast::{self as ast};

pub(super) fn binop_lexeme(op: ast::Operator) -> &'static str {
    use ast::Operator::*;
    match op {
        Add => "+",
        Sub => "-",
        Mult => "*",
        Div => "/",
        FloorDiv => "//",
        Mod => "%",
        Pow => "**",
        MatMult => "@",
        LShift => "<<",
        RShift => ">>",
        BitOr => "|",
        BitXor => "^",
        BitAnd => "&",
    }
}

pub(super) fn augop_lexeme(op: ast::Operator) -> &'static str {
    use ast::Operator::*;
    match op {
        Add => "+=",
        Sub => "-=",
        Mult => "*=",
        Div => "/=",
        FloorDiv => "//=",
        Mod => "%=",
        Pow => "**=",
        MatMult => "@=",
        LShift => "<<=",
        RShift => ">>=",
        BitOr => "|=",
        BitXor => "^=",
        BitAnd => "&=",
    }
}

pub(super) fn is_empty_string_literal(lex: &str) -> bool {
    // Quoted empty strings in any of Python's literal forms.
    matches!(
        lex,
        "\"\""
            | "''"
            | "\"\"\"\"\"\""
            | "''''''"
            | "r\"\""
            | "r''"
            | "rb\"\""
            | "rb''"
            | "br\"\""
            | "br''"
    )
}

pub(super) fn is_empty_bytes_literal(lex: &str) -> bool {
    matches!(
        lex,
        "b\"\"" | "b''" | "B\"\"" | "B''" | "rb\"\"" | "rb''" | "br\"\"" | "br''"
    )
}

/// Wrap the *content* of a quoted literal with `XX` sentinel markers,
/// preserving prefix letters (r/b/u/B/rb/br/...) and quote style (single,
/// double, triple). Returns `None` if `lex` doesn't look like a quoted
/// literal we recognize.
pub(super) fn wrap_with_sentinel(lex: &str) -> Option<String> {
    let bytes = lex.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let q = bytes[i];
    if q != b'"' && q != b'\'' {
        return None;
    }
    let triple = i + 3 <= bytes.len() && bytes[i + 1] == q && bytes[i + 2] == q;
    let q_len = if triple { 3 } else { 1 };
    let open_end = i + q_len;
    if lex.len() < open_end + q_len {
        return None;
    }
    let close_start = lex.len() - q_len;
    if close_start < open_end {
        return None;
    }
    let mut out = String::with_capacity(lex.len() + 4);
    out.push_str(&lex[..open_end]);
    out.push_str("XX");
    out.push_str(&lex[open_end..close_start]);
    out.push_str("XX");
    out.push_str(&lex[close_start..]);
    Some(out)
}

/// UPPER- or lower-case the *content* of a quoted literal, preserving prefix
/// letters and quotes. Returns `None` if `lex` isn't a recognized literal, the
/// content is unchanged by the swap, or it contains a backslash (case-swapping
/// an escape like `\n`→`\N` would change or break the string). Parity-only.
pub(super) fn swap_string_case(lex: &str, upper: bool) -> Option<String> {
    let bytes = lex.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let q = bytes[i];
    if q != b'"' && q != b'\'' {
        return None;
    }
    let triple = i + 3 <= bytes.len() && bytes[i + 1] == q && bytes[i + 2] == q;
    let q_len = if triple { 3 } else { 1 };
    let open_end = i + q_len;
    if lex.len() < open_end + q_len {
        return None;
    }
    let close_start = lex.len() - q_len;
    if close_start < open_end {
        return None;
    }
    let content = &lex[open_end..close_start];
    if content.contains('\\') {
        return None;
    }
    let swapped = if upper {
        content.to_uppercase()
    } else {
        content.to_lowercase()
    };
    if swapped == content {
        return None;
    }
    Some(format!(
        "{}{}{}",
        &lex[..open_end],
        swapped,
        &lex[close_start..]
    ))
}

pub(super) fn cmpop_lexeme(op: ast::CmpOp) -> &'static str {
    use ast::CmpOp::*;
    match op {
        Eq => "==",
        NotEq => "!=",
        Lt => "<",
        LtE => "<=",
        Gt => ">",
        GtE => ">=",
        Is => "is",
        IsNot => "is not",
        In => "in",
        NotIn => "not in",
    }
}

#[cfg(test)]
mod sentinel_tests {
    use super::wrap_with_sentinel;

    #[test]
    fn double_quoted() {
        assert_eq!(
            wrap_with_sentinel("\"foo\"").as_deref(),
            Some("\"XXfooXX\"")
        );
    }

    #[test]
    fn single_quoted() {
        assert_eq!(wrap_with_sentinel("'foo'").as_deref(), Some("'XXfooXX'"));
    }

    #[test]
    fn triple_quoted() {
        assert_eq!(
            wrap_with_sentinel("\"\"\"foo\"\"\"").as_deref(),
            Some("\"\"\"XXfooXX\"\"\"")
        );
    }

    #[test]
    fn bytes() {
        assert_eq!(
            wrap_with_sentinel("b\"foo\"").as_deref(),
            Some("b\"XXfooXX\"")
        );
    }

    #[test]
    fn raw_bytes() {
        assert_eq!(
            wrap_with_sentinel("rb\"foo\"").as_deref(),
            Some("rb\"XXfooXX\"")
        );
    }
}
