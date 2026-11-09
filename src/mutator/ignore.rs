//! Parse `# fermut: ignore` / `# fermut: ignore[op,...]` markers in source.
//!
//! Builds a line → ignore-spec map consulted by the visitor before it emits
//! a mutant. Ignore-all suppresses every operator on that line; the bracketed
//! form suppresses only the listed operator names.
//!
//! Region forms ignore every operator on a span of lines:
//! `# fermut: ignore-start` … `# fermut: ignore-end` (inclusive), and
//! `# fermut: ignore-block` — trailing a compound-statement header it covers
//! the header and its body; on a line of its own it covers the rest of the
//! enclosing block. These mirror mutmut's `# pragma: no mutate start/end/block`.
//!
//! Comments are extracted from the ruff token stream so string contents that
//! happen to contain `# fermut: ignore` are not misinterpreted as markers.

use std::collections::{HashMap, HashSet};

use ruff_python_ast::token::{TokenKind, Tokens};
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

    fn add_range(&mut self, from: u32, to: u32) {
        for line in from..=to {
            self.add(line, Ignore::All);
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
    let starts: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let pos = |byte: usize| {
        let line = starts.partition_point(|&s| s <= byte);
        (line as u32, byte - starts[line - 1])
    };
    // Region state. A block closes at the first later logical line that
    // starts left of its `limit` column; a start/end pair is explicit.
    let mut blocks: Vec<(u32, usize)> = Vec::new();
    let mut selection: Option<u32> = None;
    let mut at_line_start = true;
    let (mut logical_line, mut logical_col) = (1u32, 0usize);
    let mut last_line = 0u32;
    for token in tokens {
        let kind = token.kind();
        let start: usize = token.start().into();
        if kind.is_comment() {
            let end: usize = token.end().into();
            let body = source[start..end]
                .strip_prefix('#')
                .unwrap_or(&source[start..end]);
            let (line, col) = pos(start);
            match parse_marker(body) {
                Some(Marker::Line(entry)) => map.add(line, entry),
                // Trailing on a header: the header's logical line plus every
                // line indented deeper. Standalone: from the comment to the end
                // of the enclosing block.
                Some(Marker::Block) if at_line_start => blocks.push((line, col)),
                Some(Marker::Block) => blocks.push((logical_line, logical_col + 1)),
                Some(Marker::Start) => {
                    selection.get_or_insert(line);
                }
                Some(Marker::End) => {
                    if let Some(from) = selection.take() {
                        map.add_range(from, line);
                    }
                }
                None => {}
            }
            continue;
        }
        match kind {
            TokenKind::Newline => {
                at_line_start = true;
                continue;
            }
            TokenKind::NonLogicalNewline
            | TokenKind::Indent
            | TokenKind::Dedent
            | TokenKind::EndOfFile => continue,
            _ => {}
        }
        if at_line_start {
            at_line_start = false;
            (logical_line, logical_col) = pos(start);
            blocks.retain(|&(from, limit)| {
                let open = logical_col >= limit;
                if !open {
                    map.add_range(from, last_line);
                }
                open
            });
        }
        last_line = pos(token.end().into()).0;
    }
    // Unclosed regions run to the end of the file.
    for (from, _) in blocks {
        map.add_range(from, last_line);
    }
    if let Some(from) = selection {
        map.add_range(from, last_line);
    }
    map
}

enum Marker {
    Line(Ignore),
    Block,
    Start,
    End,
}

fn parse_marker(body: &str) -> Option<Marker> {
    let s = body.trim_start();
    let s = s.strip_prefix("fermut:")?;
    let s = s.trim_start();
    let s = s.strip_prefix("ignore")?;
    if let Some(rest) = s.strip_prefix('-') {
        let word_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        return match &rest[..word_end] {
            "block" => Some(Marker::Block),
            "start" => Some(Marker::Start),
            "end" => Some(Marker::End),
            _ => None,
        };
    }
    match s.chars().next() {
        None => Some(Marker::Line(Ignore::All)),
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
                Some(Marker::Line(Ignore::Ops(ops)))
            }
        }
        Some(c) if c.is_alphanumeric() || c == '_' => None,
        Some(_) => Some(Marker::Line(Ignore::All)),
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
    fn block_open_at_eof_runs_to_the_last_line() {
        let m = build("if x:  # fermut: ignore-block\n    y = 1 + 2\n    z = 3\n");
        assert!(m.is_ignored(2, Operator::ArithOpSwap));
        assert!(m.is_ignored(3, Operator::NumberShift));
    }

    #[test]
    fn line_ignore_inside_a_range_stays_ignore_all() {
        let m = build(
            "# fermut: ignore-start\nx = 1 + 2  # fermut: ignore\n# fermut: ignore-end\ny = 1\n",
        );
        assert!(m.is_ignored(2, Operator::ArithOpSwap));
        assert!(!m.is_ignored(4, Operator::NumberShift));
    }

    #[test]
    fn blank_names_in_op_list_are_skipped() {
        let m = build("x = 1 + 2  # fermut: ignore[arith-op-swap, ]\n");
        assert!(m.is_ignored(1, Operator::ArithOpSwap));
        assert!(!m.is_ignored(1, Operator::NumberShift));
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

    fn ignored_lines(m: &IgnoreMap, upto: u32) -> Vec<u32> {
        (1..=upto)
            .filter(|&l| m.is_ignored(l, Operator::ArithOpSwap))
            .collect()
    }

    #[test]
    fn start_end_region_is_inclusive() {
        let src = "a = 1\n# fermut: ignore-start\nb = 2\nc = 3\n# fermut: ignore-end\nd = 4\n";
        assert_eq!(ignored_lines(&build(src), 6), vec![2, 3, 4, 5]);
    }

    #[test]
    fn unclosed_start_runs_to_eof_and_stray_end_is_noop() {
        let src = "# fermut: ignore-end\na = 1\n# fermut: ignore-start\nb = 2\n";
        assert_eq!(ignored_lines(&build(src), 4), vec![3, 4]);
    }

    #[test]
    fn block_on_header_covers_body_not_else_or_siblings() {
        let src = "\
def f(x):
    if x:  # fermut: ignore-block
        y = x + 1
        return y
    else:
        return x - 1
z = 2
";
        assert_eq!(ignored_lines(&build(src), 7), vec![2, 3, 4]);
    }

    #[test]
    fn block_on_simple_statement_covers_only_that_statement() {
        let src = "a = (1 +\n     2)  # fermut: ignore-block\nb = 3\n";
        assert_eq!(ignored_lines(&build(src), 3), vec![1, 2]);
    }

    #[test]
    fn standalone_block_covers_rest_of_enclosing_block() {
        let src = "\
def f(x):
    a = x + 1
    # fermut: ignore-block
    b = x + 2
    if b:
        b -= 1
    return b
c = 3
";
        assert_eq!(ignored_lines(&build(src), 8), vec![3, 4, 5, 6, 7]);
    }

    #[test]
    fn standalone_block_as_first_body_line() {
        let src = "def f(x):\n    # fermut: ignore-block\n    return x + 1\ny = 2\n";
        assert_eq!(ignored_lines(&build(src), 4), vec![2, 3]);
    }

    #[test]
    fn unknown_hyphen_form_is_rejected() {
        let m = build("x = 1 + 2  # fermut: ignore-blocks\n");
        assert!(!m.is_ignored(1, Operator::ArithOpSwap));
    }
}
