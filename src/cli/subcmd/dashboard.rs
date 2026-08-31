//! `fermut dashboard` — emit a single self-contained HTML page combining
//! the trend log with the latest run's survivor drill-down.
//!
//! No JavaScript framework, no external assets. Inline CSS, inline SVG
//! sparkline. Drop the file in CI artifacts or a `gh-pages` branch and
//! reviewers can open it directly.
//!
//! Inputs:
//! - `history.jsonl` for the trend.
//! - Optional JSON report (from `fermut run --json`) for survivor diffs.
//!   Without it, the dashboard still renders survivor IDs but skips
//!   inline diffs (since history doesn't store source bytes).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::history::{self, HistoryEntry, StreakDir};
use crate::report::diff::unified_diff_for;
use crate::report::writers::html_escape;
use crate::report::{MutantOutcome, Report};

#[derive(Debug, Clone)]
pub struct DashboardOpts {
    pub path: PathBuf,
    pub history_path: Option<PathBuf>,
    pub output: PathBuf,
    pub report: Option<PathBuf>,
    pub limit: usize,
    /// After writing the HTML, hand it off to the OS so the user's default
    /// browser pops it up. Best-effort: a failure to launch the helper
    /// process degrades to a printed hint, never an error exit.
    pub open: bool,
}

pub fn dashboard(opts: DashboardOpts) -> Result<()> {
    let history_path = opts
        .history_path
        .clone()
        .unwrap_or_else(|| history::default_history_path(&history::resolve_root(&opts.path)));
    let entries = history::load(&history_path)?;
    let report = match &opts.report {
        Some(p) => Some(load_report(p)?),
        None => None,
    };

    let html = render_dashboard(&entries, opts.limit, report.as_ref(), &history_path);
    if let Some(parent) = opts.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
    }
    std::fs::write(&opts.output, html)
        .with_context(|| format!("writing {}", opts.output.display()))?;
    println!("wrote {}", opts.output.display());
    if opts.open {
        open_in_browser(&opts.output);
    }
    Ok(())
}

/// Best-effort hand-off to the OS's default URL opener. Failure prints a
/// hint and returns — never aborts. We resolve the path to absolute (so
/// `xdg-open` doesn't choke on relative paths from a moved cwd) but only
/// when the canonicalization itself succeeds.
fn open_in_browser(path: &Path) {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "macos")]
    let cmd_args: (&str, Vec<&std::ffi::OsStr>) = ("open", vec![target.as_os_str()]);
    #[cfg(target_os = "linux")]
    let cmd_args: (&str, Vec<&std::ffi::OsStr>) = ("xdg-open", vec![target.as_os_str()]);
    #[cfg(target_os = "windows")]
    let cmd_args: (&str, Vec<&std::ffi::OsStr>) = (
        "cmd",
        vec![
            std::ffi::OsStr::new("/C"),
            std::ffi::OsStr::new("start"),
            std::ffi::OsStr::new(""),
            target.as_os_str(),
        ],
    );
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let cmd_args: (&str, Vec<&std::ffi::OsStr>) = ("", vec![]);

    if cmd_args.0.is_empty() {
        eprintln!("--open is not supported on this platform");
        return;
    }
    match std::process::Command::new(cmd_args.0)
        .args(&cmd_args.1)
        .spawn()
    {
        Ok(_) => {}
        Err(e) => eprintln!(
            "couldn't launch `{}` to open dashboard: {e} (open {} manually)",
            cmd_args.0,
            target.display()
        ),
    }
}

fn load_report(path: &Path) -> Result<Report> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading report {}", path.display()))?;
    let report: Report = serde_json::from_str(&text)
        .with_context(|| format!("parsing report {}", path.display()))?;
    Ok(report)
}

