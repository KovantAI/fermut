//! Cross-entry analytics: survivor diffs, streaks, ages, regressions.

use super::entry::HistoryEntry;

/// True when the window contains at least two entries with distinct
/// `config_hash` values. Entries missing a hash (older schema) are
/// ignored so a mix of "tagged + untagged" doesn't trigger the warning.
/// Used by `fermut trend` and the dashboard to flag windows where a
/// score change might be a config shift rather than a real regression.
pub fn mixed_config_hashes(entries: &[HistoryEntry]) -> bool {
    let mut seen: Option<&str> = None;
    for e in entries {
        let Some(h) = e.config_hash.as_deref() else {
            continue;
        };
        match seen {
            None => seen = Some(h),
            Some(prev) if prev != h => return true,
            _ => {}
        }
    }
    false
}

/// Set diff of survivor IDs between two entries. Returns
/// `(newly_surviving, newly_killed)`: the first list is mutants that
/// survive `curr` but didn't survive `prev` (regressions); the second
/// is mutants that survived `prev` but no longer survive `curr`
/// (improvements). Both lists are sorted for stable output. Returns
/// `None` when either entry lacks recorded survivor IDs — older
/// entries from before that field existed can't be diffed.
pub fn survivor_diff<'a>(
    prev: &'a HistoryEntry,
    curr: &'a HistoryEntry,
) -> Option<(Vec<&'a str>, Vec<&'a str>)> {
    let prev_ids = prev.survivor_ids.as_ref()?;
    let curr_ids = curr.survivor_ids.as_ref()?;
    let prev_set: std::collections::HashSet<&str> = prev_ids.iter().map(String::as_str).collect();
    let curr_set: std::collections::HashSet<&str> = curr_ids.iter().map(String::as_str).collect();
    let mut newly_surviving: Vec<&str> = curr_set.difference(&prev_set).copied().collect();
    let mut newly_killed: Vec<&str> = prev_set.difference(&curr_set).copied().collect();
    newly_surviving.sort_unstable();
    newly_killed.sort_unstable();
    Some((newly_surviving, newly_killed))
}

