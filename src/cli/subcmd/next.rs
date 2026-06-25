//! `fermut next` — rank surviving mutants by which one to fix next.
//!
//! The agent's recurring question after a run is "which survivor do I write
//! a test for now?" The coding-agents guide answers it with
//! `jq 'sort_by(.mutant.operator)'` — arbitrary. `next` ranks survivors by
//! expected reward per test using two ordered sort keys:
//!
//! 1. **cluster leverage** — survivors sharing a `(file, operator)` pair *and*
//!    sitting within a couple of lines of each other (see [`CLUSTER_LINE_GAP`])
//!    often fall to one well-aimed test, so a big cluster is a high-leverage
//!    guess. The line-proximity gate matters: measured on the parity corpus,
//!    80% of same-`(file, operator)` groups span >20 lines — unrelated
//!    functions one test can't co-kill — so clustering by file alone overstates
//!    leverage. The representative carries its `sibling_ids` so the agent can
//!    check what one test might also kill.
//! 2. **kill-ease** — concrete-value operators (boundary, compare, constant)
//!    are easy to assert against; behavioral ones (decorator, exception,
//!    loop-iteration) are harder. Breaks ties between equal-size clusters.
//!
//! Each cluster also reports a **gain range**, not a single number, because
//! the co-kill above is a guess:
//!
//! - `min_gain_pts` — points the score climbs from killing the representative
//!   alone (`1 / denom`). The floor: one test, one kill.
//! - `max_gain_pts` — points if *every* mutant in the cluster dies to that one
//!   test (`cluster_size / denom`). The ceiling, realised only when the
//!   co-kill guess holds.
//!
//! Equivalent mutants are already excluded upstream — the detector emits
//! them as a separate `equivalent` status, never `survived` — so there's
//! nothing to drop here. Timeouts are excluded: they aren't killed by an
//! assertion, they need a faster test or a higher `--timeout`.
//!
//! `--max-tokens N` caps the output at a context-window budget: the
//! highest-ranked clusters that fit are emitted, the rest are dropped with
//! a count logged to stderr. The value-ranked order means a budget keeps
//! the survivors most worth an agent's tokens.
//!
//! Reads the JSON report from `fermut run --json`. JSON is the default
//! format; this command is built for machine consumers.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::subcmd::explain::operator_hint;
use crate::mutator::{Mutant, Operator};
use crate::report::{MutantOutcome, Report};

#[derive(Debug, Clone)]
pub struct NextOpts {
    pub report: PathBuf,
    /// Number of ranked clusters to emit. `None` means all. Defaults to 1
    /// at the CLI layer — "the next survivor".
    pub limit: Option<usize>,
    /// Token budget for the emitted list. When set, emit the highest-ranked
    /// prefix whose estimated token count fits, overriding `limit`. The top
    /// entry is always included even if it alone exceeds the budget — a
    /// budget should never starve the agent of its single best target.
    pub max_tokens: Option<usize>,
    pub format: NextFormat,
}

#[derive(Debug, Clone, Copy)]
pub enum NextFormat {
    Human,
    Json,
}

/// Kill-ease tier for an operator. Higher = easier to write a killing test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Ease {
    Low,
    Medium,
    High,
}

/// Concrete-value mutations are easiest to pin with an assertion; structural
/// / behavioral ones need a test that observes control flow or side effects.
fn ease(op: Operator) -> Ease {
    use Operator::*;
    match op {
        BoundaryShift | CompareOpSwap | ArithOpSwap | ConstantReplace | NumberShift
        | NumberToZero | NumberToNeg | StringToEmpty | StringSentinel | BytesSentinel
        | NotInsertion | BoolOpSwap | UnaryOpSwap | StringCaseSwap => Ease::High,
        ReturnValueToNone | AssignValueToNone | DefaultArgToNone | LambdaBodyToNone | ArgToNone
        | NoneToValue | ExprToNone | KeywordArgDrop | DictItemDrop | PositionalDrop
        | SliceBoundDrop | SliceStepMutate | AugAssignSwap => Ease::Medium,
        BreakContinueSwap | RemoveDecorator | ExceptionClassSwap | BareExcept
        | ZeroIterationForLoop | OneIterationForLoop => Ease::Low,
    }
}