fn render_dashboard(
    entries: &[HistoryEntry],
    limit: usize,
    report: Option<&Report>,
    history_path: &Path,
) -> String {
    let window: &[HistoryEntry] = if entries.len() <= limit {
        entries
    } else {
        &entries[entries.len() - limit..]
    };

    let mut body = String::new();
    body.push_str(&format!(
        "<h1>fermut dashboard</h1>\
         <div class='meta'>history: <code>{}</code> · {} total entries</div>",
        html_escape(&history_path.display().to_string()),
        entries.len()
    ));

    body.push_str(&render_config_warning(window));
    body.push_str(&render_summary(window));
    body.push_str(&render_sparkline(window));
    body.push_str(&render_trend_table(window));
    body.push_str(&render_by_file(window));
    body.push_str(&render_survivors(window, report));

    format!(
        "<!doctype html><html><head><meta charset='utf-8'>\
         <title>fermut dashboard</title><style>{CSS}</style></head>\
         <body>{body}</body></html>"
    )
}

fn render_config_warning(window: &[HistoryEntry]) -> String {
    if !history::mixed_config_hashes(window) {
        return String::new();
    }
    "<section class='card warn'>\
     <h2>⚠ Config mismatch</h2>\
     <p>This window contains runs recorded with different \
     <code>config_hash</code> values (different runner, operator set, \
     pytest args, or coverage setting). Score deltas may reflect the \
     config change rather than a real regression — interpret with care.</p>\
     </section>"
        .into()
}

fn render_summary(window: &[HistoryEntry]) -> String {
    let Some(last) = window.last() else {
        return "<div class='card empty'>No runs recorded yet.</div>".into();
    };
    // Delta the latest run posted against the previous scored run. Suppressed
    // entirely when the latest run is itself scoreless — its headline is N/A,
    // so a delta carried over from older runs would misread as the N/A run's
    // move. Scoreless priors are skipped so the comparison is real-vs-real.
    let mut scored_rev = window.iter().rev().filter(|e| !e.is_scoreless());
    let latest_scored = (!last.is_scoreless()).then(|| scored_rev.next()).flatten();
    let delta_html = match (latest_scored, scored_rev.next()) {
        (Some(cur), Some(p)) => {
            let d = cur.mutation_score - p.mutation_score;
            let cls = if d > 0.05 {
                "up"
            } else if d < -0.05 {
                "down"
            } else {
                "flat"
            };
            let sign = if d >= 0.0 { "+" } else { "" };
            format!("<span class='delta {cls}'>{sign}{d:.1} pts</span>")
        }
        _ => String::new(),
    };
    // The headline reflects the latest run itself — N/A when it scored nothing,
    // never the floored 100.0.
    let score_html = if last.is_scoreless() {
        "<div class='big-score na'>N/A</div>".to_string()
    } else {
        format!("<div class='big-score'>{:.1}%</div>", last.mutation_score)
    };
    let streak_html = match history::trailing_streak(window) {
        Some((StreakDir::Up, n)) => format!(
            "<span class='streak up' title='Score improved {n} runs in a row'>\
             ↑ improving {n} runs</span>"
        ),
        Some((StreakDir::Down, n)) => format!(
            "<span class='streak down' title='Score regressed {n} runs in a row'>\
             ↓ regressing {n} runs</span>"
        ),
        None => String::new(),
    };
    let duration = last
        .duration_ms
        .map(|ms| format!("<span class='aux'>· {:.1}s</span>", ms as f64 / 1000.0))
        .unwrap_or_default();
    let git = match (&last.git_branch, &last.git_sha) {
        (Some(b), Some(s)) => format!(
            "<span class='aux'>· {}@{}</span>",
            html_escape(b),
            html_escape(s)
        ),
        (None, Some(s)) => format!("<span class='aux'>· {}</span>", html_escape(s)),
        (Some(b), None) => format!("<span class='aux'>· {}</span>", html_escape(b)),
        _ => String::new(),
    };
    format!(
        "<section class='card summary'>\
         <h2>Latest run</h2>\
         {score_html}\
         {delta_html}\
         {streak_html}\
         <div class='counts'>\
           <span class='k'>killed: {}</span>\
           <span class='s'>survived: {}</span>\
           <span class='t'>timeout: {}</span>\
           <span class='x'>skipped: {}</span>\
           <span class='e'>errored: {}</span>\
         </div>\
         <div class='aux-line'>{} {duration} {git}</div>\
         </section>",
        last.killed,
        last.survived,
        last.timed_out,
        last.skipped,
        last.errored,
        html_escape(&last.timestamp),
    )
}

