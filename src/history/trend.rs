//! Analysis over the history log: per-run deltas, survivor diffs/ages,
//! streaks, and regression gates. Pure functions over `&[HistoryEntry]`.

use super::schema::HistoryEntry;

/// One step of a trend/dashboard delta column: given the running baseline
/// `prev` (the last comparable score), return `(delta_to_show, next_prev)` for
/// entry `e`. A non-comparable run — a scoreless vacuous-100 or a `--max-time`
/// partial subset score — shows no delta and does **not** advance the baseline,
/// so the next comparable row still deltas against the last real full run.
/// Shared by `trend` (stdout table) and `dashboard` (HTML table) so both agree.
pub(crate) fn trend_step(prev: Option<f64>, e: &HistoryEntry) -> (Option<f64>, Option<f64>) {
    if e.is_comparable() {
        (prev.map(|p| e.mutation_score - p), Some(e.mutation_score))
    } else {
        (None, prev)
    }
}

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
pub(crate) fn parse_id_file(id: &str) -> Option<&str> {
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
