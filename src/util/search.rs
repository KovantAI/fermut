//! Generic text/source scanning helpers: whole-word matching, a bounded
//! recursive `.py` grep, and line-shape probes (indentation, leading
//! identifier). Extracted from `explain` — these are plain text utilities with
//! no dependency on the mutation domain, shared by `explain` and `suggest`.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Default cap on matches returned by [`grep_symbol`]. Sized so the common
/// callers stay bounded (the coverage signal truncates to ≤8 unique files; the
/// test-matches signal renders at most a few dozen) while still capping the
/// walk on a multi-thousand-file repo.
pub(crate) const DEFAULT_GREP_MATCH_LIMIT: usize = 64;

/// Recursive walk of `root`, returning `(path, line)` pairs where `symbol`
/// appears as a whole word inside a `*.py` file. Returns after collecting
/// `limit` matches so a giant test tree can't stall `explain` / `suggest`.
/// Callers that need stricter caps can pass a smaller limit; callers that
/// genuinely want every match should not be using this function.
pub(crate) fn grep_symbol(
    root: &Path,
    symbol: &str,
    limit: usize,
) -> Result<Vec<(PathBuf, usize)>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            if out.len() >= limit {
                break;
            }
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if name.starts_with('.') || name == "__pycache__" {
                        continue;
                    }
                }
                stack.push(path);
            } else if ft.is_file() && path.extension().and_then(|s| s.to_str()) == Some("py") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    if contains_word(line, symbol) {
                        out.push((path.clone(), i + 1));
                        break;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Whole-word substring match: `needle` occurs in `haystack` bounded by
/// non-word bytes (or string edges) on both sides. Empty needle never matches.
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(bytes[abs - 1]);
        let after = abs + needle_bytes.len();
        let after_ok = after == bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle_bytes.len();
        if start >= bytes.len() {
            break;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Number of leading whitespace bytes on `line` (its indentation depth).
pub(crate) fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Leading identifier of `s` — the run of `[A-Za-z0-9_]` at the start.
/// `None` when `s` doesn't begin with an identifier character.
pub(crate) fn extract_ident(s: &str) -> Option<String> {
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            end = i + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        None
    } else {
        Some(s[..end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_word_matches_whole_word_only() {
        assert!(contains_word("foo bar baz", "bar"));
        assert!(contains_word("bar", "bar"));
        assert!(!contains_word("foobar", "bar"));
        assert!(!contains_word("barbaz", "bar"));
        assert!(!contains_word("rebar", "bar"));
        assert!(!contains_word("anything", ""));
    }

    #[test]
    fn indent_of_counts_leading_whitespace() {
        assert_eq!(indent_of("code"), 0);
        assert_eq!(indent_of("    code"), 4);
        assert_eq!(indent_of(""), 0);
    }

    #[test]
    fn extract_ident_reads_leading_identifier() {
        assert_eq!(extract_ident("foo(bar)").as_deref(), Some("foo"));
        assert_eq!(extract_ident("_x = 1").as_deref(), Some("_x"));
        assert_eq!(extract_ident("(nope)"), None);
    }
}