fn render_sparkline(window: &[HistoryEntry]) -> String {
    // Header counts every run in the window so it matches the history table;
    // the plot itself uses scored runs only — a scoreless run's vacuous 100.0
    // would draw a phantom spike to the top of the chart.
    let total_runs = window.len();
    let window: Vec<&HistoryEntry> = window.iter().filter(|e| !e.is_scoreless()).collect();
    if window.is_empty() {
        return String::new();
    }
    const W: f64 = 720.0;
    const H: f64 = 120.0;
    const PAD: f64 = 8.0;
    let n = window.len();
    let xs = |i: usize| -> f64 {
        if n == 1 {
            W / 2.0
        } else {
            PAD + (W - 2.0 * PAD) * (i as f64 / (n - 1) as f64)
        }
    };
    let ys = |score: f64| -> f64 {
        let clamped = score.clamp(0.0, 100.0);
        PAD + (H - 2.0 * PAD) * (1.0 - clamped / 100.0)
    };
    let mut points = String::new();
    let mut circles = String::new();
    for (i, e) in window.iter().enumerate() {
        let x = xs(i);
        let y = ys(e.mutation_score);
        if i > 0 {
            points.push(' ');
        }
        points.push_str(&format!("{x:.1},{y:.1}"));
        // The baseline anchor gets a larger, distinctly-classed dot and a
        // "(baseline)" title so run zero is identifiable on the chart.
        let (r, class, suffix) = if e.baseline {
            ("4.5", " class='anchor'", " (baseline)")
        } else {
            ("3", "", "")
        };
        circles.push_str(&format!(
            "<circle cx='{x:.1}' cy='{y:.1}' r='{r}'{class}><title>{} — {:.1}%{suffix}</title></circle>",
            html_escape(&e.timestamp),
            e.mutation_score
        ));
    }
    format!(
        "<section class='card chart'>\
         <h2>Trend ({total_runs} runs)</h2>\
         <svg viewBox='0 0 {W} {H}' role='img' aria-label='mutation score over time'>\
           <line x1='{PAD}' y1='{PAD}' x2='{PAD}' y2='{}' class='axis'/>\
           <line x1='{PAD}' y1='{}' x2='{}' y2='{}' class='axis'/>\
           <polyline points='{points}' class='spark'/>{circles}\
         </svg>\
         <div class='axis-label'>0% – 100% (fixed scale)</div>\
         </section>",
        H - PAD,
        H - PAD,
        W - PAD,
        H - PAD,
    )
}

