//! AST traversal that emits mutation candidates.
//!
//! NOTE on ruff API: the type paths below follow `ruff_python_ast` 0.15.x.
//! If you bump the pinned tag in `Cargo.toml`, re-verify against the
//! corresponding `ruff_python_ast` source.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ruff_python_ast::{
    self as ast,
    visitor::source_order::{walk_expr, walk_stmt, SourceOrderVisitor},
    Expr, Stmt,
};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ignore::{collect_from_tokens, IgnoreMap};
use super::lexeme::{
    augop_lexeme, binop_lexeme, cmpop_lexeme, is_empty_bytes_literal, is_empty_string_literal,
    swap_string_case, wrap_with_sentinel,
};
use super::operators::{
    Operator, ARITH_SWAPS, AUG_ASSIGN_SWAPS, BOOL_SWAPS, BOUNDARY_SWAPS, COMPARE_SWAPS,
    CONSTANT_SWAPS, CONTAINER_TYPE_SWAPS,
};
use super::Mutant;

pub fn collect(path: &Path, source: &str) -> Result<Vec<Mutant>> {
    let parsed = parse_module(source).with_context(|| format!("parsing {}", path.display()))?;
    let module = parsed.syntax();

    let mut docstrings: HashSet<TextRange> = HashSet::new();
    collect_docstring_ranges(&module.body, &mut docstrings);

    let ignores = collect_from_tokens(source, parsed.tokens());

    let mut collector = Collector {
        file: path.to_path_buf(),
        source,
        line_starts: line_start_offsets(source),
        out: Vec::new(),
        docstrings,
        ignores,
        annotation_ranges: Vec::new(),
        stmt_line: 0,
    };
    for stmt in &module.body {
        collector.visit_stmt(stmt);
    }
    Ok(collector.out)
}

/// Walk all function/class/module bodies and record the range of the leading
/// `Stmt::Expr(StringLiteral)` (the docstring) so later passes can skip them.
fn collect_docstring_ranges(body: &[Stmt], out: &mut HashSet<TextRange>) {
    if let Some(Stmt::Expr(e)) = body.first() {
        if let Expr::StringLiteral(_) = e.value.as_ref() {
            out.insert(e.value.range());
        }
    }
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(f) => collect_docstring_ranges(&f.body, out),
            Stmt::ClassDef(c) => collect_docstring_ranges(&c.body, out),
            _ => {}
        }
    }
}

/// Byte offset of the first character of each line (0-based). `line_starts[0]`
/// is always 0; entry `i` is the offset just past the `i`-th `\n`. Precomputed
/// once so `line_of` is a binary search instead of an O(n) rescan per lookup.
fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter(|&(_, b)| b == b'\n')
            .map(|(i, _)| i + 1),
    );
    starts
}

/// Classify an underscore-stripped Python integer lexeme as `(digits, radix)`
/// for [`i64::from_str_radix`]. Returns `None` for anything that isn't a plain
/// integer — floats, exponents (`1e3`), or lexemes with stray characters — so
/// the caller falls through to float parsing.
fn split_radix(cleaned: &str) -> Option<(&str, u32)> {
    if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        return Some((rest, 16));
    }
    if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        return Some((rest, 8));
    }
    if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        return Some((rest, 2));
    }
    // Plain decimal only if every char is a digit; `1.5` / `1e3` fall through.
    if !cleaned.is_empty() && cleaned.bytes().all(|b| b.is_ascii_digit()) {
        return Some((cleaned, 10));
    }
    None
}

/// Format an `i128` back into a literal of the given radix, preserving the
/// `0x`/`0o`/`0b` prefix (and sign) so a shifted `0xFF` reads as `0x100`. Widened
/// past `i64` so literals above `i64::MAX` (e.g. `99999999999999999999`) still
/// produce int-typed mutants instead of being silently dropped.
fn format_int(x: i128, radix: u32) -> String {
    let (sign, mag) = if x < 0 {
        ("-", x.unsigned_abs())
    } else {
        ("", x as u128)
    };
    match radix {
        16 => format!("{sign}0x{mag:X}"),
        8 => format!("{sign}0o{mag:o}"),
        2 => format!("{sign}0b{mag:b}"),
        _ => x.to_string(),
    }
}

/// Format an `f64` as a Python float literal, ensuring a decimal point survives
/// so integer-valued results (`6.0`) don't collapse to `int` syntax (`6`) and
/// silently change the literal's type.
fn format_float(x: f64) -> String {
    let s = x.to_string();
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

struct Collector<'a> {
    file: PathBuf,
    source: &'a str,
    /// See [`line_start_offsets`].
    line_starts: Vec<usize>,
    out: Vec<Mutant>,
    docstrings: HashSet<TextRange>,
    ignores: IgnoreMap,
    /// Ranges of union-annotation tokens whose generic mutation is invalid: each
    /// `|` operator (arith swap → `&`) and each `None` arm (none-to-value), both
    /// of which would raise at def-time. `push` suppresses any operator landing
    /// inside one. Scoped to those exact tokens so `Literal[N]` / `Annotated[..]`
    /// values elsewhere in the annotation still mutate. See `push`.
    annotation_ranges: Vec<TextRange>,
    /// First line of the statement currently being visited. Mutants record it
    /// so coverage lookups can fall back to the line coverage.py actually
    /// attributes execution to — see `Mutant::stmt_line`.
    stmt_line: u32,
}

impl<'a> Collector<'a> {
    fn line_of(&self, range: TextRange) -> u32 {
        let start: usize = range.start().into();
        // Number of line-starts at or before `start` == 1-based line number.
        // `partition_point` returns the count of entries `<= start`.
        self.line_starts.partition_point(|&off| off <= start) as u32
    }

    fn push(&mut self, op: Operator, range: TextRange, replacement: &str) {
        let line = self.line_of(range);
        if self.ignores.is_ignored(line, op) {
            return;
        }
        // `annotation_ranges` holds the `|` and `None`-arm tokens of union
        // annotations, whose generic mutation raises at def-time (`str & None` /
        // `str | ""`). Suppress any operator landing inside one; the dedicated
        // type-annotation operators are exempt (they emit the valid union drop).
        // Ranges are token-precise, so `Literal[N]` / `Annotated[..]` values —
        // even inside the same union — still mutate.
        let is_annotation_op = matches!(
            op,
            Operator::NumericTypeSwap | Operator::OptionalTypeDrop | Operator::ContainerTypeSwap
        );
        if !is_annotation_op
            && self
                .annotation_ranges
                .iter()
                .any(|a| a.contains_range(range))
        {
            return;
        }
        let original = &self.source[range];
        // Operator is part of the id because two distinct operators can
        // legitimately propose the same (range, original, replacement)
        // tuple — e.g. NumberShift and NumberToZero both emit `1->0` on a
        // literal `1`. Without the operator tag they'd share an id, the
        // cache would conflate their outcomes, and a `filter --ops` run
        // would silently flip verdicts depending on which mutant landed
        // first in iteration order.
        let id = format!(
            "{}@{}:{}:{}->{}",
            self.file.display(),
            u32::from(range.start()),
            op.name(),
            original,
            replacement
        );
        self.out.push(Mutant {
            id,
            file: self.file.clone(),
            operator: op,
            range,
            original: original.to_string(),
            replacement: replacement.to_string(),
            line,
            // Every mutant is emitted while visiting a statement, so this is
            // set; fall back to `line` rather than fabricate a head line.
            stmt_line: if self.stmt_line == 0 {
                line
            } else {
                self.stmt_line
            },
        });
    }

    /// Expand `inner` to include one adjacent comma (and any whitespace around
    /// it) so dropping a list element doesn't leave a stray `, ,`. Scans
    /// backwards first; falls back to forwards. Returns the extended range,
    /// bounded by `outer`.
    fn extend_to_consume_comma(&self, inner: TextRange, outer: TextRange) -> TextRange {
        let bytes = self.source.as_bytes();
        let inner_start: usize = inner.start().into();
        let inner_end: usize = inner.end().into();
        let outer_start: usize = outer.start().into();
        let outer_end: usize = outer.end().into();

        let mut i = inner_start;
        while i > outer_start {
            i -= 1;
            let b = bytes[i];
            if b == b',' {
                return TextRange::new((i as u32).into(), (inner_end as u32).into());
            }
            if !b.is_ascii_whitespace() {
                break;
            }
        }
        let mut j = inner_end;
        while j < outer_end {
            let b = bytes[j];
            if b == b',' {
                j += 1;
                while j < outer_end && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                return TextRange::new((inner_start as u32).into(), (j as u32).into());
            }
            if !b.is_ascii_whitespace() {
                break;
            }
            j += 1;
        }
        inner
    }