/// One ranked cluster: a representative survivor plus its same-`(file,
/// operator)` siblings.
#[derive(Debug, Serialize)]
pub(crate) struct NextEntry {
    rank: usize,
    id: String,
    file: String,
    line: u32,
    operator: String,
    original: String,
    replacement: String,
    /// How many survivors share this `(file, operator)` — the representative
    /// included. `1` means a lone survivor.
    cluster_size: usize,
    ease: Ease,
    /// Points the run's score gains from killing the representative alone
    /// (`1 / denom`). The floor — one test, one guaranteed kill. Rounded to
    /// one decimal.
    min_gain_pts: f64,
    /// Points the score gains if one test kills every mutant in the cluster
    /// (`cluster_size / denom`). The ceiling — realised only if the co-kill
    /// guess holds. Equals `min_gain_pts` for a lone survivor. Rounded to one
    /// decimal.
    max_gain_pts: f64,
    /// Operator-specific tip on how to kill it (shared with `fermut explain`).
    hint: &'static str,
    /// Other survivor ids in the same cluster (excludes `id`). A test
    /// written for the representative often kills these too.
    sibling_ids: Vec<String>,
    /// Whether the run used coverage selection. When `false`, the report had
    /// no coverage data, so a high-`ease` survivor may sit on an unexecuted
    /// line and need a brand-new test, not just a stronger assertion — the
    /// same caveat the `human` format prints. Set per run, identical across
    /// entries; carried here so the JSON (default, agent-facing) format
    /// doesn't drop the signal.
    coverage_selected: bool,
}

/// Rank a report's survivors and apply the `limit` / `max_tokens` selection.
/// Returns `(shown, total)` where `total` is the full ranked count before
/// the cut — callers report how many were dropped. Shared by the `next`
/// subcommand and the MCP server so both rank identically.
pub(crate) fn rank_report(
    report: &Report,
    limit: Option<usize>,
    max_tokens: Option<usize>,
) -> (Vec<NextEntry>, usize) {
    // Score denominator = mutants that count toward the score. Killing one
    // moves the score by `1/denom`; we use it to estimate per-cluster gain.
    let counts = report.counts();
    let denom = counts.killed + counts.timed_out + counts.survived;

    let survivors: Vec<&Mutant> = report
        .outcomes
        .iter()
        .filter_map(|o| match o {
            MutantOutcome::Survived { mutant } => Some(mutant),
            _ => None,
        })
        .collect();

    // `ease` assumes a survivor's line is executed by the suite, so a killing
    // test only needs a stronger assertion. That holds when the run used
    // coverage selection (uncovered mutants become `Skipped` with the
    // `coverage` filter, never `Survived`). If no coverage skip is present the
    // run likely had no coverage data, so a "high ease" survivor might sit on
    // an unexecuted line and need a brand-new test, not just an assertion.
    let coverage_selected = report
        .outcomes
        .iter()
        .any(|o| matches!(o, MutantOutcome::Skipped { filter, .. } if filter == "coverage"));

    let mut entries = rank(&survivors, denom);
    for e in &mut entries {
        e.coverage_selected = coverage_selected;
    }
    let total = entries.len();

    // `max_tokens` overrides `limit`: fit the best-ranked prefix into the
    // budget. Otherwise take the requested count.
    let shown: Vec<NextEntry> = match max_tokens {
        Some(budget) => take_within_budget(entries, budget),
        None => {
            let take = limit.unwrap_or(total);
            entries.into_iter().take(take).collect()
        }
    };
    (shown, total)
}