fn render_trend_table(window: &[HistoryEntry]) -> String {
    if window.is_empty() {
        return String::new();
    }
    let mut rows = String::new();
    let mut prev_score: Option<f64> = None;
    for e in window {
        // Scoreless runs have no real score: N/A cell, no delta, and the
        // vacuous 100.0 never seeds the next row's delta.
        let scoreless = e.is_scoreless();
        let delta = if scoreless {
            None
        } else {
            prev_score.map(|p| e.mutation_score - p)
        };
        let delta_cell = match delta {
            Some(d) if d.abs() < 0.05 => "<td class='flat'>0.0</td>".into(),
            Some(d) if d >= 0.0 => format!("<td class='up'>+{d:.1}</td>"),
            Some(d) => format!("<td class='down'>{d:.1}</td>"),
            None => "<td class='flat'>—</td>".into(),
        };
        let score_cell = if scoreless {
            "<td class='score'>N/A</td>".to_string()
        } else {
            format!("<td class='score'>{:.1}%</td>", e.mutation_score)
        };
        let git = match (&e.git_branch, &e.git_sha) {
            (Some(b), Some(s)) => format!("{}@{}", html_escape(b), html_escape(s)),
            (None, Some(s)) => html_escape(s),
            (Some(b), None) => html_escape(b),
            _ => String::new(),
        };
        // Tag the baseline anchor row so it reads as run zero, not a run.
        let (tr_class, ts_badge) = if e.baseline {
            (" class='baseline'", " <span class='badge'>baseline</span>")
        } else {
            ("", "")
        };
        rows.push_str(&format!(
            "<tr{tr_class}>\
             <td class='ts'>{}{ts_badge}</td>\
             {score_cell}\
             {delta_cell}\
             <td>{}</td><td>{}</td><td>{}</td>\
             <td class='git'>{git}</td>\
             </tr>",
            html_escape(&e.timestamp),
            e.killed,
            e.survived,
            e.timed_out,
        ));
        // Only real scores seed the next delta — a vacuous 100.0 must not.
        if !scoreless {
            prev_score = Some(e.mutation_score);
        }
    }
    format!(
        "<section class='card table-wrap'>\
         <h2>History (last {})</h2>\
         <table>\
           <thead><tr>\
             <th>Timestamp</th><th>Score</th><th>Δ</th>\
             <th>Killed</th><th>Survived</th><th>Timeout</th><th>Git</th>\
           </tr></thead>\
           <tbody>{rows}</tbody>\
         </table></section>",
        window.len(),
    )
}

/// Render a compact "survivors by file" card. Pure history-derived: parses
/// the mutant-id prefix, so works whether or not the user passed a JSON
/// report. Bar widths are normalized against the worst-offender file so
/// proportions read at a glance.
fn render_by_file(window: &[HistoryEntry]) -> String {
    let Some(last) = window.last() else {
        return String::new();
    };
    let Some(ids) = last.survivor_ids.as_ref() else {
        return String::new();
    };
    if ids.is_empty() {
        return String::new();
    }
    let groups = history::survivors_by_file(ids);
    if groups.is_empty() {
        return String::new();
    }
    let ages = history::survivor_age_map(window);
    let max_count = groups.first().map(|(_, v)| v.len()).unwrap_or(1).max(1);
    let mut rows = String::new();
    for (file, members) in &groups {
        let pct = (members.len() as f64 / max_count as f64 * 100.0).round() as u32;
        let max_age = members
            .iter()
            .map(|id| ages.get(id).copied().unwrap_or(1))
            .max()
            .unwrap_or(1);
        rows.push_str(&format!(
            "<tr>\
             <td class='file'>{}</td>\
             <td class='count'>{}</td>\
             <td class='bar-cell'><div class='bar' style='width:{}%'></div></td>\
             <td class='age'>{} run{}</td>\
             </tr>",
            html_escape(file),
            members.len(),
            pct,
            max_age,
            if max_age == 1 { "" } else { "s" },
        ));
    }
    format!(
        "<section class='card table-wrap'>\
         <h2>Survivors by file ({} file{}, {} mutant{})</h2>\
         <table>\
           <thead><tr>\
             <th>File</th><th>Count</th><th>Share</th><th>Oldest survivor</th>\
           </tr></thead>\
           <tbody>{rows}</tbody>\
         </table></section>",
        groups.len(),
        if groups.len() == 1 { "" } else { "s" },
        ids.len(),
        if ids.len() == 1 { "" } else { "s" },
    )
}