    fn op_range(&self, range: TextRange, lexeme: &str) -> Option<TextRange> {
        let start: usize = range.start().into();
        let end: usize = range.end().into();
        let slice = &self.source[start..end];
        let idx = slice.find(lexeme)?;
        let abs_start = start + idx;
        let abs_end = abs_start + lexeme.len();
        Some(TextRange::new(
            (abs_start as u32).into(),
            (abs_end as u32).into(),
        ))
    }

    fn handle_aug_assign(&mut self, a: &ast::StmtAugAssign) {
        let lex = augop_lexeme(a.op);
        let s: usize = a.target.range().end().into();
        let e: usize = a.value.range().start().into();
        if let Some(idx) = self.source[s..e].find(lex) {
            let abs = s + idx;
            let r = TextRange::new((abs as u32).into(), ((abs + lex.len()) as u32).into());
            for (orig, repl) in AUG_ASSIGN_SWAPS {
                if *orig == lex {
                    self.push(Operator::AugAssignSwap, r, repl);
                }
            }
        }
    }

    fn handle_return(&mut self, r: &ast::StmtReturn) {
        if let Some(value) = &r.value {
            if !matches!(value.as_ref(), Expr::NoneLiteral(_)) {
                self.push(Operator::ReturnValueToNone, value.range(), "None");
            }
        }
    }

    fn handle_not_insertion(&mut self, range: TextRange) {
        let orig = &self.source[range];
        let repl = format!("not ({})", orig);
        self.push(Operator::NotInsertion, range, &repl);
    }

    fn handle_decorators(&mut self, decs: &[ast::Decorator]) {
        let bytes = self.source.as_bytes();
        for d in decs {
            let r_start: usize = d.range().start().into();
            let r_end: usize = d.range().end().into();
            let mut s = r_start;
            while s > 0 && bytes[s - 1] != b'\n' {
                s -= 1;
            }
            let mut e = r_end;
            while e < bytes.len() && bytes[e] != b'\n' {
                e += 1;
            }
            if e < bytes.len() {
                e += 1;
            }
            let range = TextRange::new((s as u32).into(), (e as u32).into());
            self.push(Operator::RemoveDecorator, range, "");
        }
    }

    fn handle_try(&mut self, t: &ast::StmtTry) {
        for handler in &t.handlers {
            let ast::ExceptHandler::ExceptHandler(h) = handler;
            if let Some(type_) = &h.type_ {
                self.push(Operator::ExceptionClassSwap, type_.range(), "Exception");
                if h.name.is_none() {
                    self.push(Operator::BareExcept, type_.range(), "");
                }
            }
        }
    }

    /// Range covering a leading `async` keyword plus the whitespace up to the
    /// following keyword (`for` / `with`), so replacing it with `""` turns an
    /// `async for` / `async with` into its sync form. `None` when the statement
    /// doesn't actually start with `async` (defensive — `is_async` should have
    /// gated the call). The statement range starts at the `async` token.
    fn async_kw_range(&self, stmt_range: TextRange) -> Option<TextRange> {
        let s: usize = stmt_range.start().into();
        if !self.source[s..].starts_with("async") {
            return None;
        }
        let bytes = self.source.as_bytes();
        let mut e = s + "async".len();
        while e < bytes.len() && bytes[e].is_ascii_whitespace() {
            e += 1;
        }
        Some(TextRange::new((s as u32).into(), (e as u32).into()))
    }

    fn handle_match(&mut self, m: &ast::StmtMatch) {
        // Negate each `case ... if <guard>` guard. A guard decides whether the
        // arm fires; wrapping it in `not (...)` flips which case handles the
        // subject — observable whenever a test depends on the branch taken.
        for case in &m.cases {
            if let Some(guard) = &case.guard {
                let r = guard.range();
                let repl = format!("not ({})", &self.source[r]);
                self.push(Operator::MatchGuardNegate, r, &repl);
            }
        }
    }

    fn handle_raise(&mut self, r: &ast::StmtRaise) {
        // `raise X from <cause>` → `raise X`: drop the explicit exception
        // chaining. Catches tests that assert on the exception type but never
        // on `__cause__`. Covers `from None` too (suppressed chaining).
        if let (Some(exc), Some(cause)) = (&r.exc, &r.cause) {
            let range = TextRange::new(exc.range().end(), cause.range().end());
            self.push(Operator::RaiseFromDrop, range, "");
        }
    }

    fn handle_for(&mut self, f: &ast::StmtFor) {
        let r = f.iter.range();
        self.push(Operator::ZeroIterationForLoop, r, "[]");
        let orig = &self.source[r];
        let one = format!("[next(iter({}))]", orig);
        self.push(Operator::OneIterationForLoop, r, &one);
    }

    /// Emit type-annotation mutations over every annotation attached to a
    /// function (each parameter + the return).
    fn handle_param_annotations(&mut self, params: &ast::Parameters, returns: Option<&Expr>) {
        for p in params
            .posonlyargs
            .iter()
            .chain(params.args.iter())
            .chain(params.kwonlyargs.iter())
        {
            if let Some(a) = &p.parameter.annotation {
                self.annotate(a);
            }
        }
        if let Some(p) = &params.vararg {
            if let Some(a) = &p.annotation {
                self.annotate(a);
            }
        }
        if let Some(p) = &params.kwarg {
            if let Some(a) = &p.annotation {
                self.annotate(a);
            }
        }
        if let Some(r) = returns {
            self.annotate(r);
        }
    }

    /// Recurse a type-annotation expression, emitting the three experimental
    /// type-annotation operators at every level:
    /// - `NumericTypeSwap`: `int` ↔ `float` (bare name).
    /// - `ContainerTypeSwap`: builtin container name swap (`list` → `tuple`, …),
    ///   whether bare (`x: list`) or subscripted (`x: list[int]`).
    /// - `OptionalTypeDrop`: `Optional[T]` → `T` and `T | None` → `T`.
    ///
    /// Recurses into subscript slices and `|` unions so nested annotations
    /// (`dict[str, Optional[int]]`) are covered at each layer.
    fn annotate(&mut self, ann: &Expr) {
        match ann {
            Expr::Name(_) => {
                let r = ann.range();
                let lex = &self.source[r];
                match lex {
                    "int" => self.push(Operator::NumericTypeSwap, r, "float"),
                    "float" => self.push(Operator::NumericTypeSwap, r, "int"),
                    _ => {}
                }
                for (orig, repl) in CONTAINER_TYPE_SWAPS {
                    if *orig == lex {
                        self.push(Operator::ContainerTypeSwap, r, repl);
                    }
                }
            }
            Expr::Subscript(s) => {
                // `Optional[T]` → `T`, bare (`Optional`) or qualified
                // (`typing.Optional`, `t.Optional`) — replace the whole
                // subscript with the inner type's source text.
                let is_optional = match s.value.as_ref() {
                    Expr::Name(_) => &self.source[s.value.range()] == "Optional",
                    Expr::Attribute(a) => a.attr.as_str() == "Optional",
                    _ => false,
                };
                if is_optional {
                    let inner = &self.source[s.slice.range()];
                    self.push(Operator::OptionalTypeDrop, s.range(), inner);
                }
                // Container swaps only for bare builtin names — a `typing.`
                // qualifier is not a subscriptable builtin container.
                if let Expr::Name(_) = s.value.as_ref() {
                    let nr = s.value.range();
                    let name = &self.source[nr];
                    for (orig, repl) in CONTAINER_TYPE_SWAPS {
                        if *orig == name {
                            self.push(Operator::ContainerTypeSwap, nr, repl);
                        }
                    }
                }
                // The slice carries the parameter types (`T`, or a tuple of
                // them) — recurse so nested annotations mutate too.
                self.annotate(&s.slice);
            }
            // `dict[str, int]`'s slice is a tuple of the type args.
            Expr::Tuple(t) => {
                for elt in &t.elts {
                    self.annotate(elt);
                }
            }
            // PEP 604 unions: `T | None` → `T` (drop the optional arm).
            Expr::BinOp(b) if matches!(b.op, ast::Operator::BitOr) => {
                // Suppress exactly the two def-time-invalid generic mutations a
                // union annotation attracts, and nothing else: the arith swap on
                // *this* `|`, and none-to-value on a `None` arm (`str | None` →
                // `str & None` / `str | ""`, both raise at def-time). Scoped to
                // just those tokens — a `Literal[N]` / `Annotated[...]` arm keeps
                // mutating. Per-node op lookup handles nested unions correctly.
                let ls: usize = b.left.range().end().into();
                let rs: usize = b.right.range().start().into();
                if let Some(idx) = self.source[ls..rs].find('|') {
                    let a = ls + idx;
                    self.annotation_ranges
                        .push(TextRange::new((a as u32).into(), ((a + 1) as u32).into()));
                }
                if matches!(b.right.as_ref(), Expr::NoneLiteral(_)) {
                    self.annotation_ranges.push(b.right.range());
                    let keep = &self.source[b.left.range()];
                    self.push(Operator::OptionalTypeDrop, b.range(), keep);
                } else if matches!(b.left.as_ref(), Expr::NoneLiteral(_)) {
                    self.annotation_ranges.push(b.left.range());
                    let keep = &self.source[b.right.range()];
                    self.push(Operator::OptionalTypeDrop, b.range(), keep);
                }
                self.annotate(&b.left);
                self.annotate(&b.right);
            }
            _ => {}
        }
    }

