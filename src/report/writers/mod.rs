//! Report writers, one per output format.
//!
//! Each writer is an inherent `impl Report { pub fn write_<fmt>(…) }` block
//! living in its own file. Adding a format = drop a new file in here,
//! declare it in `mod {…}` below, and add an `impl Report` block — no
//! changes to `report::mod` or the engine.

pub mod html;
pub mod json;
pub mod junit;
pub mod markdown;

/// Shared XML/HTML attribute escape.
pub(super) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a<b&c>\"d'e"), "a&lt;b&amp;c&gt;&quot;d&apos;e");
    }
}
