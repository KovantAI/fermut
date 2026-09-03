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
        out: Vec::new(),
        docstrings,
        ignores,
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

struct Collector<'a> {
    file: PathBuf,
    source: &'a str,
    out: Vec<Mutant>,
    docstrings: HashSet<TextRange>,
    ignores: IgnoreMap,
    /// First line of the statement currently being visited. Mutants record it
    /// so coverage lookups can fall back to the line coverage.py actually
    /// attributes execution to — see `Mutant::stmt_line`.
    stmt_line: u32,
}

impl<'a> Collector<'a> {
    fn line_of(&self, range: TextRange) -> u32 {
        let start: usize = range.start().into();
        let line = self.source[..start].bytes().filter(|&b| b == b'\n').count() + 1;
        line as u32
    }

    fn push(&mut self, op: Operator, range: TextRange, replacement: &str) {
        let line = self.line_of(range);
        if self.ignores.is_ignored(line, op) {
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
    /// function (each parameter + the return). Annotation exprs are only
    /// visited here, never via the generic expr pass, so `int`/`list` inside an
    /// annotation is mutated while a runtime `int(x)` / `list(x)` call is not.
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
                if let Expr::Name(_) = s.value.as_ref() {
                    let nr = s.value.range();
                    let name = &self.source[nr];
                    if name == "Optional" {
                        // `Optional[T]` → `T`: replace the whole subscript with
                        // the inner type's source text.
                        let inner = &self.source[s.slice.range()];
                        self.push(Operator::OptionalTypeDrop, s.range(), inner);
                    }
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
                if matches!(b.right.as_ref(), Expr::NoneLiteral(_)) {
                    let keep = &self.source[b.left.range()];
                    self.push(Operator::OptionalTypeDrop, b.range(), keep);
                } else if matches!(b.left.as_ref(), Expr::NoneLiteral(_)) {
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
                // `async for` → `for`: sync iteration over an async iterator
                // raises at runtime — a reliable kill wherever the loop runs.
                if f.is_async {
                    if let Some(r) = self.async_kw_range(f.range()) {
                        self.push(Operator::AsyncForToSync, r, "");
                    }
                }
                self.handle_for(f);
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
        match expr {
            Expr::BinOp(b) => {
                let lex = binop_lexeme(b.op);
                if let Some(r) = self.op_range(b.range(), lex) {
                    for (orig, repl) in ARITH_SWAPS {
                        if *orig == lex {
                            self.push(Operator::ArithOpSwap, r, repl);
                        }
                    }
                }
            }
            Expr::BoolOp(b) => {
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
            Expr::UnaryOp(u) => {
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
            Expr::Compare(c) => {
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
                        let r =
                            TextRange::new((abs as u32).into(), ((abs + lex.len()) as u32).into());
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
            Expr::NumberLiteral(_) => {
                let r = expr.range();
                let lex = &self.source[r];
                if let Ok(v) = lex.parse::<i64>() {
                    self.push(Operator::NumberShift, r, &v.wrapping_add(1).to_string());
                    self.push(Operator::NumberShift, r, &v.wrapping_sub(1).to_string());
                    if v != 0 {
                        self.push(Operator::NumberToZero, r, "0");
                        self.push(Operator::NumberToNeg, r, &format!("-{}", v));
                    }
                } else if let Ok(v) = lex.parse::<f64>() {
                    if v.is_finite() {
                        self.push(Operator::NumberShift, r, &(v + 1.0).to_string());
                        self.push(Operator::NumberShift, r, &(v - 1.0).to_string());
                        if v != 0.0 {
                            self.push(Operator::NumberToZero, r, "0");
                            self.push(Operator::NumberToNeg, r, &format!("-{}", v));
                        }
                    }
                }
            }
            Expr::StringLiteral(_) if !self.docstrings.contains(&expr.range()) => {
                let r = expr.range();
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
            // f-strings (`Expr::FString`) are a distinct node from plain string
            // literals, so the catalogue used to skip them entirely. They carry
            // real logic (error messages, formatted output); collapse the whole
            // f-string to an empty string — does the formatted result matter?
            Expr::FString(_) if !self.docstrings.contains(&expr.range()) => {
                let r = expr.range();
                self.push(Operator::StringToEmpty, r, "\"\"");
            }
            Expr::BytesLiteral(_) => {
                let r = expr.range();
                let lex = &self.source[r];
                if !is_empty_bytes_literal(lex) {
                    if let Some(s) = wrap_with_sentinel(lex) {
                        self.push(Operator::BytesSentinel, r, &s);
                    }
                }
            }
            Expr::BooleanLiteral(_) => {
                let r = expr.range();
                let lex = &self.source[r];
                for (orig, repl) in CONSTANT_SWAPS {
                    if *orig == lex {
                        self.push(Operator::ConstantReplace, r, repl);
                    }
                }
            }
            Expr::Await(a) => {
                // `await X` → `X`: drop the await. The expression now evaluates
                // to the coroutine/awaitable itself instead of its result —
                // observable wherever the awaited value is used (comparison,
                // attribute access, return). Range covers `await ` up to the
                // operand start.
                let r = TextRange::new(a.range().start(), a.value.range().start());
                self.push(Operator::AwaitDrop, r, "");
            }
            Expr::Lambda(l) if !matches!(l.body.as_ref(), Expr::NoneLiteral(_)) => {
                self.push(Operator::LambdaBodyToNone, l.body.range(), "None");
            }
            Expr::NoneLiteral(_) => {
                // Does code distinguish `None` from a value? Replacing it with a
                // non-None sentinel flips `is None` / `or default` / optional
                // logic. `""` mirrors mutmut's None replacement.
                self.push(Operator::NoneToValue, expr.range(), "\"\"");
            }
            Expr::Call(c) => {
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
            Expr::List(l) if l.elts.len() >= 2 => {
                for elt in &l.elts {
                    let r = self.extend_to_consume_comma(elt.range(), l.range());
                    self.push(Operator::PositionalDrop, r, "");
                }
            }
            Expr::Set(s) if s.elts.len() >= 2 => {
                for elt in &s.elts {
                    let r = self.extend_to_consume_comma(elt.range(), s.range());
                    self.push(Operator::PositionalDrop, r, "");
                }
            }
            Expr::Tuple(t) if t.elts.len() >= 2 => {
                for elt in &t.elts {
                    let r = self.extend_to_consume_comma(elt.range(), t.range());
                    self.push(Operator::PositionalDrop, r, "");
                }
            }
            Expr::Dict(d) if d.items.len() >= 2 => {
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
            Expr::Slice(s) => {
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
            _ => {}
        }
        walk_expr(self, expr);
    }
}

fn binop_lexeme(op: ast::Operator) -> &'static str {
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

fn augop_lexeme(op: ast::Operator) -> &'static str {
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

fn is_empty_string_literal(lex: &str) -> bool {
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

fn is_empty_bytes_literal(lex: &str) -> bool {
    matches!(
        lex,
        "b\"\"" | "b''" | "B\"\"" | "B''" | "rb\"\"" | "rb''" | "br\"\"" | "br''"
    )
}

/// Wrap the *content* of a quoted literal with `XX` sentinel markers,
/// preserving prefix letters (r/b/u/B/rb/br/...) and quote style (single,
/// double, triple). Returns `None` if `lex` doesn't look like a quoted
/// literal we recognize.
fn wrap_with_sentinel(lex: &str) -> Option<String> {
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
fn swap_string_case(lex: &str, upper: bool) -> Option<String> {
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

fn cmpop_lexeme(op: ast::CmpOp) -> &'static str {
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
        // Module-level `x: int = 0` — annotation mutated, not just the value.
        let ops = ops_for("x: int = 0\n");
        assert!(ops.contains(&Operator::NumericTypeSwap));
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