    fn handle_param_defaults(&mut self, params: &ast::Parameters) {
        let all = params
            .posonlyargs
            .iter()
            .chain(params.args.iter())
            .chain(params.kwonlyargs.iter());
        for param in all {
            if let Some(default) = &param.default {
                if !matches!(default.as_ref(), Expr::NoneLiteral(_)) {
                    self.push(Operator::DefaultArgToNone, default.range(), "None");
                }
            }
        }
    }

    // --- per-expression emit helpers -------------------------------------
    // One helper per `Expr::` variant handled by `visit_expr`. Each owns the
    // mutation logic for its node; `visit_expr` is a thin dispatcher that
    // matches the variant (with its guard) and delegates here.

    fn emit_binop(&mut self, b: &ast::ExprBinOp) {
        let lex = binop_lexeme(b.op);
        if let Some(r) = self.op_range(b.range(), lex) {
            for (orig, repl) in ARITH_SWAPS {
                if *orig == lex {
                    self.push(Operator::ArithOpSwap, r, repl);
                }
            }
        }
    }

    fn emit_boolop(&mut self, b: &ast::ExprBoolOp) {
        let lex = if matches!(b.op, ast::BoolOp::And) {
            "and"
        } else {
            "or"
        };
        if let Some(r) = self.op_range(b.range(), lex) {
            for (orig, repl) in BOOL_SWAPS {
                if *orig == lex {
                    self.push(Operator::BoolOpSwap, r, repl);
                }
            }
        }
    }

    fn emit_unaryop(&mut self, u: &ast::ExprUnaryOp) {
        use ast::UnaryOp::*;
        let start = u.range().start();
        match u.op {
            USub | UAdd => {
                let end = TextSize::from(u32::from(start) + 1);
                let r = TextRange::new(start, end);
                let lex = &self.source[r];
                let repl = if lex == "-" { "+" } else { "-" };
                self.push(Operator::UnaryOpSwap, r, repl);
            }
            Not => {
                let r = TextRange::new(start, u.operand.range().start());
                self.push(Operator::UnaryOpSwap, r, "");
            }
            Invert => {
                let end = TextSize::from(u32::from(start) + 1);
                let r = TextRange::new(start, end);
                self.push(Operator::UnaryOpSwap, r, "");
            }
        }
    }

    fn emit_compare(&mut self, c: &ast::ExprCompare) {
        for (i, op) in c.ops.iter().enumerate() {
            let lex = cmpop_lexeme(*op);
            let search_from = if i == 0 {
                c.left.range().end()
            } else {
                c.comparators[i - 1].range().end()
            };
            let search_to = c.comparators[i].range().start();
            let s: usize = search_from.into();
            let e: usize = search_to.into();
            if let Some(idx) = self.source[s..e].find(lex) {
                let abs = s + idx;
                let r = TextRange::new((abs as u32).into(), ((abs + lex.len()) as u32).into());
                for (orig, repl) in COMPARE_SWAPS {
                    if *orig == lex {
                        self.push(Operator::CompareOpSwap, r, repl);
                    }
                }
                for (orig, repl) in BOUNDARY_SWAPS {
                    if *orig == lex {
                        self.push(Operator::BoundaryShift, r, repl);
                    }
                }
            }
        }
    }

    /// Emit the four integer mutants (±1 shift, to-zero, negate) for a parsed
    /// value `v` at range `r`, formatting each in the literal's original `radix`.
    /// Shared by the `i64` and wider-`i128` parse paths in [`emit_number`]. `v`
    /// is always ≥ 0 here (the lexeme carries no sign), so `-v` can't overflow.
    fn push_int_mutants(&mut self, r: TextRange, v: i128, radix: u32) {
        self.push(
            Operator::NumberShift,
            r,
            &format_int(v.wrapping_add(1), radix),
        );
        self.push(
            Operator::NumberShift,
            r,
            &format_int(v.wrapping_sub(1), radix),
        );
        if v != 0 {
            self.push(Operator::NumberToZero, r, "0");
            self.push(Operator::NumberToNeg, r, &format_int(-v, radix));
        }
    }

    fn emit_number(&mut self, n: &ast::ExprNumberLiteral) {
        let r = n.range();
        let lex = &self.source[r];
        // Imaginary literals (`1j`, `2.5J`) are complex; shifting to a real
        // literal would change the type. Leave them alone.
        if lex.ends_with('j') || lex.ends_with('J') {
            return;
        }
        // Python allows `_` digit separators (`1_000`) that Rust's parsers
        // reject; strip them before parsing.
        let cleaned = lex.replace('_', "");
        if let Some((digits, radix)) = split_radix(&cleaned) {
            // Integer literal (decimal or `0x`/`0o`/`0b`-prefixed). Preserve the
            // original base in the replacement so the mutant reads naturally.
            // Try `i64` first, then fall through to `i128` for literals above
            // `i64::MAX` (e.g. `99999999999999999999`) — Python ints are
            // unbounded, so without the wider fallback those overflow
            // `i64::from_str_radix`, push nothing, and never reach the float
            // branch below, leaving the literal silently un-mutated. (Beyond
            // `i128` — ~39+ digit literals — still needs bignum and is left
            // alone.)
            if let Ok(v) = i64::from_str_radix(digits, radix) {
                self.push_int_mutants(r, i128::from(v), radix);
            } else if let Ok(v) = i128::from_str_radix(digits, radix) {
                self.push_int_mutants(r, v, radix);
            }
        } else if let Ok(v) = cleaned.parse::<f64>() {
            // Float literal (incl. exponent form like `1e3`). `format_float`
            // keeps the result a float so `5.0` shifts to `6.0`, not `6`.
            if v.is_finite() {
                // Past f64 integer precision (|v| ≳ 2^53, e.g. `1e16`) adding or
                // subtracting 1.0 is a no-op: `v + 1.0 == v`. Emitting such a
                // shift produces a value-identical mutant no test can ever kill —
                // a guaranteed false Survived that deflates the mutation score
                // (default runs have neither `--tce` nor `--equiv-detect` to
                // catch it). Only push a shift that actually changes the value.
                if v + 1.0 != v {
                    self.push(Operator::NumberShift, r, &format_float(v + 1.0));
                }
                if v - 1.0 != v {
                    self.push(Operator::NumberShift, r, &format_float(v - 1.0));
                }
                if v != 0.0 {
                    self.push(Operator::NumberToZero, r, "0");
                    self.push(Operator::NumberToNeg, r, &format_float(-v));
                }
            }
        }
    }

    fn emit_string(&mut self, s: &ast::ExprStringLiteral) {
        let r = s.range();
        let lex = &self.source[r];
        for (orig, repl) in CONSTANT_SWAPS {
            if *orig == lex {
                self.push(Operator::ConstantReplace, r, repl);
            }
        }
        if !is_empty_string_literal(lex) {
            self.push(Operator::StringToEmpty, r, "\"\"");
            if let Some(s) = wrap_with_sentinel(lex) {
                self.push(Operator::StringSentinel, r, &s);
            }
            // Parity (opt-in): UPPER/lower-case the literal content,
            // mirroring mutmut's string-case mutation.
            if let Some(s) = swap_string_case(lex, true) {
                self.push(Operator::StringCaseSwap, r, &s);
            }
            if let Some(s) = swap_string_case(lex, false) {
                self.push(Operator::StringCaseSwap, r, &s);
            }
        }
    }

    fn emit_bytes(&mut self, b: &ast::ExprBytesLiteral) {
        let r = b.range();
        let lex = &self.source[r];
        if !is_empty_bytes_literal(lex) {
            if let Some(s) = wrap_with_sentinel(lex) {
                self.push(Operator::BytesSentinel, r, &s);
            }
        }
    }

