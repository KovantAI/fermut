//! GitHub Actions workflow-command emission.
//!
//! Survivors become `::error`, timeouts `::warning`, errored mutants `::error`
//! with the runtime error message. Killed and filter-skipped outcomes are
//! silent — no PR noise for the expected case.

use super::{MutantOutcome, Report};

impl Report {
    /// Emit one GitHub Actions workflow command per non-killed outcome.
    pub fn print_github_annotations(&self) {
        for o in &self.outcomes {
            match o {
                MutantOutcome::Killed { .. }
                | MutantOutcome::Skipped { .. }
                | MutantOutcome::Equivalent { .. } => {}
                MutantOutcome::Survived { mutant } => {
                    println!(
                        "::error file={file},line={line},title=Mutant survived::[{op}] {orig} -> {repl}",
                        file = gha_escape(&mutant.file.display().to_string()),
                        line = mutant.line,
                        op = mutant.operator.name(),
                        orig = gha_escape(&mutant.original),
                        repl = gha_escape(&mutant.replacement),
                    );
                }
                MutantOutcome::TimedOut { mutant } => {
                    println!(
                        "::warning file={file},line={line},title=Mutant timed out::[{op}] {orig} -> {repl}",
                        file = gha_escape(&mutant.file.display().to_string()),
                        line = mutant.line,
                        op = mutant.operator.name(),
                        orig = gha_escape(&mutant.original),
                        repl = gha_escape(&mutant.replacement),
                    );
                }
                MutantOutcome::Error { mutant, message } => {
                    println!(
                        "::error file={file},line={line},title=Mutant error::{msg}",
                        file = gha_escape(&mutant.file.display().to_string()),
                        line = mutant.line,
                        msg = gha_escape(message),
                    );
                }
            }
        }
    }
}

/// Escape special characters for GitHub Actions workflow commands. The
/// runner uses `%0A` / `%0D` / `%25` percent-encoding for newlines, carriage
/// returns, and `%` respectively.
fn gha_escape(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gha_escape_replaces_specials() {
        assert_eq!(gha_escape("100%"), "100%25");
        assert_eq!(gha_escape("a\nb"), "a%0Ab");
        assert_eq!(gha_escape("a\rb"), "a%0Db");
        assert_eq!(gha_escape("plain"), "plain");
    }
}
