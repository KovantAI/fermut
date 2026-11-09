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

use std::path::PathBuf;

use anyhow::Result;

use crate::report::Report;

/// Optional file-report destinations, one per output format. Bundling them
/// keeps `fermut run` and `fermut merge` off a long `Option<PathBuf>` argument
/// list, and gives both a single place to write every requested format.
#[derive(Clone, Debug, Default)]
pub struct ReportSinks {
    pub json: Option<PathBuf>,
    pub junit: Option<PathBuf>,
    pub html: Option<PathBuf>,
    pub markdown: Option<PathBuf>,
}

impl ReportSinks {
    /// Whether any file sink is configured.
    pub fn any(&self) -> bool {
        self.json.is_some()
            || self.junit.is_some()
            || self.html.is_some()
            || self.markdown.is_some()
    }

    /// Write every configured format with its default renderer. `fermut merge`
    /// uses this directly; `fermut run` writes markdown itself so it can layer
    /// in the trend history block, but writes the rest the same way.
    pub fn write_all(&self, report: &Report) -> Result<()> {
        if let Some(p) = &self.json {
            report.write_json(p)?;
        }
        if let Some(p) = &self.junit {
            report.write_junit(p)?;
        }
        if let Some(p) = &self.html {
            report.write_html(p)?;
        }
        if let Some(p) = &self.markdown {
            report.write_markdown(p)?;
        }
        Ok(())
    }
}

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

    #[test]
    fn write_all_writes_every_configured_sink() {
        let tmp = tempfile::tempdir().unwrap();
        let p = |n: &str| Some(tmp.path().join(n));
        let sinks = ReportSinks {
            json: p("r.json"),
            junit: p("r.xml"),
            html: p("r.html"),
            markdown: p("r.md"),
        };
        assert!(sinks.any());
        assert!(!ReportSinks::default().any());
        let report = Report::new(vec![crate::report::MutantOutcome::survived(
            crate::report::testing::make_mutant(),
        )]);
        sinks.write_all(&report).unwrap();
        for n in ["r.json", "r.xml", "r.html", "r.md"] {
            let body = std::fs::read_to_string(tmp.path().join(n)).unwrap();
            assert!(!body.is_empty(), "{n} empty");
        }
    }
}