    fn emit_boolean(&mut self, b: &ast::ExprBooleanLiteral) {
        let r = b.range();
        let lex = &self.source[r];
        for (orig, repl) in CONSTANT_SWAPS {
            if *orig == lex {
                self.push(Operator::ConstantReplace, r, repl);
            }
        }
    }

    fn emit_await(&mut self, a: &ast::ExprAwait) {
        // `await X` → `X`: drop the await. The expression now evaluates
        // to the coroutine/awaitable itself instead of its result —
        // observable wherever the awaited value is used (comparison,
        // attribute access, return). Range covers `await ` up to the
        // operand start.
        let r = TextRange::new(a.range().start(), a.value.range().start());
        self.push(Operator::AwaitDrop, r, "");
    }

    fn emit_call(&mut self, c: &ast::ExprCall) {
        // Parity: whole call result → None (does the return value
        // matter where it's used?).
        self.push(Operator::ExprToNone, c.range(), "None");
        for kw in &*c.arguments.keywords {
            // Skip `**kwargs` splats (kw.arg is None).
            if kw.arg.is_none() {
                continue;
            }
            let r = self.extend_to_consume_comma(kw.range(), c.arguments.range());
            self.push(Operator::KeywordArgDrop, r, "");
            // arg-to-none: does the callee actually use this keyword's
            // value? Skip values already `None` (no-op).
            if !matches!(kw.value, Expr::NoneLiteral(_)) {
                self.push(Operator::ArgToNone, kw.value.range(), "None");
            }
        }
        for arg in &*c.arguments.args {
            // Parity: drop the positional argument entirely.
            let dr = self.extend_to_consume_comma(arg.range(), c.arguments.range());
            self.push(Operator::PositionalDrop, dr, "");
            // Skip `*args` splats — `f(None)` would drop the unpack
            // semantics — and values already `None`.
            if matches!(arg, Expr::Starred(_) | Expr::NoneLiteral(_)) {
                continue;
            }
            self.push(Operator::ArgToNone, arg.range(), "None");
        }
    }

    /// Drop each element of a list/set/tuple literal in turn (PositionalDrop).
    /// Shared by `Expr::List`/`Set`/`Tuple`, which differ only in node type.
    fn emit_seq_drop(&mut self, elts: &[Expr], container: TextRange) {
        for elt in elts {
            let r = self.extend_to_consume_comma(elt.range(), container);
            self.push(Operator::PositionalDrop, r, "");
        }
    }

    fn emit_dict(&mut self, d: &ast::ExprDict) {
        for item in &d.items {
            // Skip `**spread` items where key is None.
            let key = match &item.key {
                Some(k) => k,
                None => continue,
            };
            let item_range = TextRange::new(key.range().start(), item.value.range().end());
            let r = self.extend_to_consume_comma(item_range, d.range());
            self.push(Operator::DictItemDrop, r, "");
        }
    }

    fn emit_slice(&mut self, s: &ast::ExprSlice) {
        if let Some(lower) = &s.lower {
            self.push(Operator::SliceBoundDrop, lower.range(), "");
        }
        if let Some(upper) = &s.upper {
            self.push(Operator::SliceBoundDrop, upper.range(), "");
        }
        if let Some(step) = &s.step {
            let r = step.range();
            let lex = &self.source[r];
            let repl = if lex == "1" { "2" } else { "1" };
            self.push(Operator::SliceStepMutate, r, repl);
        }
    }
}