pub fn next(opts: NextOpts) -> Result<()> {
    if opts.limit == Some(0) {
        anyhow::bail!("--limit must be at least 1 (use --all for every survivor)");
    }
    // `--max-tokens` budgets against the emitted JSON; the budget — and the
    // "fit N of M ... tokens" stderr — are meaningless for `--format human`.
    // Reject the combo rather than emit a number that doesn't describe the
    // output.
    if opts.max_tokens.is_some() && matches!(opts.format, NextFormat::Human) {
        anyhow::bail!(
            "--max-tokens applies to JSON output only; drop --format human or --max-tokens"
        );
    }
    let raw = std::fs::read_to_string(&opts.report)
        .with_context(|| format!("reading {}", opts.report.display()))?;
    let report: Report =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", opts.report.display()))?;

    let (shown, total) = rank_report(&report, opts.limit, opts.max_tokens);
    let coverage_selected = shown.first().map(|e| e.coverage_selected).unwrap_or(true);

    match opts.format {
        NextFormat::Json => println!("{}", serde_json::to_string_pretty(&shown)?),
        NextFormat::Human => print_human(&shown, coverage_selected),
    }

    // Never truncate silently: when a budget dropped clusters, say so on
    // stderr (stdout stays a clean array for the agent to parse).
    if opts.max_tokens.is_some() && shown.len() < total {
        let used: usize = shown.iter().map(estimate_tokens).sum();
        eprintln!(
            "fit {} of {} ranked survivors in ~{} tokens ({} omitted — re-run after killing these, or raise --max-tokens)",
            shown.len(),
            total,
            used,
            total - shown.len(),
        );
    }
    Ok(())
}

/// Estimate the token cost of one entry as it is actually emitted. The JSON
/// format prints pretty (indented) output, so the budget must measure that —
/// compact would undercount the real context cost ~2-3×. ~4 characters per
/// token is the standard rough heuristic.
fn estimate_tokens(entry: &NextEntry) -> usize {
    let len = serde_json::to_string_pretty(entry)
        .map(|s| s.len())
        .unwrap_or(0);
    len.div_ceil(4)
}

/// Greedily keep the highest-ranked entries whose cumulative estimated
/// tokens fit `budget`. The first (top-ranked) entry is always kept so a
/// tight budget still returns the single best target. Stops at the first
/// entry that would overflow — entries are value-ranked, so a cheaper
/// lower-value one further down isn't worth reordering past it.
fn take_within_budget(entries: Vec<NextEntry>, budget: usize) -> Vec<NextEntry> {
    let mut used = 0usize;
    let mut kept = Vec::new();
    for entry in entries {
        let cost = estimate_tokens(&entry);
        if kept.is_empty() || used + cost <= budget {
            used += cost;
            kept.push(entry);
        } else {
            break;
        }
    }
    kept
}

/// Two same-`(file, operator)` survivors join one cluster only if their lines
/// are within this gap. A cluster is the unit of "one test plausibly kills
/// all of these", so it must be a single code *site*, not a whole file.
///
/// Measured on the parity corpus (click, pyjwt, starlette, typer): grouping by
/// `(file, operator)` alone, **80%** of multi-survivor clusters span >20 lines
/// — siblings in unrelated functions that no single test co-kills. A gap of 2
/// keeps a cluster to one statement and its immediate continuation (e.g. both
/// `<=` in `lo <= x <= hi`, or an `if`/`return` pair), which is where co-kill
/// actually happens. See `benchmarks/parity/`.
const CLUSTER_LINE_GAP: u32 = 2;

