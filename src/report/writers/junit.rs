//! JUnit XML report. Compatible with Jenkins, GitHub Actions, GitLab CI test
//! reporters: survivors register as `<failure>`, filter-skipped as `<skipped>`,
//! errored mutants as `<error>`.

use std::path::Path;

use anyhow::{Context, Result};

use super::xml_escape;
use crate::report::{MutantOutcome, Report};

impl Report {
    pub fn write_junit(&self, path: &Path) -> Result<()> {
        let c = self.counts();
        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str(&format!(
            "<testsuites name=\"fermut\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
            c.total(),
            c.survived,
            c.errored,
            c.skipped + c.equivalent
        ));
        xml.push_str(&format!(
            "  <testsuite name=\"fermut\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
            c.total(),
            c.survived,
            c.errored,
            c.skipped + c.equivalent
        ));
        for o in &self.outcomes {
            let m = o.mutant();
            let classname = xml_escape(&m.file.display().to_string());
            let name = xml_escape(&format!(
                "{}:{} [{}] {}->{}",
                m.file.display(),
                m.line,
                m.operator.name(),
                m.original,
                m.replacement
            ));
            xml.push_str(&format!(
                "    <testcase classname=\"{classname}\" name=\"{name}\">"
            ));
            match o {
                MutantOutcome::Killed { .. } => {}
                MutantOutcome::Survived { .. } => {
                    xml.push_str(
                        "<failure message=\"mutant survived (test suite did not detect it)\"/>",
                    );
                }
                MutantOutcome::TimedOut { .. } => {
                    xml.push_str("<failure message=\"mutant timed out\"/>");
                }
                MutantOutcome::Skipped { filter, .. } => {
                    xml.push_str(&format!(
                        "<skipped message=\"filtered by {}\"/>",
                        xml_escape(filter)
                    ));
                }
                MutantOutcome::Error { message, .. } => {
                    xml.push_str(&format!("<error message=\"{}\"/>", xml_escape(message)));
                }
                MutantOutcome::Equivalent { source, reason, .. } => {
                    xml.push_str(&format!(
                        "<skipped message=\"equivalent mutant ({}): {}\"/>",
                        xml_escape(source),
                        xml_escape(reason)
                    ));
                }
            }
            xml.push_str("</testcase>\n");
        }
        xml.push_str("  </testsuite>\n</testsuites>\n");
        std::fs::write(path, xml).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::report::testing::make_mutant;
    use crate::report::{MutantOutcome, Report};

    #[test]
    fn write_junit_xml_well_formed_header() {
        let r = Report::new(vec![MutantOutcome::survived(make_mutant())]);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_junit(tmp.path()).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(written.starts_with("<?xml version=\"1.0\""));
        assert!(written.contains("<testsuite"));
        assert!(written.contains("<failure"));
    }
}
