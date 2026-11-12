//! AST-structural hash of a Python source file.
//!
//! Used as the cache key in place of `sha256(bytes)` so reformat / comment /
//! whitespace edits that leave the AST untouched do not bust the cache.
//!
//! Strategy: parse the source with ruff, format the parsed `Mod` with
//! `{:#?}`, strip every `TextRange` substring (`<int>..<int>`) from the dump
//! *outside* Debug-quoted string runs, and sha256 the result. Range
//! stripping is necessary because `TextRange` Debug embeds raw byte offsets
//! — any whitespace or comment edit upstream shifts every range below it
//! and would otherwise produce a brand-new hash. The in-string exemption
//! prevents user string literals like `"1..2"` from being collapsed with
//! the range-debug pattern (which would let differing strings collide).
//!
//! What still busts the hash:
//!   - any structural change (new/removed/reordered nodes)
//!   - identifier renames, literal-value edits, operator changes
//!   - docstring text edits (docstrings are `Expr::StringLiteral` nodes in
//!     the AST — observable as `__doc__`, so kept in the hash)
//!
//! What no longer busts the hash:
//!   - reformat / reflow / comment edits
//!   - trailing whitespace and final-newline differences
//!   - byte-equivalent edits that the parser collapses (e.g. quote-style
//!     differences are *not* collapsed; the literal kind is preserved)
//!
//! Parse failures fall back to a byte-level hash, prefixed `bytes:` so a
//! later well-formed version of the same file cannot collide with the
//! fallback entry.

use anyhow::{Context, Result};
use ruff_python_ast::{self as ast, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Hex-encoded sha256 over the AST of `source`, with `TextRange` offsets
/// stripped. Returns `None` if the source fails to parse so the caller can
/// fall back to byte hashing.
#[cfg(test)]
pub fn hash_ast_source(source: &str) -> Option<String> {
    let parsed = parse_module(source).ok()?;
    Some(hash_module(parsed.syntax()))
}

/// AST-structural hash of an already-parsed module. The single place the
/// canonical `{:#?}` + range-strip + `ast:v1` hashing lives, so
/// [`hash_ast_source`], [`analyze_file`], and [`compute_scope_map`] share one
/// parse instead of each re-parsing the same source.
fn hash_module(module: &ast::ModModule) -> String {
    let dbg = format!("{:#?}", module);
    let canon = strip_ranges(&dbg);
    let mut h = Sha256::new();
    h.update(b"ast:v1\n");
    h.update(canon.as_bytes());
    hex::encode(h.finalize())
}

/// Byte-hash fallback (prefixed `bytes:`) for a file that doesn't parse or
/// isn't utf-8, so every file still gets a stable, distinct cache key.
fn byte_fallback_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"bytes:v1\n");
    h.update(bytes);
    hex::encode(h.finalize())
}

/// AST-structural hash of the file at `path`. Falls back to a byte hash
/// (prefixed `bytes:`) on parse failure or non-utf8 contents so every file
/// still gets a stable, distinct cache key.
pub fn hash_file_ast(path: &Path) -> Result<String> {
    Ok(analyze_file(path, false, false)?.ast_hash)
}

/// Everything the engine needs to know about one unique source file, produced
/// in a **single** read and at most one parse. Replaces the three separate
/// per-file passes (hash / scope-map / source-for-equiv) that each re-read and
/// re-parsed the same files.
pub struct FileAnalysis {
    /// Cache key: AST-structural hash, or the byte fallback when the file
    /// doesn't parse / isn't utf-8. Always present.
    pub ast_hash: String,
    /// `Some` only when `want_scope` and the file parsed. `None` means "not
    /// requested" *or* "parse/utf-8 failed"; the caller distinguishes via its
    /// own `want_scope` flag to decide whether to warn.
    pub scope_map: Option<ScopeMap>,
    /// Top-level def/class positions, for position-stable cache keys. `Some`
    /// whenever the file parsed; cheap (names and ranges, no hashing).
    pub anchors: Option<Anchors>,
    /// Decoded source, `Some` only when `want_source` and the file is utf-8.
    pub source: Option<String>,
}