/// Group survivors into clusters — same `(file, operator)` *and* within
/// [`CLUSTER_LINE_GAP`] lines of a neighbour — then rank them. Sort key,
/// descending: cluster size first (one test often kills a tight cluster),
/// then kill-ease. Ties break ascending on `(file, line, id)` for
/// determinism, so the same report always yields the same ranking.
fn rank(survivors: &[&Mutant], denom: usize) -> Vec<NextEntry> {
    use std::collections::BTreeMap;
    // BTreeMap keyed on (file, operator-name) gives a deterministic initial
    // grouping order before the explicit sort below.
    let mut groups: BTreeMap<(String, &'static str), Vec<&Mutant>> = BTreeMap::new();
    for m in survivors {
        groups
            .entry((m.file.display().to_string(), m.operator.name()))
            .or_default()
            .push(m);
    }

    // Split each (file, operator) group into proximity clusters: sort by line,
    // start a new cluster whenever the line gap to the previous survivor
    // exceeds CLUSTER_LINE_GAP. A spread-out group becomes several clusters,
    // each a genuine one-test target.
    let mut ranked: Vec<(usize, Ease, &Mutant, Vec<&Mutant>)> = Vec::new();
    for mut members in groups.into_values() {
        // Sort by (line, offset) so proximity splitting sees survivors in
        // source order; offset breaks line ties deterministically.
        members.sort_by_key(|m| (m.line, u32::from(m.range.start())));
        let mut cluster: Vec<&Mutant> = vec![members[0]];
        for &m in &members[1..] {
            let prev_line = cluster.last().unwrap().line;
            if m.line.saturating_sub(prev_line) <= CLUSTER_LINE_GAP {
                cluster.push(m);
            } else {
                ranked.push(finish_cluster(std::mem::take(&mut cluster)));
                cluster.push(m);
            }
        }
        ranked.push(finish_cluster(cluster));
    }

    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0) // cluster size desc
            .then(b.1.cmp(&a.1)) // ease desc
            .then(a.2.file.cmp(&b.2.file)) // then deterministic tie-breaks
            .then(a.2.line.cmp(&b.2.line))
            .then(a.2.id.cmp(&b.2.id))
    });

    ranked
        .into_iter()
        .enumerate()
        .map(|(i, (size, ease, rep, siblings))| {
            let (min_gain, max_gain) = if denom > 0 {
                (
                    round1(100.0 / denom as f64),
                    round1(100.0 * size as f64 / denom as f64),
                )
            } else {
                (0.0, 0.0)
            };
            NextEntry {
                rank: i + 1,
                id: rep.id.clone(),
                file: rep.file.display().to_string(),
                line: rep.line,
                operator: rep.operator.name().to_string(),
                original: rep.original.clone(),
                replacement: rep.replacement.clone(),
                cluster_size: size,
                ease,
                min_gain_pts: min_gain,
                max_gain_pts: max_gain,
                hint: operator_hint(rep.operator),
                sibling_ids: siblings.iter().map(|m| m.id.clone()).collect(),
                // Filled in by the caller, which knows the run-level flag.
                coverage_selected: false,
            }
        })
        .collect()
}