impl<'ast, 'a> SourceOrderVisitor<'ast> for Collector<'a> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // Innermost enclosing statement wins, and is restored on the way out,
        // so a nested body's mutants do not inherit the outer statement's head.
        let outer_stmt_line = self.stmt_line;
        self.stmt_line = self.line_of(stmt.range());
        match stmt {
            Stmt::AugAssign(a) => self.handle_aug_assign(a),
            Stmt::Return(r) => self.handle_return(r),
            Stmt::Break(b) => self.push(Operator::BreakContinueSwap, b.range, "continue"),
            Stmt::Continue(c) => self.push(Operator::BreakContinueSwap, c.range, "break"),
            Stmt::If(s) => self.handle_not_insertion(s.test.range()),
            Stmt::While(s) => self.handle_not_insertion(s.test.range()),
            Stmt::Assert(s) => self.handle_not_insertion(s.test.range()),
            Stmt::FunctionDef(f) => {
                self.handle_decorators(&f.decorator_list);
                self.handle_param_defaults(&f.parameters);
                self.handle_param_annotations(&f.parameters, f.returns.as_deref());
            }
            Stmt::ClassDef(c) => {
                self.handle_decorators(&c.decorator_list);
            }
            Stmt::Try(t) => self.handle_try(t),
            Stmt::For(f) => {
                if f.is_async {
                    // `async for` → `for`: sync iteration over an async iterator
                    // raises at runtime — a reliable kill wherever it runs. Skip
                    // the loop-count mutants here: `[]` / `[next(iter(...))]` are
                    // not async-iterable, so on an `async for` they raise the
                    // same TypeError instead of producing an empty/one-shot loop
                    // — redundant with async-for-to-sync and invalid by
                    // construction.
                    if let Some(r) = self.async_kw_range(f.range()) {
                        self.push(Operator::AsyncForToSync, r, "");
                    }
                } else {
                    self.handle_for(f);
                }
            }
            Stmt::With(w) if w.is_async => {
                // `async with` → `with`: a sync context-manager protocol on an
                // async CM raises at runtime.
                if let Some(r) = self.async_kw_range(w.range()) {
                    self.push(Operator::AsyncWithToSync, r, "");
                }
            }
            Stmt::Match(m) => self.handle_match(m),
            Stmt::Raise(r) => self.handle_raise(r),
            // Variable annotation (`x: int = 0`): mutate the annotation only;
            // the value (if any) is handled by the generic expr pass.
            Stmt::AnnAssign(a) => self.annotate(&a.annotation),
            Stmt::Assign(a) if !matches!(a.value.as_ref(), Expr::NoneLiteral(_)) => {
                self.push(Operator::AssignValueToNone, a.value.range(), "None");
            }
            _ => {}
        }
        walk_stmt(self, stmt);
        self.stmt_line = outer_stmt_line;
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // Thin dispatcher: match the variant (with its guard) and delegate to
        // the matching `emit_*` helper. Bodies live on `Collector` above.
        match expr {
            Expr::BinOp(b) => self.emit_binop(b),
            Expr::BoolOp(b) => self.emit_boolop(b),
            Expr::UnaryOp(u) => self.emit_unaryop(u),
            Expr::Compare(c) => self.emit_compare(c),
            Expr::NumberLiteral(n) => self.emit_number(n),
            Expr::StringLiteral(s) if !self.docstrings.contains(&expr.range()) => {
                self.emit_string(s)
            }
            // f-strings (`Expr::FString`) are a distinct node from plain string
            // literals, so the catalogue used to skip them entirely. They carry
            // real logic (error messages, formatted output); collapse the whole
            // f-string to an empty string — does the formatted result matter?
            Expr::FString(_) if !self.docstrings.contains(&expr.range()) => {
                self.push(Operator::StringToEmpty, expr.range(), "\"\"");
            }
            Expr::BytesLiteral(b) => self.emit_bytes(b),
            Expr::BooleanLiteral(b) => self.emit_boolean(b),
            Expr::Await(a) => self.emit_await(a),
            Expr::Lambda(l) if !matches!(l.body.as_ref(), Expr::NoneLiteral(_)) => {
                self.push(Operator::LambdaBodyToNone, l.body.range(), "None");
            }
            Expr::NoneLiteral(_) => {
                // Does code distinguish `None` from a value? Replacing it with a
                // non-None sentinel flips `is None` / `or default` / optional
                // logic. `""` mirrors mutmut's None replacement.
                self.push(Operator::NoneToValue, expr.range(), "\"\"");
            }
            Expr::Call(c) => self.emit_call(c),
            // Parity: value-position attribute / subscript reads → None. Only
            // Load context (mutating a Store target would be a syntax error).
            Expr::Attribute(a) if matches!(a.ctx, ast::ExprContext::Load) => {
                self.push(Operator::ExprToNone, a.range(), "None");
            }
            Expr::Subscript(s) if matches!(s.ctx, ast::ExprContext::Load) => {
                self.push(Operator::ExprToNone, s.range(), "None");
            }
            // Parity: drop one element from a list/set/tuple literal (≥2 elts,
            // mirroring the dict-item-drop bound).
            Expr::List(l) if l.elts.len() >= 2 => self.emit_seq_drop(&l.elts, l.range()),
            Expr::Set(s) if s.elts.len() >= 2 => self.emit_seq_drop(&s.elts, s.range()),
            Expr::Tuple(t) if t.elts.len() >= 2 => self.emit_seq_drop(&t.elts, t.range()),
            Expr::Dict(d) if d.items.len() >= 2 => self.emit_dict(d),
            Expr::Slice(s) => self.emit_slice(s),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod operator_emission_tests {
    use super::collect;
    use crate::mutator::Operator;
    use std::path::Path;

    fn ops_for(source: &str) -> Vec<Operator> {
        collect(Path::new("test.py"), source)
            .unwrap()
            .into_iter()
            .map(|m| m.operator)
            .collect()
    }

    fn contains(source: &str, op: Operator) -> bool {
        ops_for(source).contains(&op)
    }

    #[test]
    fn arith_op_swap_fires() {
        assert!(contains("x = a + b\n", Operator::ArithOpSwap));
        assert!(contains("x = a * b\n", Operator::ArithOpSwap));
        assert!(contains("x = a & b\n", Operator::ArithOpSwap));
    }

    #[test]
    fn compare_and_boundary_fire() {
        let ops = ops_for("if a < b:\n    pass\n");
        assert!(ops.contains(&Operator::CompareOpSwap));
        assert!(ops.contains(&Operator::BoundaryShift));
    }

    #[test]
    fn bool_op_swap_fires() {
        assert!(contains("x = a and b\n", Operator::BoolOpSwap));
        assert!(contains("x = a or b\n", Operator::BoolOpSwap));
    }

    #[test]
    fn aug_assign_swap_fires() {
        assert!(contains("x += 1\n", Operator::AugAssignSwap));
        assert!(contains("x &= 1\n", Operator::AugAssignSwap));
    }

    #[test]
    fn unary_op_swap_fires() {
        assert!(contains("x = -a\n", Operator::UnaryOpSwap));
        assert!(contains("if not flag:\n    pass\n", Operator::UnaryOpSwap));
        assert!(contains("x = ~a\n", Operator::UnaryOpSwap));
    }

    #[test]
    fn number_shift_and_zero_and_neg_fire() {
        let ops = ops_for("x = 42\n");
        assert!(ops.contains(&Operator::NumberShift));
        assert!(ops.contains(&Operator::NumberToZero));
        assert!(ops.contains(&Operator::NumberToNeg));
    }

    #[test]
    fn zero_literal_does_not_emit_to_zero_or_neg() {
        let ops = ops_for("x = 0\n");
        assert!(ops.contains(&Operator::NumberShift));
        assert!(!ops.contains(&Operator::NumberToZero));
        assert!(!ops.contains(&Operator::NumberToNeg));
    }

    fn repls_for(source: &str, op: Operator) -> Vec<String> {
        collect(Path::new("test.py"), source)
            .unwrap()
            .into_iter()
            .filter(|m| m.operator == op)
            .map(|m| m.replacement)
            .collect()
    }

    #[test]
    fn float_shift_stays_a_float() {
        // Regression: `5.0 + 1.0` stringified to `6`, changing the literal from
        // float to int. Shifts must keep the decimal point.
        let repls = repls_for("x = 5.0\n", Operator::NumberShift);
        assert!(repls.contains(&"6.0".to_string()), "{repls:?}");
        assert!(repls.contains(&"4.0".to_string()), "{repls:?}");
        assert_eq!(
            repls_for("x = 5.0\n", Operator::NumberToNeg),
            vec!["-5.0".to_string()]
        );
    }

    #[test]
    fn non_integer_float_shift_is_unchanged() {
        let repls = repls_for("x = 1.5\n", Operator::NumberShift);
        assert!(repls.contains(&"2.5".to_string()), "{repls:?}");
        assert!(repls.contains(&"0.5".to_string()), "{repls:?}");
    }

    #[test]
    fn large_float_emits_no_value_identical_shift() {
        // Regression: `1e16` is past f64 integer precision, so `v + 1.0 == v`
        // and `v - 1.0 == v`. The old code pushed both shifts anyway → two
        // value-identical no-op mutants, each a guaranteed false Survived that
        // deflates the score. Both shifts must be suppressed; only the still-
        // meaningful to-zero / negate mutants remain.
        assert!(
            repls_for("x = 1e16\n", Operator::NumberShift).is_empty(),
            "a precision-saturated float must emit no shift mutants"
        );
        let ops = ops_for("x = 1e16\n");
        assert!(ops.contains(&Operator::NumberToZero), "{ops:?}");
        assert!(ops.contains(&Operator::NumberToNeg), "{ops:?}");
    }

    #[test]
    fn underscored_decimal_literal_mutates() {
        // Regression: `1_000` failed both i64 and f64 parse → zero mutants.
        let repls = repls_for("x = 1_000\n", Operator::NumberShift);
        assert!(repls.contains(&"1001".to_string()), "{repls:?}");
        assert!(repls.contains(&"999".to_string()), "{repls:?}");
    }

    #[test]
    fn int_literal_above_i64_max_mutates() {
        // Regression: `99999999999999999999` (> i64::MAX) overflowed
        // `i64::from_str_radix`, pushed nothing, and the float branch was
        // unreachable (it's the `else` of `split_radix`) → zero mutants. The
        // i128 fallthrough must produce int-typed shifts, keeping the base.
        let repls = repls_for("x = 99999999999999999999\n", Operator::NumberShift);
        assert!(
            repls.contains(&"100000000000000000000".to_string()),
            "{repls:?}"
        );
        assert!(
            repls.contains(&"99999999999999999998".to_string()),
            "{repls:?}"
        );
        assert_eq!(
            repls_for("x = 99999999999999999999\n", Operator::NumberToNeg),
            vec!["-99999999999999999999".to_string()]
        );
    }

    #[test]
    fn hex_literal_above_i64_max_mutates_preserving_base() {
        // The same overflow path for a based literal: an 0x value past i64::MAX
        // must still shift while keeping its `0x` prefix.
        let repls = repls_for("x = 0xFFFFFFFFFFFFFFFF\n", Operator::NumberShift);
        assert!(
            repls.contains(&"0x10000000000000000".to_string()),
            "{repls:?}"
        );
        assert!(
            repls.contains(&"0xFFFFFFFFFFFFFFFE".to_string()),
            "{repls:?}"
        );
    }

    #[test]
    fn hex_literal_mutates_preserving_base() {
        let repls = repls_for("x = 0xFF\n", Operator::NumberShift);
        assert!(repls.contains(&"0x100".to_string()), "{repls:?}");
        assert!(repls.contains(&"0xFE".to_string()), "{repls:?}");
    }

    #[test]
    fn binary_literal_mutates_preserving_base() {
        let repls = repls_for("x = 0b1\n", Operator::NumberShift);
        assert!(repls.contains(&"0b10".to_string()), "{repls:?}");
        assert!(repls.contains(&"0b0".to_string()), "{repls:?}");
    }

    #[test]
    fn octal_literal_mutates_preserving_base() {
        let repls = repls_for("x = 0o17\n", Operator::NumberShift);
        assert!(repls.contains(&"0o20".to_string()), "{repls:?}");
        assert!(repls.contains(&"0o16".to_string()), "{repls:?}");
    }

    #[test]
    fn exponent_literal_mutates_as_float() {
        let repls = repls_for("x = 1e3\n", Operator::NumberShift);
        assert!(repls.contains(&"1001.0".to_string()), "{repls:?}");
        assert!(repls.contains(&"999.0".to_string()), "{repls:?}");
    }

    #[test]
    fn imaginary_literal_is_skipped() {
        assert!(!ops_for("x = 1j\n").contains(&Operator::NumberShift));
    }

    #[test]
    fn line_of_maps_each_statement_to_its_own_line() {
        // Regression: `line_of` was an O(n) rescan of the source per call and
        // was rewritten to a binary search over precomputed line-start offsets.
        // Verify the line mapping stays exact across multiple lines, including
        // a blank line and an indented body (offsets past several newlines).
        let src = "a = 1\n\nb = 2\nif a:\n    c = 3\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let line_of = |lit: &str| {
            mutants
                .iter()
                .find(|m| m.original == lit && m.operator == Operator::NumberShift)
                .unwrap_or_else(|| panic!("no NumberShift for {lit}"))
                .line
        };
        assert_eq!(line_of("1"), 1);
        assert_eq!(line_of("2"), 3);
        assert_eq!(line_of("3"), 5);
    }

    #[test]
    fn constant_replace_fires() {
        assert!(contains("x = True\n", Operator::ConstantReplace));
        assert!(contains("x = False\n", Operator::ConstantReplace));
    }

    #[test]
    fn string_ops_fire() {
        let ops = ops_for("x = \"hello\"\n");
        assert!(ops.contains(&Operator::StringToEmpty));
        assert!(ops.contains(&Operator::StringSentinel));
    }

    #[test]
    fn empty_string_not_mutated_by_to_empty_or_sentinel() {
        let ops = ops_for("x = \"\"\n");
        assert!(!ops.contains(&Operator::StringToEmpty));
        assert!(!ops.contains(&Operator::StringSentinel));
    }

    #[test]
    fn bytes_sentinel_fires() {
        assert!(contains("x = b\"hi\"\n", Operator::BytesSentinel));
    }

    #[test]
    fn return_value_to_none_fires() {
        assert!(contains(
            "def f():\n    return 1\n",
            Operator::ReturnValueToNone
        ));
    }

    #[test]
    fn return_none_not_mutated() {
        let ops = ops_for("def f():\n    return None\n");
        assert!(!ops.contains(&Operator::ReturnValueToNone));
    }

    #[test]
    fn break_continue_swap_fires() {
        let src = "for x in items:\n    if x < 0:\n        break\n    continue\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::BreakContinueSwap));
    }

    #[test]
    fn not_insertion_fires() {
        assert!(contains("if x:\n    pass\n", Operator::NotInsertion));
        assert!(contains("while x:\n    pass\n", Operator::NotInsertion));
        assert!(contains("assert x\n", Operator::NotInsertion));
    }

    #[test]
    fn remove_decorator_fires() {
        let src = "@trace\ndef f():\n    pass\n";
        assert!(contains(src, Operator::RemoveDecorator));
    }

    #[test]
    fn default_arg_to_none_fires() {
        assert!(contains(
            "def f(x=5):\n    pass\n",
            Operator::DefaultArgToNone
        ));
    }

    #[test]
    fn default_already_none_not_mutated() {
        let ops = ops_for("def f(x=None):\n    pass\n");
        assert!(!ops.contains(&Operator::DefaultArgToNone));
    }

    #[test]
    fn lambda_body_to_none_fires() {
        assert!(contains(
            "f = lambda x: x * x\n",
            Operator::LambdaBodyToNone
        ));
    }

    #[test]
    fn slice_bound_drop_fires() {
        let ops = ops_for("x = a[1:n]\n");
        assert!(ops.contains(&Operator::SliceBoundDrop));
    }

    #[test]
    fn slice_step_mutate_fires() {
        let ops = ops_for("x = a[::2]\n");
        assert!(ops.contains(&Operator::SliceStepMutate));
    }

    #[test]
    fn assign_value_to_none_fires() {
        assert!(contains("x = 1 + 2\n", Operator::AssignValueToNone));
    }

    #[test]
    fn keyword_arg_drop_fires() {
        let ops = ops_for("f(x=1, y=2)\n");
        assert!(ops.contains(&Operator::KeywordArgDrop));
    }

    #[test]
    fn arg_to_none_fires_on_positional_and_keyword() {
        let ops = ops_for("f(a, b, key=value)\n");
        // One ArgToNone per non-None positional and per keyword value.
        let n = ops.iter().filter(|o| **o == Operator::ArgToNone).count();
        assert_eq!(n, 3, "expected a→None, b→None, value→None");
    }

    #[test]
    fn arg_to_none_skips_none_args_and_splats() {
        // Already-None arg and *args/**kwargs splats must not emit ArgToNone.
        let ops = ops_for("f(None, *rest, k=None, **kw)\n");
        assert!(!ops.contains(&Operator::ArgToNone));
    }

    #[test]
    fn none_to_value_fires() {
        assert!(contains("x = None\n", Operator::NoneToValue));
        assert!(contains(
            "def f(a=None):\n    pass\n",
            Operator::NoneToValue
        ));
        assert!(contains("if x is None:\n    pass\n", Operator::NoneToValue));
    }

    #[test]
    fn fstring_to_empty_fires() {
        assert!(contains("x = f\"{a} hello\"\n", Operator::StringToEmpty));
    }

    #[test]
    fn parity_expr_to_none_fires_on_call_attr_subscript() {
        assert!(contains("x = foo(a)\n", Operator::ExprToNone));
        assert!(contains("x = obj.attr\n", Operator::ExprToNone));
        assert!(contains("x = data[key]\n", Operator::ExprToNone));
    }

    #[test]
    fn parity_expr_to_none_skips_store_targets() {
        let ops = ops_for("obj.attr = 1\nd[k] = 2\n");
        assert!(!ops.contains(&Operator::ExprToNone));
    }

    #[test]
    fn parity_positional_drop_fires() {
        assert!(contains("f(a, b)\n", Operator::PositionalDrop));
        assert!(contains("x = [1, 2, 3]\n", Operator::PositionalDrop));
        assert!(contains("x = (1, 2)\n", Operator::PositionalDrop));
    }

    #[test]
    fn parity_string_case_swap_fires() {
        assert!(ops_for("x = \"Hello\"\n").contains(&Operator::StringCaseSwap));
    }

    #[test]
    fn parity_string_case_swap_skips_escapes() {
        assert!(!ops_for("x = \"a\\nb\"\n").contains(&Operator::StringCaseSwap));
    }

    #[test]
    fn dict_item_drop_fires_only_when_multi_item() {
        assert!(contains("d = {'a': 1, 'b': 2}\n", Operator::DictItemDrop));
        let ops = ops_for("d = {'a': 1}\n");
        assert!(!ops.contains(&Operator::DictItemDrop));
    }

    #[test]
    fn await_drop_fires() {
        let src = "async def f():\n    x = await g()\n    return x\n";
        assert!(contains(src, Operator::AwaitDrop));
    }

    #[test]
    fn await_drop_removes_only_the_await_keyword() {
        // The mutant must splice out `await ` and leave the operand, producing
        // valid Python: `x = g()`.
        let src = "async def f():\n    x = await g()\n    return x\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let m = mutants
            .iter()
            .find(|m| m.operator == Operator::AwaitDrop)
            .expect("await-drop mutant");
        assert!(
            m.original.starts_with("await"),
            "original should span the await keyword: {:?}",
            m.original
        );
        assert_eq!(m.replacement, "");
    }

    #[test]
    fn async_for_to_sync_fires() {
        let src = "async def f(it):\n    async for x in it:\n        pass\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let m = mutants
            .iter()
            .find(|m| m.operator == Operator::AsyncForToSync)
            .expect("async-for-to-sync mutant");
        assert!(m.original.starts_with("async"));
        assert_eq!(m.replacement, "");
    }

    #[test]
    fn sync_for_does_not_emit_async_strip() {
        let ops = ops_for("for x in items:\n    pass\n");
        assert!(!ops.contains(&Operator::AsyncForToSync));
    }

    #[test]
    fn async_with_to_sync_fires() {
        let src = "async def f(cm):\n    async with cm as c:\n        pass\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let m = mutants
            .iter()
            .find(|m| m.operator == Operator::AsyncWithToSync)
            .expect("async-with-to-sync mutant");
        assert!(m.original.starts_with("async"));
        assert_eq!(m.replacement, "");
    }

    #[test]
    fn sync_with_does_not_emit_async_strip() {
        let ops = ops_for("with open('f') as fh:\n    pass\n");
        assert!(!ops.contains(&Operator::AsyncWithToSync));
    }

    #[test]
    fn match_guard_negate_fires_only_on_guarded_cases() {
        let src = "def f(x):\n    match x:\n        case n if n > 0:\n            return 1\n        case _:\n            return 0\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let guard: Vec<_> = mutants
            .iter()
            .filter(|m| m.operator == Operator::MatchGuardNegate)
            .collect();
        // Exactly one case has a guard (`if n > 0`); the wildcard has none.
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].original, "n > 0");
        assert_eq!(guard[0].replacement, "not (n > 0)");
    }

    #[test]
    fn raise_from_drop_fires_and_is_experimental() {
        let src = "def f():\n    try:\n        pass\n    except ValueError as e:\n        raise RuntimeError('x') from e\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let m = mutants
            .iter()
            .find(|m| m.operator == Operator::RaiseFromDrop)
            .expect("raise-from-drop mutant");
        // Range covers ` from e`, replacement empty → `raise RuntimeError('x')`.
        assert!(m.original.contains("from e"), "original: {:?}", m.original);
        assert_eq!(m.replacement, "");
        assert!(Operator::RaiseFromDrop.is_experimental());
    }

    #[test]
    fn raise_from_none_is_also_dropped() {
        let src = "def f():\n    raise RuntimeError('x') from None\n";
        assert!(contains(src, Operator::RaiseFromDrop));
    }

    #[test]
    fn plain_raise_does_not_emit_from_drop() {
        let ops = ops_for("def f():\n    raise RuntimeError('x')\n");
        assert!(!ops.contains(&Operator::RaiseFromDrop));
    }

    #[test]
    fn numeric_type_swap_fires_on_param_and_return_annotations() {
        let src = "def f(a: int, b: float) -> int:\n    return a\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let swaps: Vec<_> = mutants
            .iter()
            .filter(|m| m.operator == Operator::NumericTypeSwap)
            .collect();
        // a: int→float, b: float→int, return int→float.
        assert_eq!(swaps.len(), 3);
        assert!(swaps
            .iter()
            .any(|m| m.original == "int" && m.replacement == "float"));
        assert!(swaps
            .iter()
            .any(|m| m.original == "float" && m.replacement == "int"));
    }

    #[test]
    fn numeric_type_swap_ignores_runtime_int_call() {
        // A runtime `int(x)` must NOT be mutated — only annotations.
        let ops = ops_for("def f(x):\n    return int(x)\n");
        assert!(!ops.contains(&Operator::NumericTypeSwap));
    }

    #[test]
    fn optional_type_drop_fires_on_subscript_and_union() {
        let src = "def f(a: Optional[int], b: str | None) -> None:\n    return None\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let drops: Vec<_> = mutants
            .iter()
            .filter(|m| m.operator == Operator::OptionalTypeDrop)
            .collect();
        assert_eq!(drops.len(), 2, "Optional[int]→int and (str | None)→str");
        assert!(drops.iter().any(|m| m.replacement == "int"));
        assert!(drops.iter().any(|m| m.replacement == "str"));
    }

    #[test]
    fn optional_drop_recurses_into_inner_numeric() {
        // `Optional[int]` must ALSO yield the inner int→float swap, at the
        // int's own range — two independent mutants.
        let src = "def f(a: Optional[int]):\n    return a\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::OptionalTypeDrop));
        assert!(ops.contains(&Operator::NumericTypeSwap));
    }

    #[test]
    fn container_type_swap_fires_bare_and_subscripted() {
        let src = "def f(a: list, b: set[int]) -> tuple:\n    return b\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let swaps: Vec<_> = mutants
            .iter()
            .filter(|m| m.operator == Operator::ContainerTypeSwap)
            .collect();
        // a: list→tuple, b: set→frozenset (subscript value), return tuple→list.
        assert_eq!(swaps.len(), 3);
        assert!(swaps
            .iter()
            .any(|m| m.original == "list" && m.replacement == "tuple"));
        assert!(swaps
            .iter()
            .any(|m| m.original == "set" && m.replacement == "frozenset"));
        assert!(swaps
            .iter()
            .any(|m| m.original == "tuple" && m.replacement == "list"));
    }

    #[test]
    fn container_type_swap_ignores_runtime_list_call() {
        let ops = ops_for("def f(x):\n    return list(x)\n");
        assert!(!ops.contains(&Operator::ContainerTypeSwap));
    }

    #[test]
    fn ann_assign_annotation_is_mutated() {
        // Module-level `x: int = 0` — annotation mutated, AND the value is still
        // reached by the generic pass (no regression from the annotation guard).
        let ops = ops_for("x: int = 0\n");
        assert!(ops.contains(&Operator::NumericTypeSwap));
        assert!(
            ops.contains(&Operator::NumberShift),
            "value 0 still mutates"
        );
    }

    #[test]
    fn type_annotation_ops_are_experimental() {
        for op in [
            Operator::NumericTypeSwap,
            Operator::OptionalTypeDrop,
            Operator::ContainerTypeSwap,
        ] {
            assert!(op.is_experimental(), "{op:?} must be experimental");
        }
    }

    #[test]
    fn nested_container_annotation_mutates_each_layer() {
        // `list[Optional[int]]`: container swap on `list`, optional drop on
        // Optional[int], numeric swap on the inner int — one mutant per layer.
        let src = "def f(a: list[Optional[int]]):\n    return a\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::ContainerTypeSwap));
        assert!(ops.contains(&Operator::OptionalTypeDrop));
        assert!(ops.contains(&Operator::NumericTypeSwap));
    }

    #[test]
    fn dict_annotation_slice_tuple_recurses_without_container_swap() {
        // `dict` is a two-arg generic, deliberately absent from the container
        // swap table — but its slice tuple `str, Optional[int]` must still
        // recurse so the inner Optional/int mutate.
        let src = "def f(a: dict[str, Optional[int]]):\n    return a\n";
        let ops = ops_for(src);
        assert!(
            !ops.contains(&Operator::ContainerTypeSwap),
            "dict not swapped"
        );
        assert!(ops.contains(&Operator::OptionalTypeDrop));
        assert!(ops.contains(&Operator::NumericTypeSwap));
    }

    #[test]
    fn annotation_union_does_not_emit_generic_mutants() {
        // The generic expr pass recurses into annotations, but `str | None`
        // must NOT collect an arith swap on `|` or a none-to-value on `None`
        // (both raise at def-time). Only the annotation operators apply.
        let src = "def f(a: str | None):\n    return a\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::OptionalTypeDrop));
        assert!(
            !ops.contains(&Operator::ArithOpSwap),
            "no `|`→`&` in annotation"
        );
        assert!(
            !ops.contains(&Operator::NoneToValue),
            "no none-to-value in annotation"
        );
    }

    #[test]
    fn runtime_binop_outside_annotation_still_mutates() {
        // The annotation guard must be scoped: a real `a | b` in the body still
        // gets the arith swap.
        let ops = ops_for("def f(a, b):\n    return a | b\n");
        assert!(ops.contains(&Operator::ArithOpSwap));
    }

    #[test]
    fn annotation_non_union_literals_still_mutate() {
        // The union guard is scoped to `|` nodes only — a `Literal[N]` value and
        // `Annotated[...]` metadata elsewhere in an annotation still get the
        // generic mutations they got before the guard existed.
        assert!(
            ops_for("from typing import Literal\ndef f(x: Literal[0]):\n    return x\n")
                .contains(&Operator::NumberShift)
        );
        assert!(
            ops_for("def f(a: Annotated[int, some(0)]):\n    return a\n")
                .contains(&Operator::NumberShift),
            "Annotated metadata literal still mutates"
        );
    }

    #[test]
    fn async_for_omits_loop_count_mutants() {
        // `async for` emits only the async-strip; the loop-count mutants (`[]` /
        // `[next(iter(...))]`) are not async-iterable and are suppressed.
        let src = "async def f(it):\n    async for x in it:\n        pass\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::AsyncForToSync));
        assert!(!ops.contains(&Operator::ZeroIterationForLoop));
        assert!(!ops.contains(&Operator::OneIterationForLoop));
    }

    #[test]
    fn async_for_body_and_iter_still_mutate() {
        // Skipping loop-count on async-for must not stop the generic walk: the
        // iter expression and loop body still produce their mutants.
        let src = "async def f(it):\n    async for x in foo(it):\n        return x\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::AsyncForToSync));
        assert!(
            ops.contains(&Operator::ArgToNone),
            "iter arg `it` still mutates"
        );
        assert!(
            ops.contains(&Operator::ReturnValueToNone),
            "loop body still mutates"
        );
    }

    #[test]
    fn literal_arm_of_union_still_mutates() {
        // The union guard is token-precise: in `Literal[0] | None` only the `|`
        // and `None` are suppressed — the `0` inside the `Literal` arm still
        // gets its NumberShift (parity with pre-annotation-guard behaviour).
        let src = "from typing import Literal\ndef f(x: Literal[0] | None):\n    return x\n";
        let ops = ops_for(src);
        assert!(
            ops.contains(&Operator::NumberShift),
            "Literal value still mutates"
        );
        assert!(ops.contains(&Operator::OptionalTypeDrop));
        assert!(
            !ops.contains(&Operator::ArithOpSwap),
            "union `|` still suppressed"
        );
        assert!(
            !ops.contains(&Operator::NoneToValue),
            "union `None` still suppressed"
        );
    }

    #[test]
    fn optional_type_drop_fires_on_qualified_typing_optional() {
        // `typing.Optional[int]` (attribute form) must drop like the bare name.
        let src = "import typing\ndef f(a: typing.Optional[int]):\n    return a\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let drop = mutants
            .iter()
            .find(|m| m.operator == Operator::OptionalTypeDrop)
            .expect("qualified Optional should drop");
        assert_eq!(drop.replacement, "int");
    }

    #[test]
    fn experimental_for_loop_ops_fire() {
        let ops = ops_for("for x in items:\n    pass\n");
        assert!(ops.contains(&Operator::ZeroIterationForLoop));
        assert!(ops.contains(&Operator::OneIterationForLoop));
    }

    #[test]
    fn experimental_exception_ops_fire() {
        let src = "try:\n    pass\nexcept ValueError:\n    pass\n";
        let ops = ops_for(src);
        assert!(ops.contains(&Operator::ExceptionClassSwap));
        assert!(ops.contains(&Operator::BareExcept));
    }

    #[test]
    fn except_with_name_does_not_emit_bare_except() {
        let src = "try:\n    pass\nexcept ValueError as e:\n    pass\n";
        let ops = ops_for(src);
        assert!(!ops.contains(&Operator::BareExcept));
        assert!(ops.contains(&Operator::ExceptionClassSwap));
    }

    #[test]
    fn function_docstring_not_mutated() {
        let src = "def f():\n    \"\"\"docstring\"\"\"\n    return 1\n";
        let ops = ops_for(src);
        // ReturnValueToNone fires (on 1), NumberShift fires.
        // ConstantReplace + StringToEmpty + StringSentinel must NOT fire on the docstring.
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let on_docstring: Vec<_> = mutants
            .iter()
            .filter(|m| m.original.contains("docstring"))
            .collect();
        assert!(
            on_docstring.is_empty(),
            "docstring should not be mutated: {:?}",
            on_docstring
        );
        // Sanity: other mutations on the function body still emit.
        assert!(ops.contains(&Operator::ReturnValueToNone));
    }

    #[test]
    fn stmt_line_is_the_head_of_a_multi_line_statement() {
        // Three constant elements, so CPython folds the literal onto line 1
        // and no test context ever lands on lines 2-4. Every mutant there must
        // carry stmt_line 1 or the coverage filter drops it as uncovered with
        // no test able to rescue it. Fewer than three elements would still be
        // traced per line, and would not exhibit the bug this pins.
        let src = "ITEMS = [\n    \"read\",\n    \"write\",\n    \"admin\",\n]\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();

        let on_elements: Vec<_> = mutants.iter().filter(|m| m.line > 1).collect();
        assert!(
            !on_elements.is_empty(),
            "expected string mutants on the element lines"
        );
        for m in on_elements {
            assert_eq!(m.stmt_line, 1, "{}", m.describe());
        }
    }

    #[test]
    fn stmt_line_equals_line_for_single_line_statements() {
        let src = "x = 1 + 2\ny = 3 + 4\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        assert!(!mutants.is_empty());
        for m in &mutants {
            assert_eq!(m.stmt_line, m.line, "{}", m.describe());
        }
    }

    #[test]
    fn stmt_line_uses_the_innermost_enclosing_statement() {
        // The `return` on line 3 must not inherit the `def` on line 1, and the
        // multi-line `return` tuple on lines 5-6 must map back to line 5.
        let src =
            "def f(a):\n    if a:\n        return 1 + 2\n    return (\n        3 + 4,\n    )\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();

        let inner = mutants
            .iter()
            .find(|m| m.line == 3 && m.original == "+")
            .expect("arith mutant on line 3");
        assert_eq!(inner.stmt_line, 3);

        let wrapped = mutants
            .iter()
            .find(|m| m.line == 5 && m.original == "+")
            .expect("arith mutant on line 5");
        assert_eq!(wrapped.stmt_line, 4, "head of the multi-line return");
    }

    #[test]
    fn ignore_all_marker_suppresses_every_op_on_line() {
        let src = "x = 1 + 2  # fermut: ignore\ny = 3 + 4\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        assert!(mutants.iter().all(|m| m.line != 1));
        assert!(mutants.iter().any(|m| m.line == 2));
    }

    #[test]
    fn ignore_specific_op_only_suppresses_that_op() {
        let src = "x = 1 + 2  # fermut: ignore[arith-op-swap]\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        assert!(mutants.iter().all(|m| m.operator != Operator::ArithOpSwap));
        assert!(mutants.iter().any(|m| m.operator == Operator::NumberShift));
    }

    #[test]
    fn module_docstring_not_mutated() {
        let src = "\"\"\"module docstring\"\"\"\nx = 1\n";
        let mutants = collect(Path::new("test.py"), src).unwrap();
        let on_docstring: Vec<_> = mutants
            .iter()
            .filter(|m| m.original.contains("module docstring"))
            .collect();
        assert!(on_docstring.is_empty());
    }
}