/// The top-level `def`s and `class`es of a file, by name and source range.
///
/// Lets the cache key a mutant by *where it sits in its function* instead of
/// by absolute byte offset: a line added in one function, or a comment above
/// it, moves every mutant below in the file, but not their position within
/// their own top-level def.
#[derive(Debug, Clone, Default)]
pub struct Anchors(Vec<Anchor>);

#[derive(Debug, Clone)]
struct Anchor {
    range: TextRange,
    /// `name` for the first top-level def/class of that name, `name#2` for
    /// the second (a conditional redefinition), and so on.
    label: String,
}

impl Anchors {
    fn from_module(module: &ast::ModModule) -> Self {
        let mut seen: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        let mut out = Vec::new();
        for stmt in &module.body {
            let name = match stmt {
                Stmt::FunctionDef(f) => f.name.as_str(),
                Stmt::ClassDef(c) => c.name.as_str(),
                _ => continue,
            };
            let n = seen.entry(name).or_insert(0);
            *n += 1;
            let label = if *n == 1 {
                name.to_string()
            } else {
                format!("{name}#{n}")
            };
            out.push(Anchor {
                range: stmt.range(),
                label,
            });
        }
        Self(out)
    }

    /// The label of the top-level def/class containing `range`, and `range`'s
    /// start offset from that def's start. `None` at module level.
    pub fn locate(&self, range: TextRange) -> Option<(&str, u32)> {
        self.0
            .iter()
            .find(|a| a.range.start() <= range.start() && range.end() <= a.range.end())
            .map(|a| (a.label.as_str(), u32::from(range.start() - a.range.start())))
    }
}

#[cfg(test)]
pub fn compute_anchors(source: &str) -> Option<Anchors> {
    Some(Anchors::from_module(parse_module(source).ok()?.syntax()))
}

/// Read `path` once, parse it at most once, and derive the requested
/// artifacts. `want_scope`/`want_source` gate the extra work so a run that
/// needs neither pays only for the hash. Errors only on read failure.
pub fn analyze_file(path: &Path, want_scope: bool, want_source: bool) -> Result<FileAnalysis> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let source = std::str::from_utf8(&bytes).ok();
    let parsed = source.and_then(|s| parse_module(s).ok());
    let ast_hash = match parsed.as_ref() {
        Some(p) => hash_module(p.syntax()),
        None => byte_fallback_hash(&bytes),
    };
    let scope_map = if want_scope {
        parsed
            .as_ref()
            .map(|p| compute_scope_map_parsed(p.syntax()))
    } else {
        None
    };
    let anchors = parsed.as_ref().map(|p| Anchors::from_module(p.syntax()));
    let source = if want_source {
        source.map(str::to_owned)
    } else {
        None
    };
    Ok(FileAnalysis {
        ast_hash,
        scope_map,
        anchors,
        source,
    })
}

