//! Self-contained HTML report. Single file, inline CSS, no JS framework —
//! uses `<details>` for collapsible per-file sections.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::html_escape;
use crate::report::diff::unified_diff_for;
use crate::report::{MutantOutcome, Report};

impl Report {
    pub fn write_html(&self, path: &Path) -> Result<()> {
        let c = self.counts();
        let mut by_file: BTreeMap<PathBuf, Vec<&MutantOutcome>> = BTreeMap::new();
        for o in &self.outcomes {
            by_file.entry(o.mutant().file.clone()).or_default().push(o);
        }

        let mut body = String::new();
        body.push_str(&format!(
            "<h1>fermut report</h1><div class='summary'>\
             <span class='k'>killed: {}</span> \
             <span class='s'>survived: {}</span> \
             <span class='t'>timeout: {}</span> \
             <span class='x'>skipped: {}</span> \
             <span class='q'>equivalent: {}</span> \
             <span class='e'>errored: {}</span> \
             <span class='score'>score: {}</span></div>",
            c.killed,
            c.survived,
            c.timed_out,
            c.skipped,
            c.equivalent,
            c.errored,
            c.score_label()
        ));

        for (file, outcomes) in &by_file {
            let killed = outcomes
                .iter()
                .filter(|o| matches!(o, MutantOutcome::Killed { .. }))
                .count();
            let survived = outcomes
                .iter()
                .filter(|o| matches!(o, MutantOutcome::Survived { .. }))
                .count();
            body.push_str(&format!(
                "<details open><summary><b>{}</b> — {} mutants ({} killed, {} survived)</summary>",
                html_escape(&file.display().to_string()),
                outcomes.len(),
                killed,
                survived
            ));
            body.push_str("<ul class='mutants'>");
            for o in outcomes {
                let m = o.mutant();
                let cls = match o {
                    MutantOutcome::Killed { .. } => "killed",
                    MutantOutcome::Survived { .. } => "survived",
                    MutantOutcome::TimedOut { .. } => "timeout",
                    MutantOutcome::Skipped { .. } => "skipped",
                    MutantOutcome::Error { .. } => "error",
                    MutantOutcome::Equivalent { .. } => "equivalent",
                };
                let label = format!(
                    "line {} · {} · <code>{}</code> → <code>{}</code>",
                    m.line,
                    m.operator.name(),
                    html_escape(&m.original),
                    html_escape(&m.replacement)
                );
                body.push_str(&format!(
                    "<li class='m {cls}'><span class='tag'>{}</span> {label}",
                    o.status_label()
                ));
                if matches!(
                    o,
                    MutantOutcome::Survived { .. } | MutantOutcome::TimedOut { .. }
                ) {
                    if let Ok(diff) = unified_diff_for(m) {
                        body.push_str(&format!(
                            "<details class='diff'><summary>diff</summary><pre>{}</pre></details>",
                            html_escape(&diff)
                        ));
                    }
                }
                body.push_str("</li>");
            }
            body.push_str("</ul></details>");
        }

        let html = format!("<!doctype html><html><head><meta charset='utf-8'><title>fermut report</title><style>{}</style></head><body>{}</body></html>", CSS, body);
        std::fs::write(path, html).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

const CSS: &str = r#"
body { font: 14px/1.5 -apple-system, system-ui, sans-serif; margin: 2em; color: #222; }
h1 { margin: 0 0 .5em; }
.summary { background: #f4f4f4; padding: .8em 1em; border-radius: 6px; margin-bottom: 1.5em; }
.summary span { margin-right: 1em; font-weight: 600; }
.summary .k { color: #2e7d32; }
.summary .s { color: #c62828; }
.summary .t { color: #ef6c00; }
.summary .x { color: #757575; }
.summary .e { color: #6a1b9a; }
.summary .score { float: right; }
details { margin-bottom: .8em; }
summary { cursor: pointer; padding: .4em 0; }
.mutants { list-style: none; padding: 0 0 0 1em; margin: 0; }
.m { padding: .25em .4em; border-left: 3px solid #ddd; margin: 2px 0; font-family: ui-monospace, monospace; font-size: 12.5px; }
.m.killed   { border-color: #2e7d32; background: #f1f8f1; }
.m.survived { border-color: #c62828; background: #fdecea; }
.m.timeout  { border-color: #ef6c00; background: #fff3e0; }
.m.skipped  { border-color: #bdbdbd; background: #fafafa; color: #757575; }
.m.error    { border-color: #6a1b9a; background: #f3e5f5; }
.tag { display: inline-block; min-width: 70px; font-weight: 700; text-transform: uppercase; font-size: 10.5px; letter-spacing: .04em; }
code { background: rgba(0,0,0,0.05); padding: 0 .3em; border-radius: 3px; }
.diff { margin-top: .3em; }
.diff pre { background: #fafafa; padding: .6em .9em; border-radius: 4px; overflow-x: auto; font-size: 12px; line-height: 1.4; }
"#;

#[cfg(test)]
mod tests {
    use crate::mutator::{Mutant, Operator};
    use crate::report::testing::make_mutant;
    use crate::report::{MutantOutcome, Report};
    use ruff_text_size::TextRange;

    fn read(r: &Report) -> String {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        r.write_html(tmp.path()).unwrap();
        std::fs::read_to_string(tmp.path()).unwrap()
    }

    #[test]
    fn html_headlines_counts_score_and_inlines_css() {
        let r = Report::new(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        let h = read(&r);
        // Self-contained document with inlined stylesheet — no external assets.
        assert!(h.starts_with("<!doctype html>"));
        assert!(h.contains("<title>fermut report</title>"));
        assert!(h.contains(".m.survived"), "CSS must be inlined");
        // Summary carries every bucket count and the score label.
        assert!(h.contains("killed: 1"));
        assert!(h.contains("survived: 1"));
        assert!(h.contains("score: 50.0%"));
    }

    #[test]
    fn html_emits_a_class_and_tag_for_every_outcome_kind() {
        let r = Report::new(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
            MutantOutcome::timed_out(make_mutant()),
            MutantOutcome::skipped(make_mutant(), "coverage"),
            MutantOutcome::error(make_mutant(), "boom".into()),
            MutantOutcome::equivalent(make_mutant(), "reason", "bytecode"),
        ]);
        let h = read(&r);
        for cls in [
            "killed",
            "survived",
            "timeout",
            "skipped",
            "error",
            "equivalent",
        ] {
            assert!(
                h.contains(&format!("class='m {cls}'")),
                "missing class {cls}"
            );
        }
    }

    #[test]
    fn html_groups_mutants_per_file_with_kill_survive_counts() {
        let mut a = make_mutant();
        a.file = "a.py".into();
        let mut b = make_mutant();
        b.file = "b.py".into();
        let r = Report::new(vec![
            MutantOutcome::killed(a.clone()),
            MutantOutcome::survived(a),
            MutantOutcome::killed(b),
        ]);
        let h = read(&r);
        // Files sort into their own <details> section with a per-file tally.
        assert!(h.contains("<b>a.py</b> — 2 mutants (1 killed, 1 survived)"));
        assert!(h.contains("<b>b.py</b> — 1 mutants (1 killed, 0 survived)"));
    }

    #[test]
    fn html_escapes_markup_in_file_and_code_spans() {
        let mut m = make_mutant();
        m.file = "<x>.py".into();
        m.original = "a & b".into();
        m.replacement = "<script>".into();
        let r = Report::new(vec![MutantOutcome::killed(m)]);
        let h = read(&r);
        // No raw injection survives into the document.
        assert!(!h.contains("<script>"));
        assert!(h.contains("&lt;script&gt;"));
        assert!(h.contains("&lt;x&gt;.py"));
        assert!(h.contains("a &amp; b"));
    }

    #[test]
    fn html_renders_diff_block_for_survivor_with_readable_source() {
        // A survivor whose file exists on disk gets an inline unified diff;
        // killed mutants never do.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f.py");
        std::fs::write(&file, "x = 1 + 2\n").unwrap();
        let m = Mutant {
            id: "d-1".into(),
            file: file.clone(),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(6u32.into(), 7u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
        };
        let r = Report::new(vec![MutantOutcome::survived(m)]);
        let h = read(&r);
        assert!(h.contains("<details class='diff'>"));
        assert!(h.contains("-x = 1 + 2"));
        assert!(h.contains("+x = 1 - 2"));
    }
}