#[cfg(test)]
mod id_invariant_tests {
    //! Mutant ids are the cache key. Two invariants must hold or the result
    //! cache silently misattributes outcomes:
    //!
    //! 1. **Stability**: identical input → identical id set. Re-collecting
    //!    must never reshuffle ids, otherwise warm-cache lookups all miss
    //!    and we re-run the whole suite.
    //! 2. **Uniqueness within a single file**: no two distinct mutants
    //!    share an id. A collision means the second mutant's outcome
    //!    overwrites the first in the cache (or vice versa, depending on
    //!    insertion order), so one of the two will silently get the wrong
    //!    cached verdict on subsequent runs.

    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use super::collect;
    use crate::mutator::Mutant;

    /// A source that exercises every operator we ship today — boundary,
    /// arith, compare, bool, aug-assign, constant, unary, container,
    /// slice — so the uniqueness invariant is stressed across the full
    /// operator catalog rather than a single op family.
    const RICH_SOURCE: &str = r#"
def f(a, b, lo, hi):
    x = a + b
    y = a * b
    z = a - b
    if a < b and lo <= hi:
        return True
    if not (a > b or lo >= hi):
        return False
    a += 1
    b -= 2
    c = [1, 2, 3, 4]
    d = (1, 2, 3)
    e = {1, 2, 3}
    s = "hello"
    n = 0
    m = -5
    return c[1:3] + d[::2]
"#;

