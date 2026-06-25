//! Source-range splicing.
//!
//! fermut never re-serializes a mutated AST — it splices the replacement
//! into the original source at the byte range the visitor recorded. That
//! keeps formatting, comments, encoding, and quote style intact across every
//! mutant, and limits the patch surface to exactly what the operator
//! changed.

use ruff_text_size::TextRange;

/// Replace a byte range of `source` with `replacement`, returning a new String.
/// Uses byte offsets from `ruff_text_size::TextRange`.
pub fn patch_source(source: &str, range: TextRange, replacement: &str) -> String {
    let start: usize = range.start().into();
    let end: usize = range.end().into();
    debug_assert!(start <= end && end <= source.len(), "range out of bounds");
    // UTF-8 boundary check: slicing mid-codepoint panics in `&source[..start]`
    // with "byte index N is not a char boundary". Ruff's parser is supposed
    // to hand us codepoint-aligned ranges, but a buggy operator computing
    // its own range can produce a misaligned one — and that panic would
    // take down a rayon worker rather than show up as a clean error.
    // Catch it in debug builds; in release, panic-on-slice still wins
    // over silently corrupting the source.
    debug_assert!(
        source.is_char_boundary(start) && source.is_char_boundary(end),
        "range ({start}..{end}) not on UTF-8 codepoint boundaries"
    );
    let mut out = String::with_capacity(source.len() + replacement.len());
    out.push_str(&source[..start]);
    out.push_str(replacement);
    out.push_str(&source[end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_text_size::TextSize;

    fn r(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    #[test]
    fn replaces_inner_range() {
        assert_eq!(patch_source("a + b", r(2, 3), "-"), "a - b");
    }

    #[test]
    fn replaces_at_start() {
        assert_eq!(patch_source("abc", r(0, 1), "X"), "Xbc");
    }

    #[test]
    fn replaces_at_end() {
        assert_eq!(patch_source("abc", r(2, 3), "X"), "abX");
    }

    #[test]
    fn replaces_whole_string() {
        assert_eq!(patch_source("abc", r(0, 3), "XYZ"), "XYZ");
    }

    #[test]
    fn zero_width_insertion() {
        assert_eq!(patch_source("ac", r(1, 1), "b"), "abc");
    }

    #[test]
    fn empty_replacement_deletes_range() {
        assert_eq!(patch_source("hello world", r(5, 6), ""), "helloworld");
    }

    #[test]
    fn replacement_can_be_longer_than_range() {
        assert_eq!(patch_source("a-b", r(1, 2), "+++"), "a+++b");
    }

    #[test]
    fn replaces_inside_multibyte_string_at_codepoint_boundary() {
        // 漢字 occupies 6 bytes (3 each). The replacement target is the
        // `=` at byte 2; the rest of the string contains multibyte
        // characters whose bytes are sliced unchanged. Verifies that
        // codepoint-aligned ranges around multibyte content work.
        let src = "x = \"漢字\"";
        // byte layout: x(0) ' '(1) =(2) ' '(3) "(4) 漢(5..8) 字(8..11) "(11)
        assert_eq!(patch_source(src, r(2, 3), "+"), "x + \"漢字\"");
    }

    #[test]
    #[should_panic(expected = "UTF-8")]
    fn debug_assert_catches_non_boundary_range() {
        // 漢 starts at byte 4 and occupies bytes 4..7. Slicing at byte 5
        // (mid-codepoint) is illegal. In debug builds the assert fires
        // with a clear message; without the assert, the string-slice
        // panic at runtime would be less informative and could take
        // down a rayon worker silently.
        let src = "x = 漢字";
        let _ = patch_source(src, r(5, 6), "X");
    }
}