fn render_survivors(window: &[HistoryEntry], report: Option<&Report>) -> String {
    let Some(last) = window.last() else {
        return String::new();
    };
    let Some(ids) = last.survivor_ids.as_ref() else {
        return "<section class='card'><h2>Survivors</h2>\
                <p class='hint'>No survivor IDs recorded for the most recent run \
                (entry pre-dates the field). Re-run <code>fermut run …</code> \
                to populate.</p></section>"
            .into();
    };
    if ids.is_empty() {
        return "<section class='card'><h2>Survivors</h2>\
                <p class='hint'>None — every mutant was detected. 🎯</p></section>"
            .into();
    }

    // Map report survivors by mutant id so we can attach the source diff
    // when a report is available.
    let report_by_id: HashMap<&str, &MutantOutcome> = report
        .map(|r| {
            r.outcomes
                .iter()
                .filter(|o| matches!(o, MutantOutcome::Survived { .. }))
                .map(|o| (o.mutant().id.as_str(), o))
                .collect()
        })
        .unwrap_or_default();

    // Age map needs the full window — a survivor's age is its consecutive
    // streak across prior entries, not a property of any single entry.
    let ages = history::survivor_age_map(window);
    // Render survivors sorted by descending age so persistent blockers
    // surface at the top instead of being buried by id-order.
    let mut sorted_ids: Vec<&String> = ids.iter().collect();
    sorted_ids.sort_by(|a, b| {
        let aa = ages.get(a.as_str()).copied().unwrap_or(1);
        let ab = ages.get(b.as_str()).copied().unwrap_or(1);
        ab.cmp(&aa).then_with(|| a.cmp(b))
    });

    let mut items = String::new();
    for id in sorted_ids {
        let age = ages.get(id.as_str()).copied().unwrap_or(1);
        let age_cls = if age >= 5 {
            "age chronic"
        } else if age >= 2 {
            "age"
        } else {
            "age fresh"
        };
        let age_badge = format!("<span class='{age_cls}'>age {age}</span>");
        items.push_str("<li class='surv'>");
        if let Some(outcome) = report_by_id.get(id.as_str()) {
            let m = outcome.mutant();
            items.push_str(&format!(
                "<div class='surv-head'>{age_badge} <code>{}</code>:<b>{}</b> · {} · \
                 <code>{}</code> → <code>{}</code></div>",
                html_escape(&m.file.display().to_string()),
                m.line,
                html_escape(m.operator.name()),
                html_escape(&m.original),
                html_escape(&m.replacement),
            ));
            if let Ok(diff) = unified_diff_for(m) {
                items.push_str(&format!(
                    "<details class='diff'><summary>diff</summary><pre>{}</pre></details>",
                    html_escape(&diff)
                ));
            }
        } else {
            items.push_str(&format!(
                "<div class='surv-head'>{age_badge} <code>{}</code></div>\
                 <div class='hint'>Pass <code>--report &lt;file.json&gt;</code> \
                 to see the inline diff for this survivor.</div>",
                html_escape(id),
            ));
        }
        items.push_str("</li>");
    }

    format!(
        "<section class='card'>\
         <h2>Survivors ({})</h2>\
         <ul class='surv-list'>{items}</ul></section>",
        ids.len(),
    )
}