/// Same-direction score moves trailing the window. Walks from the most
/// recent entry backwards counting consecutive deltas with the same sign;
/// ties (`|d| < 0.05`) break the streak. Returns the count and whether
/// the streak is up or down. Requires at least two moves (three entries)
/// to report anything — a single move isn't a trend.
///
/// Shared by `fermut trend` (CLI table) and `fermut dashboard` (summary
/// card) so the two surfaces never drift on what counts as a streak.
pub fn trailing_streak(entries: &[HistoryEntry]) -> Option<(StreakDir, usize)> {
    // Only comparable runs seed a streak. Scoreless runs carry a vacuous 100.0
    // (a real 85% next to a fake 100% looks like a 15pt move); a `--max-time`
    // partial run carries a nondeterministic subset score that would invent or
    // break a streak the same way. Drop both so the streak reflects only runs
    // that measured a real, full score. Matches `trend_step`/`regression_against`.
    let scores: Vec<f64> = entries
        .iter()
        .filter(|e| e.is_comparable())
        .map(|e| e.mutation_score)
        .collect();
    if scores.len() < 3 {
        return None;
    }
    let mut dir: Option<StreakDir> = None;
    let mut count = 0usize;
    for w in scores.windows(2).rev() {
        let delta = w[1] - w[0];
        let this = if delta > 0.05 {
            StreakDir::Up
        } else if delta < -0.05 {
            StreakDir::Down
        } else {
            break;
        };
        match dir {
            None => {
                dir = Some(this);
                count = 1;
            }
            Some(d) if d == this => count += 1,
            _ => break,
        }
    }
    if count >= 2 {
        dir.map(|d| (d, count))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreakDir {
    Up,
    Down,
}

/// Consecutive trailing runs each id in `entries.last()` has survived.
/// Walks backwards from the most recent entry: an id's age is the count of
/// uninterrupted earlier entries that also list it as a survivor. Returns
/// an empty map when the window is empty or the last entry lacks survivor
/// IDs. Entries missing survivor IDs (older schema) terminate a streak —
/// we can't claim an id "survived" a run we have no survivor list for.
///
/// Age 1 means "first appearance in last run" (no prior). Age 5 means
/// "alive in last 5 consecutive runs." That's the signal teams want for
/// triage: high-age survivors are the persistent blockers, age-1 entries
/// are new regressions worth checking before they settle in.
pub fn survivor_age_map(entries: &[HistoryEntry]) -> std::collections::HashMap<&str, usize> {
    let mut out: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let Some(last) = entries.last() else {
        return out;
    };
    let Some(last_ids) = last.survivor_ids.as_deref() else {
        return out;
    };
    for id in last_ids {
        out.insert(id.as_str(), 1);
    }
    // Walk older → newer-1, oldest first wouldn't help; we need the most
    // recent contiguous block, so iterate backwards from the second-to-last.
    let mut still_alive: std::collections::HashSet<&str> =
        last_ids.iter().map(String::as_str).collect();
    for prior in entries[..entries.len() - 1].iter().rev() {
        let Some(prior_ids) = prior.survivor_ids.as_deref() else {
            // Missing data breaks the streak for everyone still being counted.
            break;
        };
        let prior_set: std::collections::HashSet<&str> =
            prior_ids.iter().map(String::as_str).collect();
        // Survivors no longer present in this older entry can't extend their
        // streak any further; drop them so subsequent iterations don't keep
        // bumping them.
        still_alive.retain(|id| prior_set.contains(id));
        if still_alive.is_empty() {
            break;
        }
        for id in &still_alive {
            if let Some(v) = out.get_mut(id) {
                *v += 1;
            }
        }
    }
    out
}

/// Group survivor IDs by the file they target. The id format produced by
/// `mutator::visitor::Collector::push` is `{file}@{offset}:{orig}->{repl}`,
/// so we split at the last `@` followed by a digit-led `:` segment to
/// extract the file prefix. IDs that don't match the expected shape (older
/// schemas, manual edits) fall into a `"<unknown>"` bucket so they remain
/// visible rather than silently dropped. Returns a vec of `(file, ids)`
/// sorted by descending survivor count, ties broken alphabetically — so
/// the worst offender always renders first.
pub fn survivors_by_file(ids: &[String]) -> Vec<(String, Vec<&str>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<&str>> =
        std::collections::BTreeMap::new();
    for id in ids {
        let file = parse_id_file(id).unwrap_or("<unknown>").to_string();
        groups.entry(file).or_default().push(id.as_str());
    }
    let mut v: Vec<(String, Vec<&str>)> = groups.into_iter().collect();
    v.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Extract the `{file}` prefix from a mutant ID shaped
/// `{file}@{offset:digits}:{orig}->{repl}`. Returns `None` when the
/// expected delimiters aren't found — we'd rather report `<unknown>`
/// than silently misattribute.
fn parse_id_file(id: &str) -> Option<&str> {
    // Walk from the right so file paths that happen to contain `@`
    // (rare but legal on POSIX) still resolve to the longest valid prefix.
    let mut search_end = id.len();
    while let Some(at_off) = id[..search_end].rfind('@') {
        let after = &id[at_off + 1..];
        // Need: digits, then `:`, then anything.
        let digits_end = after.find(|c: char| !c.is_ascii_digit())?;
        if digits_end > 0 && after.as_bytes().get(digits_end) == Some(&b':') {
            return Some(&id[..at_off]);
        }
        search_end = at_off;
    }
    None
}

/// Positive point-drop between `current` and the most recent entry in
/// `prior` on the same branch. When either side has no recorded branch
/// we treat the pair as comparable (best effort — better signal than
/// skipping the gate entirely on detached HEAD / pre-git-tagged
/// entries). Returns `None` when there is no comparable prior or the
/// score didn't regress.
///
/// Takes `current` separately from `prior` because the regression gate
/// must reason about the in-memory run, not whatever the history file
/// happens to contain — append can silently fail (read-only FS, disk
/// full) and leave the file's tail one entry behind reality.
pub fn regression_against(prior: &[HistoryEntry], current: &HistoryEntry) -> Option<f64> {
    // A scoreless run has a vacuous 100.0, not a real score, and a `--max-time`
    // partial run scored a nondeterministic subset — neither side of the
    // comparison may be one, or we fabricate a drop (real prev vs fake/partial
    // current) or hide one (fake/partial prev vs real current). `is_comparable`
    // rejects both.
    if !current.is_comparable() {
        return None;
    }
    let current_branch = current.git_branch.as_deref();
    let prev = prior.iter().rev().filter(|e| e.is_comparable()).find(|e| {
        match (e.git_branch.as_deref(), current_branch) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    })?;
    let drop = prev.mutation_score - current.mutation_score;
    if drop > 0.0 {
        Some(drop)
    } else {
        None
    }
}

/// Convenience over [`regression_against`] for windows that already
/// include the run under test as their last element. Used by
/// `fermut trend` (which only ever looks at on-disk state).
pub fn branch_scoped_regression(entries: &[HistoryEntry]) -> Option<f64> {
    if entries.len() < 2 {
        return None;
    }
    let (prior, tail) = entries.split_at(entries.len() - 1);
    regression_against(prior, &tail[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::CURRENT_SCHEMA_V;

    fn mk(score: f64, branch: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            schema_version: CURRENT_SCHEMA_V,
            timestamp: "2026-06-04T12:00:00Z".into(),
            mutation_score: score,
            // Non-zero so the entry is a real scored run, not a scoreless
            // vacuous-100 that trend/regression now filter out.
            killed: 1,
            survived: 0,
            timed_out: 0,
            skipped: 0,
            errored: 0,
            equivalent: 0,
            total: None,
            duration_ms: None,
            config_hash: None,
            fermut_version: None,
            git_sha: None,
            git_branch: branch.map(str::to_string),
            survivor_ids: None,
            baseline: false,
            partial: false,
        }
    }

    fn mk_with_survivors(score: f64, ids: Option<&[&str]>) -> HistoryEntry {
        let mut e = mk(score, None);
        e.survivor_ids = ids.map(|s| s.iter().map(|x| x.to_string()).collect());
        e
    }

    /// A scored `mk` entry forced back to zero buckets — its `mutation_score`
    /// is the vacuous 100.0 floor, so `is_scoreless()` is true.
    fn mk_scoreless(branch: Option<&str>) -> HistoryEntry {
        let mut e = mk(100.0, branch);
        e.killed = 0;
        assert!(e.is_scoreless());
        e
    }

    /// A real scored entry flagged `partial` — as a `--max-time` run truncated
    /// by the budget would be. Scored (not scoreless), but not comparable.
    fn mk_partial(score: f64, branch: Option<&str>) -> HistoryEntry {
        let mut e = mk(score, branch);
        e.partial = true;
        assert!(
            !e.is_scoreless(),
            "partial entry still scored a real subset"
        );
        assert!(
            !e.is_comparable(),
            "partial entry must be excluded from the gate"
        );
        e
    }

    fn mk_surv(ids: &[&str]) -> HistoryEntry {
        let mut e = mk(0.0, None);
        e.survivor_ids = Some(ids.iter().map(|s| s.to_string()).collect());
        e
    }

    #[test]
    fn branch_scoped_regression_skips_other_branches() {
        let es = vec![
            mk(90.0, Some("main")),
            mk(50.0, Some("feature/x")), // big drop on feature, ignored
            mk(88.0, Some("main")),      // 2pt drop on main → reported
        ];
        assert_eq!(branch_scoped_regression(&es), Some(2.0));
    }

    #[test]
    fn branch_scoped_regression_none_when_improving() {
        let es = vec![mk(80.0, Some("main")), mk(85.0, Some("main"))];
        assert_eq!(branch_scoped_regression(&es), None);
    }

    #[test]
    fn branch_scoped_regression_none_when_no_prior_on_branch() {
        let es = vec![mk(50.0, Some("other")), mk(90.0, Some("main"))];
        // No prior 'main' entry; falls back to the only prior (other) and
        // 50→90 is an improvement, not a regression.
        assert_eq!(branch_scoped_regression(&es), None);
    }

    #[test]
    fn survivor_diff_finds_new_and_killed() {
        let prev = mk_with_survivors(90.0, Some(&["a", "b", "c"]));
        let curr = mk_with_survivors(85.0, Some(&["b", "c", "d", "e"]));
        let (new_surv, new_killed) = survivor_diff(&prev, &curr).unwrap();
        assert_eq!(new_surv, vec!["d", "e"]);
        assert_eq!(new_killed, vec!["a"]);
    }

    #[test]
    fn survivor_diff_returns_none_when_either_lacks_data() {
        let with = mk_with_survivors(90.0, Some(&["a"]));
        let without = mk_with_survivors(85.0, None);
        assert!(survivor_diff(&with, &without).is_none());
        assert!(survivor_diff(&without, &with).is_none());
    }

    #[test]
    fn survivor_diff_handles_empty_sets() {
        let a = mk_with_survivors(100.0, Some(&[]));
        let b = mk_with_survivors(100.0, Some(&[]));
        let (n, k) = survivor_diff(&a, &b).unwrap();
        assert!(n.is_empty() && k.is_empty());
    }

    #[test]
    fn branch_scoped_regression_best_effort_when_branch_missing() {
        let es = vec![mk(90.0, None), mk(70.0, Some("main"))];
        assert_eq!(branch_scoped_regression(&es), Some(20.0));
    }

    #[test]
    fn regression_against_compares_in_memory_current() {
        // Simulates an append-failed run: history file contains only the
        // prior entries, current is in memory. Gate must compare current
        // against the matching-branch prior, not against the prior's prior.
        let prior = vec![
            mk(90.0, Some("main")), // older
            mk(85.0, Some("main")), // latest matching-branch prior
        ];
        let current = mk(75.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(10.0));
    }

    #[test]
    fn regression_against_returns_none_when_improving() {
        let prior = vec![mk(80.0, Some("main"))];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_other_branch_priors() {
        let prior = vec![
            mk(90.0, Some("main")),      // last matching-branch prior
            mk(50.0, Some("feature/x")), // ignored
        ];
        let current = mk(88.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(2.0));
    }

    #[test]
    fn regression_against_returns_none_when_prior_empty() {
        let current = mk(80.0, Some("main"));
        assert_eq!(regression_against(&[], &current), None);
    }

    #[test]
    fn regression_against_ignores_scoreless_current() {
        // Current run scored nothing (all errored/skipped) → vacuous 100.0.
        // Comparing a real 90% prior against it would fabricate a -10pt drop.
        let prior = vec![mk(90.0, Some("main"))];
        let current = mk_scoreless(Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_scoreless_prior() {
        // The most recent prior scored nothing (vacuous 100.0). It must be
        // skipped so the gate compares against the last real score (90→85).
        let prior = vec![
            mk(90.0, Some("main")),     // last real prior
            mk_scoreless(Some("main")), // vacuous 100.0, must be skipped
        ];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(5.0));
    }

    #[test]
    fn trailing_streak_ignores_scoreless_entries() {
        // A vacuous-100 run sitting between real scores must not invent a move.
        let es = vec![
            mk(50.0, None),
            mk(60.0, None),
            mk_scoreless(None), // vacuous 100.0 — dropped, not a +40 spike
            mk(70.0, None),
        ];
        assert_eq!(trailing_streak(&es), Some((StreakDir::Up, 2)));
    }

    #[test]
    fn trailing_streak_ignores_partial_entries() {
        // A `--max-time` partial run's subset score must not invent or break a
        // streak. 50→60→70 is a clean 2-run up streak; a partial 20 wedged in
        // would otherwise read as a down-then-up whipsaw.
        let es = vec![
            mk(50.0, None),
            mk(60.0, None),
            mk_partial(20.0, None), // subset score — dropped, not a −40 move
            mk(70.0, None),
        ];
        assert_eq!(trailing_streak(&es), Some((StreakDir::Up, 2)));
    }

    #[test]
    fn regression_against_ignores_partial_current() {
        // Current run was --max-time-truncated → scored a nondeterministic
        // subset. Comparing a real 90% prior against it would fabricate a drop.
        let prior = vec![mk(90.0, Some("main"))];
        let current = mk_partial(70.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), None);
    }

    #[test]
    fn regression_against_skips_partial_prior() {
        // The most recent prior was a partial (budget-truncated) run. It must
        // be skipped so the gate compares against the last full run (90→85),
        // never poisoning the baseline with a subset score.
        let prior = vec![
            mk(90.0, Some("main")),         // last full prior
            mk_partial(50.0, Some("main")), // budget-truncated, must be skipped
        ];
        let current = mk(85.0, Some("main"));
        assert_eq!(regression_against(&prior, &current), Some(5.0));
    }

    #[test]
    fn parse_id_file_extracts_prefix() {
        assert_eq!(parse_id_file("src/foo.py@42:a->b"), Some("src/foo.py"));
        assert_eq!(
            parse_id_file("pkg/sub/mod.py@1234:return->pass"),
            Some("pkg/sub/mod.py")
        );
    }

    #[test]
    fn parse_id_file_handles_at_in_path() {
        // POSIX paths can technically contain `@`. The rightmost valid
        // `@<digits>:` boundary wins.
        assert_eq!(
            parse_id_file("weird@dir/m.py@7:x->y"),
            Some("weird@dir/m.py")
        );
    }

    #[test]
    fn parse_id_file_rejects_unrecognised_shape() {
        assert_eq!(parse_id_file("no-at-here"), None);
        assert_eq!(parse_id_file("file.py@notdigits:foo"), None);
        assert_eq!(parse_id_file("file.py@42-missing-colon"), None);
    }

    #[test]
    fn survivors_by_file_groups_and_sorts_by_count() {
        let ids = vec![
            "src/a.py@1:x->y".to_string(),
            "src/b.py@2:x->y".to_string(),
            "src/a.py@3:x->y".to_string(),
            "src/a.py@4:x->y".to_string(),
            "garbage".to_string(),
        ];
        let groups = survivors_by_file(&ids);
        // a.py (3) before b.py (1) before <unknown> (1, alphabetically later).
        assert_eq!(groups[0].0, "src/a.py");
        assert_eq!(groups[0].1.len(), 3);
        assert_eq!(groups[1].0, "<unknown>");
        assert_eq!(groups[2].0, "src/b.py");
    }

    #[test]
    fn survivors_by_file_empty_input_empty_output() {
        assert!(survivors_by_file(&[]).is_empty());
    }

    #[test]
    fn survivor_age_counts_consecutive_appearances() {
        let entries = vec![
            mk_surv(&["a", "b"]), // age would be 3 for a, but b was killed in r2
            mk_surv(&["a"]),      // b not here
            mk_surv(&["a", "c"]), // newest
        ];
        let ages = survivor_age_map(&entries);
        assert_eq!(ages.get("a").copied(), Some(3));
        assert_eq!(ages.get("c").copied(), Some(1));
        // b is not in the latest run — not tracked.
        assert!(!ages.contains_key("b"));
    }

    #[test]
    fn survivor_age_streak_breaks_on_missing_data() {
        let entries = vec![
            mk_surv(&["a"]),
            {
                let mut e = mk(0.0, None);
                e.survivor_ids = None; // older entry, pre-field
                e
            },
            mk_surv(&["a"]),
        ];
        let ages = survivor_age_map(&entries);
        // Latest run has "a" → age starts at 1. Walking back, we hit a
        // missing-ids entry; can't claim "a" survived there, streak stops.
        assert_eq!(ages.get("a").copied(), Some(1));
    }

    #[test]
    fn survivor_age_empty_inputs_safe() {
        assert!(survivor_age_map(&[]).is_empty());
        let no_ids = mk(0.0, None);
        assert!(survivor_age_map(&[no_ids]).is_empty());
        let empty = mk_surv(&[]);
        assert!(survivor_age_map(&[empty]).is_empty());
    }

    #[test]
    fn survivor_age_single_entry_gives_age_one() {
        let entries = vec![mk_surv(&["x", "y"])];
        let ages = survivor_age_map(&entries);
        assert_eq!(ages.get("x").copied(), Some(1));
        assert_eq!(ages.get("y").copied(), Some(1));
    }
}
