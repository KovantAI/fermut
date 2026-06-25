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
             <span class='score'>score: {:.1}%</span></div>",
            c.killed,
            c.survived,
            c.timed_out,
            c.skipped,
            c.equivalent,
            c.errored,
            c.mutation_score()
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