/// Replace every `<int>..<int>` substring (ruff's `TextRange` Debug format)
/// with a single `#` sentinel — but only outside Debug-quoted string runs.
/// String-literal *values* are embedded verbatim by `{:#?}` (e.g.
/// `StringLiteral { value: "1..2" }`), and those characters are
/// program-observable: collapsing them would let `x = "1..2"` and
/// `x = "100..200"` collide in the cache. A simple in-string toggle on
/// unescaped `"` is enough because Debug escapes interior `"` and `\` with
/// a leading backslash. ASCII-only scan keeps the result valid UTF-8.
fn strip_ranges(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            out.push(b);
            i += 1;
            if b == b'\\' && i < bytes.len() {
                out.push(bytes[i]);
                i += 1;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        if b == b'"' {
            in_str = true;
            out.push(b);
            i += 1;
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i + 2 < bytes.len()
                && bytes[i] == b'.'
                && bytes[i + 1] == b'.'
                && bytes[i + 2].is_ascii_digit()
            {
                let mut j = i + 2;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                out.push(b'#');
                i = j;
                continue;
            }
            out.extend_from_slice(&bytes[start..i]);
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).expect("ASCII-only edits preserve UTF-8")
}

/// Per-file scope index used by the `cache_scope = "scope"` cache mode.
///
/// The file's top-level statements are split into two layers:
///
///   * **Prelude** — every top-level statement that is *not* the body of a
///     function or class. Function/class definitions are included **without
///     their bodies** (decorators, name, type params, parameter list and
///     return annotation only). Editing the prelude — imports, module-level
///     constants, a function's signature, a class's bases — invalidates
///     every scope in the file.
///
///   * **Scopes** — one entry per top-level `def` / `class`. The entry's
///     `body_hash` covers everything under that scope (including nested
///     defs and class methods). Editing the body of one top-level scope
///     leaves every other scope's `body_hash` untouched.
///
/// `scope_hash_for(range)` returns `sha256(prelude || qualname || body_hash)`
/// for the innermost top-level scope that contains `range`. Mutants at module
/// level (no enclosing top-level def/class) fall back to a module-wide hash,
/// matching the safety of the `file` cache mode.
///
/// **Soundness caveat**: the scope mode assumes a mutant's test outcomes
/// depend only on the enclosing top-level scope plus the file prelude. A test
/// that covers function `bar` but indirectly calls function `foo` can return
/// a stale cached verdict if only `foo`'s body changed. Default cache mode
/// is `file` precisely because of this; the user must opt into `scope`.
#[derive(Debug, Clone)]
pub struct ScopeMap {
    prelude_hash: String,
    /// Hash of the full module AST. Used as fallback for module-level mutants
    /// so they bust on any change (file-mode semantics).
    module_hash: String,
    /// Top-level def/class scopes in source order.
    scopes: Vec<ScopeEntry>,
}

#[derive(Debug, Clone)]
struct ScopeEntry {
    range: TextRange,
    qualname: String,
    body_hash: String,
}

impl ScopeMap {
    /// Cache-key hash for a mutant whose source range is `mutant_range`.
    /// Returns the prelude + scope-body composite hash when the mutant falls
    /// inside a top-level def/class; otherwise the module-wide hash.
    pub fn scope_hash_for(&self, mutant_range: TextRange) -> String {
        if let Some(entry) = self.innermost_containing(mutant_range) {
            let mut h = Sha256::new();
            h.update(b"scope:v1\n");
            h.update(self.prelude_hash.as_bytes());
            h.update(b"\n");
            h.update(entry.qualname.as_bytes());
            h.update(b"\n");
            h.update(entry.body_hash.as_bytes());
            hex::encode(h.finalize())
        } else {
            let mut h = Sha256::new();
            h.update(b"scope:v1\nmodule\n");
            h.update(self.module_hash.as_bytes());
            hex::encode(h.finalize())
        }
    }

    /// Top-level scopes have disjoint source ranges, so containment is a
    /// straight linear scan. Returns `None` when no scope contains the range
    /// (module-level mutant).
    fn innermost_containing(&self, mutant_range: TextRange) -> Option<&ScopeEntry> {
        self.scopes.iter().find(|s| {
            s.range.start() <= mutant_range.start() && mutant_range.end() <= s.range.end()
        })
    }

    #[cfg(test)]
    pub fn scope_qualnames(&self) -> Vec<&str> {
        self.scopes.iter().map(|s| s.qualname.as_str()).collect()
    }
}

/// Parse `source` and build a [`ScopeMap`]. Returns `None` on parse failure
/// so the caller can fall back to file-level hashing.
#[cfg(test)]
pub fn compute_scope_map(source: &str) -> Option<ScopeMap> {
    let parsed = parse_module(source).ok()?;
    Some(compute_scope_map_parsed(parsed.syntax()))
}

/// [`compute_scope_map`] on an already-parsed module — reuses the caller's
/// single parse (and hashes that same module, instead of re-parsing `source`
/// via `hash_ast_source` as before).
fn compute_scope_map_parsed(module: &ast::ModModule) -> ScopeMap {
    let module_hash = hash_module(module);

    let mut prelude = Sha256::new();
    prelude.update(b"prelude:v1\n");
    let mut scopes = Vec::new();

    for stmt in &module.body {
        match stmt {
            Stmt::FunctionDef(f) => {
                let sig = function_signature_fingerprint(f);
                prelude.update(b"fn|");
                prelude.update(f.name.as_str().as_bytes());
                prelude.update(b"|");
                prelude.update(sig.as_bytes());
                prelude.update(b"\n");
                scopes.push(ScopeEntry {
                    range: f.range(),
                    qualname: f.name.as_str().to_string(),
                    body_hash: body_fingerprint(&f.body),
                });
            }
            Stmt::ClassDef(c) => {
                let sig = class_signature_fingerprint(c);
                prelude.update(b"class|");
                prelude.update(c.name.as_str().as_bytes());
                prelude.update(b"|");
                prelude.update(sig.as_bytes());
                prelude.update(b"\n");
                scopes.push(ScopeEntry {
                    range: c.range(),
                    qualname: c.name.as_str().to_string(),
                    body_hash: body_fingerprint(&c.body),
                });
            }
            other => {
                prelude.update(b"stmt|");
                prelude.update(strip_ranges(&format!("{:#?}", other)).as_bytes());
                prelude.update(b"\n");
            }
        }
    }

    let prelude_hash = hex::encode(prelude.finalize());
    ScopeMap {
        prelude_hash,
        module_hash,
        scopes,
    }
}

fn function_signature_fingerprint(f: &ast::StmtFunctionDef) -> String {
    let mut h = Sha256::new();
    h.update(b"fn-sig:v1\n");
    h.update(if f.is_async {
        &b"async\n"[..]
    } else {
        &b"sync\n"[..]
    });
    h.update(f.name.as_str().as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", f.decorator_list)).as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", f.type_params)).as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", f.parameters)).as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", f.returns)).as_bytes());
    hex::encode(h.finalize())
}