const CSS: &str = r#"
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, system-ui, sans-serif; margin: 0; padding: 2em; color: #222; background: #f7f7f9; }
h1 { margin: 0 0 .2em; font-size: 1.6em; }
h2 { margin: 0 0 .8em; font-size: 1.1em; color: #444; }
.meta { color: #666; margin-bottom: 1.5em; font-size: 12.5px; }
.card { background: #fff; border-radius: 8px; padding: 1.2em 1.4em; margin-bottom: 1em; box-shadow: 0 1px 2px rgba(0,0,0,0.04); }
.card.warn { background: #fff8e1; border-left: 4px solid #ef6c00; }
.card.warn h2 { color: #b35400; }
.card.warn p { margin: 0; color: #5d4400; font-size: 13px; line-height: 1.55; }
.summary .big-score { font-size: 2.4em; font-weight: 700; color: #2e7d32; display: inline-block; margin-right: .6em; }
.summary .delta { font-size: 1em; font-weight: 600; padding: .15em .55em; border-radius: 999px; vertical-align: middle; }
.summary .delta.up   { background: #e8f5e9; color: #2e7d32; }
.summary .delta.down { background: #fdecea; color: #c62828; }
.summary .delta.flat { background: #eee;     color: #555;    }
.summary .streak { font-size: .9em; font-weight: 600; padding: .15em .55em; border-radius: 999px; vertical-align: middle; margin-left: .4em; }
.summary .streak.up   { background: #e8f5e9; color: #2e7d32; }
.summary .streak.down { background: #fdecea; color: #c62828; }
.summary .counts { margin: .8em 0 .2em; font-family: ui-monospace, monospace; font-size: 12.5px; }
.summary .counts span { margin-right: 1em; }
.summary .counts .k { color: #2e7d32; }
.summary .counts .s { color: #c62828; }
.summary .counts .t { color: #ef6c00; }
.summary .counts .x { color: #757575; }
.summary .counts .e { color: #6a1b9a; }
.summary .aux-line { color: #666; font-size: 12.5px; }
.aux { margin-right: .3em; }
.chart svg { width: 100%; height: auto; max-height: 160px; }
.chart .spark { fill: none; stroke: #3949ab; stroke-width: 2; }
.chart circle { fill: #3949ab; stroke: #fff; stroke-width: 1.5; }
.chart circle.anchor { fill: #ef6c00; stroke: #fff; stroke-width: 1.5; }
.chart .axis { stroke: #ccc; stroke-width: 1; }
.chart .axis-label { color: #888; font-size: 11.5px; margin-top: .3em; }
.table-wrap { padding: 0; overflow: hidden; }
.table-wrap h2 { padding: 1em 1.2em 0; }
table { width: 100%; border-collapse: collapse; font-family: ui-monospace, monospace; font-size: 12.5px; }
th, td { padding: .5em .8em; text-align: left; border-bottom: 1px solid #eee; }
th { background: #fafafa; font-weight: 600; color: #555; font-size: 11.5px; text-transform: uppercase; letter-spacing: .04em; }
td.score { font-weight: 600; }
td.up { color: #2e7d32; }
td.down { color: #c62828; }
td.flat { color: #999; }
td.ts { white-space: nowrap; color: #555; }
td.git { color: #555; }
tr.baseline td { background: #fff8f0; }
.badge { display: inline-block; margin-left: .5em; padding: .05em .45em; border-radius: 999px; background: #ef6c00; color: #fff; font-size: 10.5px; font-weight: 600; letter-spacing: .03em; vertical-align: middle; }
.surv-list { list-style: none; padding: 0; margin: 0; }
.surv { padding: .7em .9em; border-left: 3px solid #c62828; background: #fdecea; border-radius: 4px; margin-bottom: .6em; }
.surv-head { font-family: ui-monospace, monospace; font-size: 12.5px; }
.diff { margin-top: .4em; }
.diff pre { background: #fff; padding: .6em .9em; border-radius: 4px; overflow-x: auto; font-size: 12px; line-height: 1.4; border: 1px solid #f1d0cc; margin: 0; }
.hint { color: #777; font-size: 12px; margin-top: .3em; }
code { background: rgba(0,0,0,0.05); padding: 0 .3em; border-radius: 3px; }
.age { display: inline-block; font-size: 11px; font-weight: 600; padding: .05em .45em; border-radius: 999px; background: #eef2ff; color: #3949ab; margin-right: .4em; vertical-align: middle; }
.age.fresh { background: #fff3e0; color: #ef6c00; }
.age.chronic { background: #fce4ec; color: #ad1457; }
td.file { font-family: ui-monospace, monospace; }
td.count { font-weight: 600; font-variant-numeric: tabular-nums; text-align: right; }
td.bar-cell { width: 40%; padding: .35em .8em; }
td.bar-cell .bar { height: 10px; background: linear-gradient(90deg, #c62828, #ef6c00); border-radius: 3px; }
td.age { color: #555; font-size: 12px; white-space: nowrap; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::CURRENT_SCHEMA_V;

    fn entry(ts: &str, score: f64, survivors: Option<&[&str]>) -> HistoryEntry {
        HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: ts.into(),
            mutation_score: score,
            killed: 0,
            survived: survivors.map(|s| s.len()).unwrap_or(0),
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: None,
            duration_ms: None,
            config_hash: None,
            git_sha: None,
            git_branch: None,
            survivor_ids: survivors.map(|s| s.iter().map(|x| x.to_string()).collect()),
            baseline: false,
        }
    }

    #[test]
    fn dashboard_html_has_expected_sections() {
        let entries = vec![
            entry("2026-01-01T00:00:00Z", 80.0, Some(&["a"])),
            entry("2026-01-02T00:00:00Z", 85.0, Some(&["a", "b"])),
        ];
        let html = render_dashboard(&entries, 10, None, Path::new("/tmp/history.jsonl"));
        assert!(html.contains("fermut dashboard"));
        assert!(html.contains("Latest run"));
        assert!(html.contains("Trend (2 runs)"));
        assert!(html.contains("History"));
        assert!(html.contains("Survivors (2)"));
        assert!(html.contains("85.0%"));
    }

    #[test]
    fn dashboard_scoreless_latest_shows_na_and_no_delta() {
        // Latest run scored nothing (killed+survived+timed_out == 0) → vacuous
        // 100.0. Headline must read N/A, no delta may carry over from the older
        // real run, and the trend header still counts both runs.
        let real = entry("2026-01-01T00:00:00Z", 80.0, Some(&["a"]));
        let scoreless = entry("2026-01-02T00:00:00Z", 100.0, Some(&[]));
        assert!(scoreless.is_scoreless());
        let html = render_dashboard(&[real, scoreless], 10, None, Path::new("/tmp/h.jsonl"));
        // Headline N/A, not a floored 100.0%.
        assert!(html.contains("big-score na"));
        assert!(html.contains(">N/A<"));
        // No summary delta pill carried over from the older run.
        assert!(!html.contains("class='delta"));
        // Header counts BOTH runs even though only the scored one is plotted.
        assert!(html.contains("Trend (2 runs)"));
    }

    #[test]
    fn dashboard_marks_the_baseline_anchor() {
        let mut anchor = entry("2026-01-01T00:00:00Z", 54.0, Some(&["a"]));
        anchor.baseline = true;
        let later = entry("2026-01-02T00:00:00Z", 70.0, Some(&["a"]));
        let html = render_dashboard(&[anchor, later], 10, None, Path::new("/tmp/h.jsonl"));
        // Table row tagged + badged, chart dot distinctly classed and titled.
        assert!(html.contains("<tr class='baseline'>"));
        assert!(html.contains("<span class='badge'>baseline</span>"));
        assert!(html.contains("class='anchor'"));
        assert!(html.contains("(baseline)"));
        // The ordinary later run carries none of the anchor markup.
        assert!(!html.contains("<tr class='baseline'><td class='ts'>2026-01-02"));
    }

    #[test]
    fn dashboard_handles_empty_history() {
        let html = render_dashboard(&[], 10, None, Path::new("/tmp/history.jsonl"));
        assert!(html.contains("No runs recorded yet"));
    }

    #[test]
    fn dashboard_celebrates_zero_survivors() {
        let entries = vec![entry("2026-01-01T00:00:00Z", 100.0, Some(&[]))];
        let html = render_dashboard(&entries, 10, None, Path::new("/tmp/history.jsonl"));
        assert!(html.contains("every mutant was detected"));
    }

    #[test]
    fn dashboard_notes_missing_survivor_ids_for_legacy_entries() {
        let entries = vec![entry("2026-01-01T00:00:00Z", 90.0, None)];
        let html = render_dashboard(&entries, 10, None, Path::new("/tmp/history.jsonl"));
        assert!(html.contains("pre-dates the field"));
    }

    #[test]
    fn dashboard_surfaces_config_mismatch_warning() {
        let mut a = entry("2026-01-01T00:00:00Z", 80.0, Some(&["x"]));
        let mut b = entry("2026-01-02T00:00:00Z", 90.0, Some(&["y"]));
        a.config_hash = Some("aaa".into());
        b.config_hash = Some("bbb".into());
        let html = render_dashboard(&[a, b], 10, None, Path::new("/tmp/h.jsonl"));
        assert!(html.contains("Config mismatch"));
        assert!(html.contains("config_hash"));
    }

    #[test]
    fn dashboard_omits_warning_for_uniform_hashes() {
        let mut a = entry("2026-01-01T00:00:00Z", 80.0, Some(&["x"]));
        let mut b = entry("2026-01-02T00:00:00Z", 90.0, Some(&["y"]));
        a.config_hash = Some("same".into());
        b.config_hash = Some("same".into());
        let html = render_dashboard(&[a, b], 10, None, Path::new("/tmp/h.jsonl"));
        assert!(!html.contains("Config mismatch"));
    }

    #[test]
    fn dashboard_renders_by_file_card_and_age_badges() {
        // Two runs, three survivors in the latest: two in a.py, one in b.py.
        // "x" survives both runs → age 2 (chronic-ish); "z" appears only in
        // the latest → age 1 (fresh).
        let mut e1 = entry("2026-01-01T00:00:00Z", 80.0, None);
        e1.survivor_ids = Some(vec!["src/a.py@1:x->y".into()]);
        let mut e2 = entry("2026-01-02T00:00:00Z", 85.0, None);
        e2.survivor_ids = Some(vec![
            "src/a.py@1:x->y".into(),
            "src/a.py@2:m->n".into(),
            "src/b.py@1:p->q".into(),
        ]);
        let html = render_dashboard(&[e1, e2], 10, None, Path::new("/tmp/h.jsonl"));
        assert!(html.contains("Survivors by file"));
        assert!(html.contains("src/a.py"));
        assert!(html.contains("src/b.py"));
        // Age badge present (at minimum for the fresh ones).
        assert!(html.contains("age 1"));
        assert!(html.contains("age 2"));
    }

    #[test]
    fn dashboard_writes_file_to_output_path() {
        let tmp = tempfile::tempdir().unwrap();
        let hist = tmp.path().join("history.jsonl");
        let e = entry("2026-01-01T00:00:00Z", 90.0, Some(&["x"]));
        history::append(&hist, &e).unwrap();
        let out = tmp.path().join("out").join("dash.html");
        dashboard(DashboardOpts {
            path: tmp.path().to_path_buf(),
            history_path: Some(hist),
            output: out.clone(),
            report: None,
            limit: 10,
            open: false,
        })
        .unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.contains("fermut dashboard"));
    }

    #[test]
    fn dashboard_renders_streak_chip_in_summary() {
        // Three monotonically improving runs → "improving 2 runs" streak.
        let entries = vec![
            entry("2026-01-01T00:00:00Z", 60.0, Some(&["a"])),
            entry("2026-01-02T00:00:00Z", 70.0, Some(&["a"])),
            entry("2026-01-03T00:00:00Z", 80.0, Some(&["a"])),
        ];
        let html = render_dashboard(&entries, 10, None, Path::new("/tmp/h.jsonl"));
        assert!(html.contains("improving 2 runs"));
        assert!(html.contains("class='streak up'"));
    }

    #[test]
    fn dashboard_streak_chip_omitted_when_no_streak() {
        // Two entries: not enough to call it a trend.
        let entries = vec![
            entry("2026-01-01T00:00:00Z", 60.0, Some(&["a"])),
            entry("2026-01-02T00:00:00Z", 70.0, Some(&["a"])),
        ];
        let html = render_dashboard(&entries, 10, None, Path::new("/tmp/h.jsonl"));
        assert!(!html.contains("class='streak"));
    }
}