    fn ids(mutants: &[Mutant]) -> Vec<String> {
        mutants.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn id_set_is_stable_across_collect_calls() {
        let a = collect(Path::new("rich.py"), RICH_SOURCE).unwrap();
        let b = collect(Path::new("rich.py"), RICH_SOURCE).unwrap();
        assert_eq!(
            ids(&a),
            ids(&b),
            "mutant ids must be deterministic between collect() invocations \
             — a reshuffle invalidates every warm-cache entry on the next run"
        );
    }

    #[test]
    fn ids_are_unique_within_a_file() {
        let mutants = collect(Path::new("rich.py"), RICH_SOURCE).unwrap();
        assert!(!mutants.is_empty(), "test corpus must produce mutants");

        // Group by id; any group with >1 entry is a collision. Build the
        // diagnostic eagerly so a failure points at the offending mutants
        // rather than just a count.
        let mut by_id: HashMap<&str, Vec<&Mutant>> = HashMap::new();
        for m in &mutants {
            by_id.entry(m.id.as_str()).or_default().push(m);
        }
        let collisions: Vec<_> = by_id.iter().filter(|(_, v)| v.len() > 1).collect();
        assert!(
            collisions.is_empty(),
            "id collisions detected — cache would misattribute outcomes:\n{}",
            collisions
                .iter()
                .map(|(id, ms)| {
                    let lines: Vec<String> = ms
                        .iter()
                        .map(|m| {
                            format!(
                                "  op={:?} range={:?} `{}`->`{}`",
                                m.operator, m.range, m.original, m.replacement
                            )
                        })
                        .collect();
                    format!("id={id}\n{}", lines.join("\n"))
                })
                .collect::<Vec<_>>()
                .join("\n---\n")
        );
    }

    #[test]
    fn collision_resistant_across_operator_choice() {
        // Defensive: even if two operators happen to propose the same
        // (range, original, replacement) tuple at the same offset — e.g. a
        // hypothetical BoundaryShift turning `<=` into `<` colliding with
        // a future ComparatorSwap doing the same — their ids must differ.
        // We can't easily synthesize this from real source today, so we
        // assert the structural invariant the id format gives us: no two
        // mutants in `RICH_SOURCE` with different operators share an id.
        let mutants = collect(Path::new("rich.py"), RICH_SOURCE).unwrap();
        let mut seen: HashSet<(String, crate::mutator::Operator)> = HashSet::new();
        for m in &mutants {
            assert!(
                seen.insert((m.id.clone(), m.operator)),
                "two mutants with the same (id, operator) pair — duplicate emission"
            );
        }
        // And the harder check: an id should map to exactly one operator,
        // otherwise distinct operators end up sharing a cache slot.
        let mut op_for_id: HashMap<&str, crate::mutator::Operator> = HashMap::new();
        for m in &mutants {
            if let Some(prev) = op_for_id.insert(m.id.as_str(), m.operator) {
                assert_eq!(
                    prev, m.operator,
                    "id `{}` produced by two distinct operators — cache would \
                     conflate their outcomes",
                    m.id
                );
            }
        }
    }
}
