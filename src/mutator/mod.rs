//! Mutation generation.
//!
//! - `operators` — the set of supported mutation operators + lexeme swap tables.
//! - `visitor` — AST traversal that emits candidate `Mutant`s for each
//!   operator on a single Python source file.
//! - `loader` — walks a source tree, opens each `.py` file, runs the visitor.
//! - `region` — ROR / logical region model behind the `--operators` profiles.
//! - `encoding` — handles UTF-8 BOM stripping and PEP 263 encoding directives.

pub mod encoding;
pub mod ignore;
mod lexeme;
pub mod loader;
pub mod operators;
pub mod region;
mod text_range_serde;
pub mod visitor;

use std::path::PathBuf;

use ruff_text_size::TextRange;
use serde::{Deserialize, Serialize};

pub use loader::collect_from_tree;
pub use operators::Operator;

/// A single source-level mutation: replace `range` in `file` with `replacement`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mutant {
    pub id: String,
    pub file: PathBuf,
    pub operator: Operator,
    #[serde(with = "text_range_serde")]
    pub range: TextRange,
    pub original: String,
    pub replacement: String,
    /// Line the mutated range starts on. This is what reports, annotations and
    /// ids show — the place a reader looks to find the mutation.
    pub line: u32,
    /// First line of the statement enclosing the mutated range. Equal to
    /// `line` for a single-line statement; earlier when the range sits on a
    /// continuation line — an element of a multi-line collection literal, an
    /// argument of a wrapped call.
    ///
    /// A continuation line can be absent from per-line coverage data even when
    /// the statement ran — CPython folds a collection literal of three or more
    /// constant elements onto the literal's first line, so the element lines
    /// emit no line event. Coverage lookups therefore fall back to this line;
    /// see [`crate::filter::coverage::CoverageContexts::tests_for_mutant`] for
    /// which continuation lines are affected and which are not.
    ///
    /// Defaults to `0` when absent from deserialized input (a report written
    /// by an older version), which disables the fallback.
    #[serde(default)]
    pub stmt_line: u32,
    /// Region-model tag for mutants of a predicate site the static
    /// subsumption model covers (single-op ordering compare, 2-operand
    /// `and`/`or` in truth position). `None` everywhere else. See
    /// [`region`] and the `--operators` profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<SiteTag>,
}

/// Where a mutant sits in the region model of its predicate site.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteTag {
    /// Start offset of the `Compare` / `BoolOp` node. Groups the mutants of
    /// one predicate: op-swap mutants' `range` is the operator token, while
    /// constant/operand-drop mutants replace the whole node, so `range` can't.
    pub start: u32,
    /// Regions where this mutant's truth differs from the original's.
    pub mask: region::DiffMask,
    /// In the site's minimal (dominator) set: no sibling candidate subsumes it.
    pub minimal: bool,
}

impl Mutant {
    pub fn describe(&self) -> String {
        // Include the byte offset (`@N`, same value the id carries) so two
        // mutants of the same operator on the same line — e.g. both `<=` in
        // `lo <= x and x <= hi` — render as distinct rows instead of looking
        // like a duplicate. It also lets a reader map a list row back to its
        // full `<file>@<offset>:...` id.
        format!(
            "{}:{}@{} [{}] `{}` → `{}`",
            self.file.display(),
            self.line,
            u32::from(self.range.start()),
            self.operator.name(),
            self.original,
            self.replacement
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutant_at(start: u32, end: u32) -> Mutant {
        Mutant {
            id: format!("f.py@{start}:boundary-shift:<=-><"),
            file: PathBuf::from("f.py"),
            operator: Operator::BoundaryShift,
            range: TextRange::new(start.into(), end.into()),
            original: "<=".into(),
            replacement: "<".into(),
            line: 14,
            stmt_line: 14,
            site: None,
        }
    }

    #[test]
    fn describe_disambiguates_co_located_same_operator_mutants() {
        // `lo <= x and x <= hi` — two `<=` boundary-shifts on line 14. Without
        // the byte offset the rows are byte-identical and look like a dup.
        let a = mutant_at(216, 218).describe();
        let b = mutant_at(227, 229).describe();
        assert_ne!(a, b, "co-located same-op mutants must render distinctly");
        assert!(a.contains(":14@216"), "got: {a}");
        assert!(b.contains(":14@227"), "got: {b}");
    }
}