fn class_signature_fingerprint(c: &ast::StmtClassDef) -> String {
    let mut h = Sha256::new();
    h.update(b"class-sig:v1\n");
    h.update(c.name.as_str().as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", c.decorator_list)).as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", c.type_params)).as_bytes());
    h.update(b"\n");
    h.update(strip_ranges(&format!("{:#?}", c.arguments)).as_bytes());
    hex::encode(h.finalize())
}

fn body_fingerprint(body: &[Stmt]) -> String {
    let mut h = Sha256::new();
    h.update(b"body:v1\n");
    for stmt in body {
        h.update(strip_ranges(&format!("{:#?}", stmt)).as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

/// One file's split for the `coverage` cache mode: what runs at import, and
/// each top-level def/class a test can execute.
#[derive(Debug, Clone)]
pub struct ScopeLayout {
    /// Every top-level statement that isn't a def/class, plus each def's and
    /// class's decorators: code that runs at import time, so no per-test
    /// coverage context can attribute it.
    pub globals: String,
    /// Top-level defs/classes in source order: `(first line, last line,
    /// hash of the whole statement)`. Lines are 1-based, as coverage records them.
    pub scopes: Vec<(u32, u32, String)>,
}

impl ScopeLayout {
    /// Index into `scopes` of the top-level def/class spanning `line`, or
    /// `None` at module level.
    pub fn scope_at(&self, line: u32) -> Option<usize> {
        let i = self.scopes.partition_point(|s| s.0 <= line);
        (i > 0 && line <= self.scopes[i - 1].1).then(|| i - 1)
    }
}

/// [`ScopeLayout`] of the file at `path`; `None` when it can't be read or
/// parsed (the caller hashes its bytes into the global layer instead).
pub fn scope_layout(path: &Path) -> Option<ScopeLayout> {
    let source = std::fs::read_to_string(path).ok()?;
    let parsed = parse_module(&source).ok()?;
    let newlines: Vec<usize> = source.match_indices('\n').map(|(i, _)| i).collect();
    let line_of = |offset: ruff_text_size::TextSize| {
        1 + newlines.partition_point(|&i| i < usize::from(offset)) as u32
    };
    let mut globals = Sha256::new();
    globals.update(b"globals:v1\n");
    let mut scopes = Vec::new();
    for stmt in &parsed.syntax().body {
        let decorators = match stmt {
            Stmt::FunctionDef(f) => &f.decorator_list,
            Stmt::ClassDef(c) => &c.decorator_list,
            other => {
                globals.update(strip_ranges(&format!("{:#?}", other)).as_bytes());
                globals.update(b"\n");
                continue;
            }
        };
        // Only decorated ones: adding a plain helper changes no import-time
        // behaviour, so it mustn't re-run everything.
        if !decorators.is_empty() {
            globals.update(strip_ranges(&format!("{:#?}", decorators)).as_bytes());
            globals.update(b"\n");
        }
        let range = stmt.range();
        let mut h = Sha256::new();
        h.update(b"scope:v1\n");
        h.update(strip_ranges(&format!("{:#?}", stmt)).as_bytes());
        scopes.push((
            line_of(range.start()),
            line_of(range.end()),
            hex::encode(h.finalize()),
        ));
    }
    Some(ScopeLayout {
        globals: hex::encode(globals.finalize()),
        scopes,
    })
}

/// Byte ranges of every function definition in `source` (all nesting depths,
/// source order, so an outer function precedes the ones nested in it), via a
/// source-order walk. Empty on a parse failure.
pub(crate) fn function_spans(source: &str) -> Vec<TextRange> {
    use ruff_python_ast::visitor::source_order::{walk_stmt, SourceOrderVisitor};
    let Ok(parsed) = parse_module(source) else {
        return Vec::new();
    };
    struct FnSpans {
        spans: Vec<TextRange>,
    }
    impl<'a> SourceOrderVisitor<'a> for FnSpans {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            if let Stmt::FunctionDef(f) = stmt {
                self.spans.push(f.range());
            }
            walk_stmt(self, stmt);
        }
    }
    let mut v = FnSpans { spans: Vec::new() };
    for stmt in &parsed.syntax().body {
        v.visit_stmt(stmt);
    }
    v.spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_file_returns_scope_map_and_source_when_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.py");
        let src = "def f():\n    return 1\n";
        std::fs::write(&path, src).unwrap();
        let a = analyze_file(&path, true, true).unwrap();
        assert_eq!(a.source.as_deref(), Some(src));
        assert_eq!(a.ast_hash, hash_file_ast(&path).unwrap());
        let map = a.scope_map.expect("parsed file yields a scope map");
        let in_f = TextRange::new(20u32.into(), 21u32.into());
        assert_ne!(map.scope_hash_for(in_f), a.ast_hash);
        // Unparseable source: no scope map, but the source is still returned.
        std::fs::write(&path, "def (:\n").unwrap();
        let bad = analyze_file(&path, true, true).unwrap();
        assert!(bad.scope_map.is_none());
        assert_eq!(bad.source.as_deref(), Some("def (:\n"));
    }

    fn h(src: &str) -> String {
        hash_ast_source(src).expect("parse")
    }

    #[test]
    fn whitespace_change_does_not_change_hash() {
        let a = "def f(x):\n    return x + 1\n";
        let b = "def f(x):\n\n    return x  +  1\n";
        assert_eq!(h(a), h(b));
    }

    #[test]
    fn trailing_newline_does_not_change_hash() {
        let a = "x = 1\n";
        let b = "x = 1";
        assert_eq!(h(a), h(b));
    }

    #[test]
    fn comment_change_does_not_change_hash() {
        let a = "def f():\n    return 1  # one\n";
        let b = "def f():\n    return 1  # something completely different\n";
        let c = "def f():\n    return 1\n";
        assert_eq!(h(a), h(b));
        assert_eq!(h(a), h(c));
    }

    #[test]
    fn literal_change_changes_hash() {
        let a = "x = 1\n";
        let b = "x = 2\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn operator_change_changes_hash() {
        let a = "x = a + b\n";
        let b = "x = a - b\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn identifier_rename_changes_hash() {
        let a = "def f(x):\n    return x\n";
        let b = "def f(y):\n    return y\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn added_statement_changes_hash() {
        let a = "x = 1\n";
        let b = "x = 1\ny = 2\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn docstring_edit_changes_hash() {
        let a = "def f():\n    \"\"\"old\"\"\"\n    return 1\n";
        let b = "def f():\n    \"\"\"new\"\"\"\n    return 1\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn malformed_source_returns_none() {
        assert!(hash_ast_source("def f(:\n").is_none());
    }

    #[test]
    fn hash_file_ast_falls_back_on_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.py");
        std::fs::write(&path, b"def f(:\n").unwrap();
        let h = hash_file_ast(&path).unwrap();
        assert!(
            h.len() == 64,
            "fallback must still produce a 64-char hex digest"
        );
    }

    #[test]
    fn hash_file_ast_byte_fallback_differs_from_ast_hash() {
        // A parseable file and a malformed file with identical byte content
        // can't both exist — but we can at least verify the fallback prefix
        // segregates fallback entries from AST entries by hashing the same
        // bytes both ways and asserting they differ.
        let bytes = b"x = 1\n";
        let ast = hash_ast_source(std::str::from_utf8(bytes).unwrap()).unwrap();
        let mut h = Sha256::new();
        h.update(b"bytes:v1\n");
        h.update(bytes);
        let fallback = hex::encode(h.finalize());
        assert_ne!(ast, fallback);
    }

    #[test]
    fn strip_ranges_replaces_text_range_debug_format() {
        let input = "Mod { range: 0..42, body: [Expr { range: 4..7 }] }";
        let out = strip_ranges(input);
        assert!(!out.contains("0..42"));
        assert!(!out.contains("4..7"));
    }

    fn scope_hash_at(source: &str, needle: &str) -> String {
        let map = compute_scope_map(source).expect("scope map");
        let start = source
            .find(needle)
            .unwrap_or_else(|| panic!("needle {:?} not found", needle));
        let r = TextRange::new(
            (start as u32).into(),
            ((start + needle.len()) as u32).into(),
        );
        map.scope_hash_for(r)
    }

    #[test]
    fn scope_map_lists_top_level_functions_in_order() {
        let src = "def foo():\n    return 1\n\nclass Bar:\n    pass\n\ndef baz():\n    return 2\n";
        let map = compute_scope_map(src).unwrap();
        assert_eq!(map.scope_qualnames(), vec!["foo", "Bar", "baz"]);
    }

    #[test]
    fn body_edit_does_not_change_sibling_scope_hash() {
        // Editing `foo`'s body must not change the scope hash for a mutant
        // inside `bar`. Prelude is identical (same imports, same signatures),
        // so `bar`'s composite hash should be stable.
        let a = "def foo():\n    return 1\n\ndef bar():\n    return 99\n";
        let b = "def foo():\n    return 42\n\ndef bar():\n    return 99\n";
        assert_eq!(scope_hash_at(a, "99"), scope_hash_at(b, "99"));
    }

    #[test]
    fn body_edit_changes_own_scope_hash() {
        let a = "def foo():\n    return 1\n";
        let b = "def foo():\n    return 2\n";
        // The mutated lexeme itself differs between sources, so use a stable
        // anchor: hash for the `return` statement of foo.
        assert_ne!(scope_hash_at(a, "return 1"), scope_hash_at(b, "return 2"));
    }

    #[test]
    fn prelude_edit_busts_every_scope() {
        // Adding an import changes the prelude → every scope_hash must change.
        let a = "def foo():\n    return 1\n\ndef bar():\n    return 99\n";
        let b = "import os\n\ndef foo():\n    return 1\n\ndef bar():\n    return 99\n";
        assert_ne!(scope_hash_at(a, "99"), scope_hash_at(b, "99"));
        assert_ne!(scope_hash_at(a, "return 1"), scope_hash_at(b, "return 1"));
    }

    #[test]
    fn signature_edit_busts_every_scope() {
        // Changing `foo`'s parameters must invalidate `bar` too — callers in
        // bar might rely on foo's signature.
        let a = "def foo(x):\n    return x\n\ndef bar():\n    return 99\n";
        let b = "def foo(x, y):\n    return x\n\ndef bar():\n    return 99\n";
        assert_ne!(scope_hash_at(a, "99"), scope_hash_at(b, "99"));
    }

    #[test]
    fn module_level_mutant_falls_back_to_module_hash() {
        let src_a = "THRESHOLD = 5\n\ndef foo():\n    return 1\n";
        let src_b = "THRESHOLD = 7\n\ndef foo():\n    return 1\n";
        // Mutant range covering the literal `5` / `7` at module scope.
        let map_a = compute_scope_map(src_a).unwrap();
        let map_b = compute_scope_map(src_b).unwrap();
        let r_a = {
            let s = src_a.find("5").unwrap();
            TextRange::new((s as u32).into(), ((s + 1) as u32).into())
        };
        let r_b = {
            let s = src_b.find("7").unwrap();
            TextRange::new((s as u32).into(), ((s + 1) as u32).into())
        };
        assert_ne!(map_a.scope_hash_for(r_a), map_b.scope_hash_for(r_b));
    }

    #[test]
    fn comment_edit_inside_body_does_not_bust_scope_hash() {
        // Comments aren't in the AST. Should leave scope_hash untouched.
        let a = "def foo():\n    # old\n    return 1\n";
        let b = "def foo():\n    # totally different\n    return 1\n";
        assert_eq!(scope_hash_at(a, "return 1"), scope_hash_at(b, "return 1"));
    }

    #[test]
    fn class_method_edits_isolated_from_sibling_top_level_function() {
        let a = "class C:\n    def m(self):\n        return 1\n\ndef bar():\n    return 99\n";
        let b = "class C:\n    def m(self):\n        return 42\n\ndef bar():\n    return 99\n";
        // Top-level `bar` should not see the class-method edit.
        assert_eq!(scope_hash_at(a, "99"), scope_hash_at(b, "99"));
    }

    #[test]
    fn scope_map_returns_none_on_parse_error() {
        assert!(compute_scope_map("def f(:\n").is_none());
    }

    #[test]
    fn strip_ranges_preserves_isolated_digits() {
        // Bare integers (literal values, list lengths) survive — only the
        // `digits..digits` shape is stripped.
        let input = "Int { value: 42 }";
        assert_eq!(strip_ranges(input), "Int { value: 42 }");
    }

    #[test]
    fn strip_ranges_does_not_collapse_inside_quoted_strings() {
        // String-literal values are embedded verbatim by `{:#?}` and must
        // not be touched, even when they happen to match the range shape.
        let input = "StringLiteral { value: \"1..2\", range: 5..10 }";
        let out = strip_ranges(input);
        assert!(out.contains("\"1..2\""), "string contents must survive");
        assert!(!out.contains("5..10"), "real ranges still get stripped");
    }

    #[test]
    fn strip_ranges_handles_escaped_quote_inside_string() {
        // `\"` inside a Debug-quoted string must not end the in-string run.
        let input = "value: \"a\\\"1..2\\\"b\", range: 3..4";
        let out = strip_ranges(input);
        assert!(out.contains("1..2"), "in-string digits must survive");
        assert!(!out.contains("3..4"), "trailing range must be stripped");
    }

    #[test]
    fn differing_string_literals_with_range_pattern_do_not_collide() {
        let a = "x = \"1..2\"\n";
        let b = "x = \"100..200\"\n";
        assert_ne!(h(a), h(b));
    }

    #[test]
    fn anchors_locate_by_top_level_def_and_relative_offset() {
        let src = "import os\n\ndef f():\n    return 1\n\nclass C:\n    def m(self):\n        return 2\n\ndef f():\n    return 3\n";
        let a = compute_anchors(src).unwrap();
        let at = |needle: &str| {
            let start = src.find(needle).unwrap() as u32;
            a.locate(TextRange::new(start.into(), (start + 1).into()))
        };
        // Methods belong to their top-level class.
        let class_start = src.find("class C").unwrap() as u32;
        assert_eq!(
            at("2"),
            Some(("C", src.find('2').unwrap() as u32 - class_start))
        );
        // A redefinition gets its own label, so the two `f`s never share keys.
        assert_eq!(at("1").map(|(l, _)| l), Some("f"));
        assert_eq!(at("3").map(|(l, _)| l), Some("f#2"));
        // Module level: no anchor.
        assert_eq!(at("os"), None);
    }

    #[test]
    fn anchor_offsets_ignore_edits_above_the_def() {
        let body = "def g(x):\n    return x >= 18\n";
        let a = compute_anchors(body).unwrap();
        let b_src = format!("# comment\ndef f():\n    pass\n\n{body}");
        let b = compute_anchors(&b_src).unwrap();
        let in_a = body.find(">=").unwrap() as u32;
        let in_b = b_src.find(">=").unwrap() as u32;
        assert_eq!(
            a.locate(TextRange::new(in_a.into(), (in_a + 2).into())),
            b.locate(TextRange::new(in_b.into(), (in_b + 2).into())),
        );
    }
}
