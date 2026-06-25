//! UTF-8 BOM stripping and PEP 263 encoding-directive parsing.

/// Strip a leading UTF-8 BOM (`U+FEFF`) if present. ruff's parser would
/// otherwise reject it on some versions and the byte offsets we get out
/// would be misaligned against the file the user sees.
pub fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Best-effort PEP 263 encoding-declaration check. Only the first two lines
/// matter. We allow utf-8 / ascii / no declaration; anything else means
/// the on-disk bytes likely aren't the Unicode our string already represents
/// (Rust's `read_to_string` is UTF-8 only), so skip the file rather than
/// mis-parse.
pub fn is_utf8_compatible_encoding(source: &str) -> bool {
    for line in source.lines().take(2) {
        if let Some(name) = extract_encoding_name(line) {
            let normalized = name.to_ascii_lowercase().replace('_', "-");
            return matches!(normalized.as_str(), "utf-8" | "utf8" | "ascii" | "us-ascii");
        }
    }
    true
}

fn extract_encoding_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let after_hash = &trimmed[1..];
    let key_idx = after_hash.find("coding")?;
    let rest = &after_hash[key_idx + "coding".len()..];
    let sep = rest.chars().next()?;
    if sep != ':' && sep != '=' {
        return None;
    }
    let mut rest = rest[sep.len_utf8()..].trim_start();
    rest = rest.split_whitespace().next().unwrap_or("");
    rest = rest.trim_end_matches([',', ';', '-']);
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_bom_removes_leading_bom() {
        assert_eq!(strip_bom("\u{feff}print('hi')"), "print('hi')");
    }

    #[test]
    fn strip_bom_noop_without_bom() {
        assert_eq!(strip_bom("print('hi')"), "print('hi')");
    }

    #[test]
    fn no_encoding_declaration_passes() {
        assert!(is_utf8_compatible_encoding("x = 1\n"));
    }

    #[test]
    fn utf8_declaration_passes() {
        assert!(is_utf8_compatible_encoding(
            "# -*- coding: utf-8 -*-\nx = 1\n"
        ));
        assert!(is_utf8_compatible_encoding(
            "#!/usr/bin/env python\n# coding: utf-8\n"
        ));
    }

    #[test]
    fn ascii_declaration_passes() {
        assert!(is_utf8_compatible_encoding("# coding: ascii\n"));
    }

    #[test]
    fn latin1_declaration_rejected() {
        assert!(!is_utf8_compatible_encoding("# -*- coding: latin-1 -*-\n"));
    }

    #[test]
    fn declaration_after_two_lines_ignored() {
        let src = "\n\n# -*- coding: latin-1 -*-\nx = 1\n";
        assert!(is_utf8_compatible_encoding(src));
    }

    #[test]
    fn extract_encoding_handles_both_separators() {
        assert_eq!(extract_encoding_name("# coding: utf-8"), Some("utf-8"));
        assert_eq!(extract_encoding_name("# coding=utf-8"), Some("utf-8"));
        assert_eq!(
            extract_encoding_name("# -*- coding: utf-8 -*-"),
            Some("utf-8")
        );
        assert_eq!(extract_encoding_name("x = 1"), None);
    }
}