/// Reduce a proximity cluster to `(size, ease, representative, siblings)`.
/// Representative = lowest byte offset (first occurrence in the file), stable
/// regardless of report ordering.
fn finish_cluster(mut members: Vec<&Mutant>) -> (usize, Ease, &Mutant, Vec<&Mutant>) {
    members.sort_by_key(|m| u32::from(m.range.start()));
    let size = members.len();
    let rep = members[0];
    let siblings: Vec<&Mutant> = members[1..].to_vec();
    (size, ease(rep.operator), rep, siblings)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn print_human(entries: &[NextEntry], coverage_selected: bool) {
    if entries.is_empty() {
        println!("no survivors — nothing to fix next");
        return;
    }
    if !coverage_selected {
        println!(
            "note: this run shows no coverage selection, so a survivor's line\n      \
             may not be executed by any test — a \"high ease\" target might need\n      \
             a new test, not just a stronger assertion.\n"
        );
    }
    for e in entries {
        let ease = match e.ease {
            Ease::High => "high",
            Ease::Medium => "medium",
            Ease::Low => "low",
        };
        println!(
            "#{} {}:{}  [{}]  `{}` → `{}`",
            e.rank, e.file, e.line, e.operator, e.original, e.replacement
        );
        let gain = if (e.max_gain_pts - e.min_gain_pts).abs() < f64::EPSILON {
            format!("+{:.1} pts", e.min_gain_pts)
        } else {
            format!("+{:.1}–{:.1} pts", e.min_gain_pts, e.max_gain_pts)
        };
        println!(
            "   cluster: {} mutant{}  |  ease: {ease}  |  gain: {gain}",
            e.cluster_size,
            if e.cluster_size == 1 { "" } else { "s" },
        );
        println!("   {}", e.hint);
        if !e.sibling_ids.is_empty() {
            println!(
                "   one test may also kill {} sibling{}",
                e.sibling_ids.len(),
                if e.sibling_ids.len() == 1 { "" } else { "s" },
            );
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn mutant(file: &str, offset: u32, op: Operator, line: u32) -> Mutant {
        Mutant {
            id: format!("{file}@{offset}:{}:x->y", op.name()),
            file: PathBuf::from(file),
            operator: op,
            range: TextRange::new(offset.into(), (offset + 2).into()),
            original: "x".into(),
            replacement: "y".into(),
            line,
        }
    }

    #[test]
    fn larger_cluster_ranks_first() {
        // Two boundary survivors in a.py, one in b.py. a.py cluster wins on
        // size even though both are equally easy.
        let a1 = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let a2 = mutant("a.py", 20, Operator::BoundaryShift, 2);
        let b1 = mutant("b.py", 30, Operator::BoundaryShift, 3);
        let survivors = vec![&a1, &a2, &b1];
        let ranked = rank(&survivors, 10);
        assert_eq!(ranked[0].file, "a.py");
        assert_eq!(ranked[0].cluster_size, 2);
        assert_eq!(ranked[0].sibling_ids.len(), 1);
        assert_eq!(ranked[1].file, "b.py");
        assert_eq!(ranked[1].cluster_size, 1);
    }

    #[test]
    fn ease_breaks_size_ties() {
        // Two singleton clusters: a high-ease boundary beats a low-ease
        // decorator removal.
        let easy = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let hard = mutant("b.py", 20, Operator::RemoveDecorator, 2);
        let survivors = vec![&hard, &easy];
        let ranked = rank(&survivors, 10);
        assert_eq!(ranked[0].operator, "boundary-shift");
        assert_eq!(ranked[0].ease, Ease::High);
        assert_eq!(ranked[1].ease, Ease::Low);
    }

    #[test]
    fn representative_is_lowest_offset() {
        // Same line, two offsets (e.g. `lo <= x <= hi`) — one cluster, rep is
        // the lower offset.
        let late = mutant("a.py", 99, Operator::CompareOpSwap, 1);
        let early = mutant("a.py", 12, Operator::CompareOpSwap, 1);
        let survivors = vec![&late, &early];
        let ranked = rank(&survivors, 10);
        assert_eq!(ranked[0].cluster_size, 2);
        assert!(ranked[0].id.contains("@12:"), "rep should be offset 12");
        assert_eq!(ranked[0].sibling_ids.len(), 1);
        assert!(ranked[0].sibling_ids[0].contains("@99:"));
    }

    #[test]
    fn same_file_operator_far_apart_splits_into_clusters() {
        // Two boundary survivors in a.py but 50 lines apart — different code
        // sites, so they must NOT share a cluster (one test won't kill both).
        let near = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let far = mutant("a.py", 20, Operator::BoundaryShift, 51);
        let ranked = rank(&[&near, &far], 10);
        assert_eq!(ranked.len(), 2, "far-apart survivors are separate clusters");
        assert!(ranked.iter().all(|e| e.cluster_size == 1));
    }

    #[test]
    fn adjacent_lines_within_gap_share_a_cluster() {
        // Lines 1 and 3 — gap 2, within CLUSTER_LINE_GAP — stay together.
        let a = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let b = mutant("a.py", 20, Operator::BoundaryShift, 3);
        let ranked = rank(&[&a, &b], 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].cluster_size, 2);
    }

    #[test]
    fn gain_range_floor_is_one_kill_ceiling_is_whole_cluster() {
        // 3-mutant cluster, denom 30 → killing the rep alone moves the score
        // 1/30 = 3.3 pts; killing all three moves it 3/30 = 10 pts.
        let m1 = mutant("a.py", 1, Operator::ArithOpSwap, 1);
        let m2 = mutant("a.py", 2, Operator::ArithOpSwap, 2);
        let m3 = mutant("a.py", 3, Operator::ArithOpSwap, 3);
        let survivors = vec![&m1, &m2, &m3];
        let ranked = rank(&survivors, 30);
        assert!((ranked[0].min_gain_pts - 3.3).abs() < f64::EPSILON);
        assert!((ranked[0].max_gain_pts - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lone_survivor_has_equal_min_and_max_gain() {
        let m = mutant("a.py", 1, Operator::ArithOpSwap, 1);
        let ranked = rank(&[&m], 10);
        assert_eq!(ranked[0].cluster_size, 1);
        assert!((ranked[0].min_gain_pts - ranked[0].max_gain_pts).abs() < f64::EPSILON);
    }

    #[test]
    fn different_operators_same_file_are_distinct_clusters() {
        let b = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let c = mutant("a.py", 20, Operator::CompareOpSwap, 2);
        let survivors = vec![&b, &c];
        let ranked = rank(&survivors, 10);
        assert_eq!(ranked.len(), 2);
        assert!(ranked.iter().all(|e| e.cluster_size == 1));
    }

    #[test]
    fn empty_survivors_yields_empty_ranking() {
        assert!(rank(&[], 0).is_empty());
    }

    #[test]
    fn budget_keeps_highest_ranked_prefix() {
        // Three singleton clusters, ranked easy→hard. A budget tight enough
        // to fit only the first two drops the third.
        let a = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let b = mutant("b.py", 20, Operator::CompareOpSwap, 2);
        let c = mutant("c.py", 30, Operator::RemoveDecorator, 3);
        let ranked = rank(&[&a, &b, &c], 10);
        let per = estimate_tokens(&ranked[0]);
        // Budget for ~2 entries (each entry differs slightly; pad generously
        // but below 3×).
        let budget = estimate_tokens(&ranked[0]) + estimate_tokens(&ranked[1]);
        let kept = take_within_budget(ranked, budget);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].rank, 1);
        assert_eq!(kept[1].rank, 2);
        assert!(per > 0);
    }

    #[test]
    fn budget_always_keeps_top_entry_even_if_over() {
        // Budget of 0 still returns the single best target.
        let a = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let b = mutant("b.py", 20, Operator::CompareOpSwap, 2);
        let ranked = rank(&[&a, &b], 10);
        let kept = take_within_budget(ranked, 0);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rank, 1);
    }

    #[test]
    fn budget_large_enough_keeps_all() {
        let a = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let b = mutant("b.py", 20, Operator::CompareOpSwap, 2);
        let ranked = rank(&[&a, &b], 10);
        let kept = take_within_budget(ranked, 100_000);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn ranking_is_deterministic_regardless_of_input_order() {
        let m1 = mutant("a.py", 10, Operator::BoundaryShift, 1);
        let m2 = mutant("b.py", 20, Operator::CompareOpSwap, 2);
        let m3 = mutant("c.py", 30, Operator::ArithOpSwap, 3);
        let forward = rank(&[&m1, &m2, &m3], 10);
        let reverse = rank(&[&m3, &m2, &m1], 10);
        let ids_f: Vec<&str> = forward.iter().map(|e| e.id.as_str()).collect();
        let ids_r: Vec<&str> = reverse.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids_f, ids_r);
    }
}
