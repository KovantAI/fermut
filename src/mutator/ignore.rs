//! Parse `# fermut: ignore` / `# fermut: ignore[op,...]` markers in source.
//!
//! Builds a line → ignore-spec map consulted by the visitor before it emits
//! a mutant. Ignore-all suppresses every operator on that line; the bracketed
//! form suppresses only the listed operator names.
//!
//! Comments are extracted from the ruff token stream so string contents that
//! happen to contain `# fermut: ignore` are not misinterpreted as markers.

use std::collections::{HashMap, HashSet};

use ruff_python_ast::token::Tokens;
use ruff_text_size::Ranged;

use super::operators::Operator;

#[derive(Debug, Clone)]
pub enum Ignore {
    All,
    Ops(HashSet<Operator>),
}

#[derive(Debug, Default)]
pub struct IgnoreMap {
    by_line: HashMap<u32, Ignore>,
}

impl IgnoreMap {
    pub fn is_ignored(&self, line: u32, op: Operator) -> bool {
        match self.by_line.get(&line) {
            Some(Ignore::All) => true,
            Some(Ignore::Ops(set)) => set.contains(&op),
            None => false,
        }
    }

    fn add(&mut self, line: u32, entry: Ignore) {
        match self.by_line.get_mut(&line) {
            None => {
                self.by_line.insert(line, entry);
            }
            Some(Ignore::All) => {}
            Some(slot) => match entry {
                Ignore::All => *slot = Ignore::All,
                Ignore::Ops(more) => {
                    if let Ignore::Ops(set) = slot {
                        set.extend(more);
                    }
                }
            },
        }
    }
}

pub fn collect_from_tokens(source: &str, tokens: &Tokens) -> IgnoreMap {
    let mut map = IgnoreMap::default();
    for token in tokens {
        if !token.kind().is_comment() {
            continue;
        }
        let range = token.range();
        let start: usize = range.start().into();
        let end: usize = range.end().into();
        let body = source[start..end]
            .strip_prefix('#')
            .unwrap_or(&source[start..end]);
        let Some(entry) = parse_marker(body) else {
            continue;
        };
        let line = line_of(source, start);
        map.add(line, entry);
    }
    map
}

fn line_of(source: &str, byte: usize) -> u32 {
    source[..byte].bytes().filter(|&b| b == b'\n').count() as u32 + 1
}

fn parse_marker(body: &str) -> Option<Ignore> {
    let s = body.trim_start();
    let s = s.strip_prefix("fermut:")?;
    let s = s.trim_start();
    let s = s.strip_prefix("ignore")?;
    match s.chars().next() {
        None => Some(Ignore::All),
        Some('[') => {
            let rest = &s[1..];
            let close = rest.find(']')?;
            let mut ops = HashSet::new();
            for name in rest[..close].split(',') {
                let n = name.trim();
                if n.is_empty() {
                    continue;
                }
                if let Some(op) = Operator::from_name(n) {
                    ops.insert(op);
                }
            }
            if ops.is_empty() {
                None
            } else {
                Some(Ignore::Ops(ops))
            }
        }
        Some(c) if c.is_alphanumeric() || c == '_' => None,
        Some(_) => Some(Ignore::All),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_parser::parse_module;

    fn build(source: &str) -> IgnoreMap {
        let parsed = parse_module(source).unwrap();
        collect_from_tokens(source, parsed.tokens())
    }

    #[test]
    fn ignore_all_marker_marks_line() {
        let m = build("x = 1 + 2  # fermut: ignore\n");
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
        assert!(m.is_ignored(1, Operator::NumberShift));
        assert!(!m.is_ignored(2, Operator::ArithOpSwap));
    }

    #[test]
    fn ignore_specific_op_only() {
        let m = build("x = 1 + 2  # fermut: ignore[arith-op-swap]\n");
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
        assert!(!m.is_ignored(1, Operator::NumberShift));
    }

    #[test]
    fn ignore_multiple_ops_in_list() {
        let m = build("x = 1 + 2  # fermut: ignore[arith-op-swap, number-shift]\n");
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
        assert!(m.is_ignored(1, Operator::NumberShift));
        assert!(!m.is_ignored(1, Operator::ConstantReplace));
    }

    #[test]
    fn ignore_accepts_exp_prefixed_name() {
        let m = build("for x in xs:  # fermut: ignore[exp:zero-iteration-for-loop]\n    pass\n");
        assert!(m.is_ignored(1, Operator::ZeroIterationForLoop));
        assert!(!m.is_ignored(1, Operator::OneIterationForLoop));
    }

    #[test]
    fn unknown_op_in_list_dropped_but_known_kept() {
        let m = build("x = 1 + 2  # fermut: ignore[arith-op-swap, bogus]\n");
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
    }

    #[test]
    fn all_unknown_ops_means_no_ignore() {
        let m = build("x = 1 + 2  # fermut: ignore[bogus]\n");
        assert!(!m.is_ignored(1, Operator::ArithOpSwap));
    }

    #[test]
    fn ignored_word_is_not_a_marker() {
        let m = build("x = 1 + 2  # fermut: ignored maybe\n");
        assert!(!m.is_ignored(1, Operator::ArithOpSwap));
    }

    #[test]
    fn marker_inside_string_not_parsed() {
        let m = build("x = \"# fermut: ignore\"\n");
        assert!(!m.is_ignored(1, Operator::StringToEmpty));
    }

    #[test]
    fn multiple_markers_on_same_line_merge() {
        // Token stream gives one comment per `#` run, so split as two
        // separate-line comments and verify merging across an explicit add.
        let mut m = IgnoreMap::default();
        m.add(
            1,
            Ignore::Ops([Operator::ArithOpSwap].into_iter().collect()),
        );
        m.add(1, Ignore::Ops([Operator::BoolOpSwap].into_iter().collect()));
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
        assert!(m.is_ignored(1, Operator::BoolOpSwap));
    }

    #[test]
    fn all_promotes_over_ops() {
        let mut m = IgnoreMap::default();
        m.add(
            1,
            Ignore::Ops([Operator::ArithOpSwap].into_iter().collect()),
        );
        m.add(1, Ignore::All);
        assert!(m.is_ignored(1, Operator::NumberShift));
    }

    #[test]
    fn marker_with_trailing_text_treated_as_all() {
        let m = build("x = 1 + 2  # fermut: ignore - flake8 disagrees\n");
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
    }
}
