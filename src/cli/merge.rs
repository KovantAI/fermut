//! `fermut merge` — combine multiple JSON shard reports into one, and the
//! shared `i/n` shard-spec parser used by both the CLI flag and the config
//! file value.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::filter::shard::SHARD_FILTER_NAME;
use crate::report::{MutantOutcome, ProjectSummary, Report, ReportFormat, ReportSinks, Shard};

/// A `Skipped { filter: "shard" }` outcome is not a verdict — it is the
/// placeholder a sharded run emits for every mutant *outside* its own slice.
/// Each shard's report describes the whole universe (its slice with real
/// verdicts, the rest as this placeholder), so merging must let any real
/// verdict win over it rather than last-write-wins. The filter name is the
/// shared `SHARD_FILTER_NAME` const so this cannot drift from `ShardFilter`.
fn is_shard_placeholder(o: &MutantOutcome) -> bool {
    matches!(o, MutantOutcome::Skipped { filter, .. } if filter == SHARD_FILTER_NAME)
}

/// Do two real (non-placeholder) outcomes for the same mutant id carry the same
/// verdict? Compares only the outcome payload, never the `Mutant` itself: a
/// report written by an older fermut may carry a defaulted `stmt_line`, and that
/// skew must not read as a conflict when the verdict is genuinely identical.
/// Still distinguishes same-status skips/errors/equivalents that differ in their
/// reason — the disagreement finding this guards against.
fn same_verdict(a: &MutantOutcome, b: &MutantOutcome) -> bool {
    use MutantOutcome::*;
    match (a, b) {
        (Killed { .. }, Killed { .. })
        | (Survived { .. }, Survived { .. })
        | (TimedOut { .. }, TimedOut { .. }) => true,
        (Skipped { filter: x, .. }, Skipped { filter: y, .. }) => x == y,
        (Error { message: x, .. }, Error { message: y, .. }) => x == y,
        (
            Equivalent {
                reason: r1,
                source: s1,
                ..
            },
            Equivalent {
                reason: r2,
                source: s2,
                ..
            },
        ) => r1 == r2 && s1 == s2,
        _ => false,
    }
}

pub(super) fn parse_shard_spec(s: &str) -> Result<(u32, u32), String> {
    let (i, n) = s
        .split_once('/')
        .ok_or_else(|| format!("expected `i/n`, got {s:?}"))?;
    let i: u32 = i
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    let n: u32 = n
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    if n == 0 {
        return Err("shard total must be >= 1".into());
    }
    if i < 1 || i > n {
        return Err(format!("shard index {i} out of range 1..={n}"));
    }
    Ok((i, n))
}

/// Checks the shard specs the input reports recorded: every shard comes from
/// the same `n`, and every index `1..=n` is present. `--expect-shards N` makes
/// it strict: every input must record a shard, `n` must be `N`, and no index
/// may repeat.
#[derive(Default)]
struct ShardCheck {
    expect: Option<u32>,
    total: Option<u32>,
    seen: BTreeSet<u32>,
    untagged: usize,
}

impl ShardCheck {
    fn add(&mut self, path: &Path, shard: Option<Shard>) -> Result<()> {
        let Some(Shard { index, total }) = shard else {
            if self.expect.is_some() {
                bail!(
                    "{} records no shard spec; --expect-shards needs every input to come \
                     from `fermut run --shard i/n` with a fermut that records it",
                    path.display()
                );
            }
            self.untagged += 1;
            return Ok(());
        };
        if let Some(n) = self.expect.filter(|&n| n != total) {
            bail!(
                "{} is shard {index}/{total}, but --expect-shards is {n}",
                path.display()
            );
        }
        match self.total {
            Some(t) if t != total => bail!(
                "{} is shard {index}/{total}, but an earlier input is from a {t}-shard run. \
                 Reports from different shard counts don't partition the same way.",
                path.display()
            ),
            _ => self.total = Some(total),
        }
        if !self.seen.insert(index) && self.expect.is_some() {
            bail!(
                "shard {index}/{total} appears more than once ({})",
                path.display()
            );
        }
        Ok(())
    }

    /// Fails on a missing index. Skipped when an input recorded no spec: it
    /// may be the shard that looks missing.
    fn finish(&self) -> Result<()> {
        let Some(total) = self.total.filter(|_| self.untagged == 0) else {
            return Ok(());
        };
        let missing: Vec<String> = (1..=total)
            .filter(|i| !self.seen.contains(i))
            .map(|i| format!("{i}/{total}"))
            .collect();
        if !missing.is_empty() {
            bail!(
                "missing shard report(s): {}. The merged score would cover only part \
                 of the mutant universe.",
                missing.join(", ")
            );
        }
        Ok(())
    }
}

/// Gates on the merged report, beyond the shard checks.
#[derive(Default)]
pub(super) struct MergeGates {
    /// `--expect-shards`: the exact number of shards the inputs must cover.
    pub expect_shards: Option<u32>,
    /// `--max-errored`: fail when more mutants than this errored.
    pub max_errored: Option<usize>,
}

pub(super) fn merge_reports(
    inputs: &[PathBuf],
    sinks: &ReportSinks,
    history: Option<&PathBuf>,
    config_hash: Option<String>,
    project_root: &Path,
    gates: &MergeGates,
) -> Result<()> {
    let merged = match group_by_project(inputs)? {
        None => merge_group(inputs, gates.expect_shards)?,
        Some(groups) => aggregate(groups, gates.expect_shards)?,
    };
    let config_hash = config_hash.or_else(|| merged.config_hash.clone());

    sinks.write_all(&merged)?;
    if !sinks.any() && history.is_none() {
        // Nothing requested → print JSON to stdout so the command is useful by default.
        merged.print(ReportFormat::Json);
    }

    // Gates run after the reports are written, so a failed merge still leaves
    // them to inspect, but before the history entry, so it's never recorded.
    let errored = merged.counts().errored;
    if let Some(max) = gates.max_errored.filter(|&max| errored > max) {
        bail!(
            "{errored} mutant(s) errored (--max-errored {max}): the merged score is untrustworthy"
        );
    }
    if history.is_some() && merged.is_scoreless() {
        bail!(
            "no mutant was scored, so the score is N/A, not 100%. Refusing to write a \
             history entry that would record a vacuous 100% in the trend."
        );
    }

    // Emit a complete history entry from the merged report. This is what lets a
    // sharded CI run record its trend point WITHOUT harvesting one shard's
    // history line as a template: git sha/branch come from the merge checkout
    // (the same commit the shards ran on), `fermut_version` is stamped
    // automatically, and the counts are the merged full-universe totals.
    // `config_hash` is the one the shards agree on (see `shared_config_hash`)
    // unless the caller overrides it with `--config-hash`.
    if let Some(p) = history {
        let entry = crate::history::HistoryEntry::from_report(
            &merged,
            project_root,
            merged.duration_ms,
            config_hash,
        );
        let line = serde_json::to_string(&entry).context("serializing merged history entry")?;
        // Single staged entry, overwrite (not append): re-running merge must not
        // stack duplicate lines in the staging file.
        std::fs::write(p, format!("{line}\n"))
            .with_context(|| format!("writing {}", p.display()))?;
    }
    Ok(())
}

/// Merges one shard set into a report: shard checks, verdict combine, the
/// shared `config_hash`, summed duration.
fn merge_group(inputs: &[PathBuf], expect_shards: Option<u32>) -> Result<Report> {
    let mut shards = ShardCheck {
        expect: expect_shards,
        ..ShardCheck::default()
    };
    // Read + parse lazily so `combine_outcomes` holds only one report's
    // outcomes plus the running map at a time. A sharded report is the whole
    // mutant universe, so eagerly collecting all N would be an N× blow-up.
    let mut hashes = Vec::with_capacity(inputs.len());
    let mut durations = Vec::with_capacity(inputs.len());
    let reports = inputs.iter().map(|path| -> Result<Vec<MutantOutcome>> {
        let report = crate::report::load(path)?;
        shards.add(path, report.shard)?;
        hashes.push(report.config_hash);
        durations.push(report.duration_ms);
        Ok(report.outcomes)
    });
    let outcomes = combine_outcomes(reports)?;
    shards.finish()?;
    let mut merged = Report::new(outcomes);
    merged.config_hash = shared_config_hash(hashes)?;
    // Total work across shards. `None` if any shard predates the field, rather
    // than a sum that silently undercounts.
    merged.duration_ms = durations.into_iter().sum();
    Ok(merged)
}

/// `(project dir, its input reports)`, in first-seen order.
type Groups = Vec<(String, Vec<PathBuf>)>;

/// Splits `DIR=PATH` inputs into per-project groups, in first-seen order.
/// `None` when no input has a prefix: a plain shard merge.
fn group_by_project(inputs: &[PathBuf]) -> Result<Option<Groups>> {
    let split: Vec<_> = inputs
        .iter()
        .map(|p| p.to_str().and_then(|s| s.split_once('=')))
        .collect();
    if split.iter().all(Option::is_none) {
        return Ok(None);
    }
    let mut groups: Groups = Vec::new();
    for (input, parts) in inputs.iter().zip(split) {
        let Some((dir, path)) = parts else {
            bail!(
                "{} has no `DIR=` prefix, but other inputs do. Prefix every input to \
                 aggregate projects.",
                input.display()
            );
        };
        let dir = dir.trim_end_matches('/');
        let p = Path::new(dir);
        if dir.is_empty() || p.has_root() || p.components().any(|c| c == Component::ParentDir) {
            bail!("project dir {dir:?} must be a non-empty repo-relative path without `..`");
        }
        match groups.iter_mut().find(|(d, _)| d == dir) {
            Some((_, paths)) => paths.push(path.into()),
            None => groups.push((dir.to_string(), vec![path.into()])),
        }
    }
    for (a, _) in &groups {
        if let Some((b, _)) = groups
            .iter()
            .find(|(b, _)| b != a && Path::new(b).starts_with(a))
        {
            bail!("project {b} is nested inside project {a}: their mutants would count twice");
        }
    }
    Ok(Some(groups))
}

/// Pools per-project merges into one report. Each project merges on its own
/// (so shard checks and the config agreement stay per project), then its
/// mutants are rebased under `DIR/` so ids from different projects never
/// collide. The pooled counts are the scored-weighted score.
fn aggregate(groups: Groups, expect_shards: Option<u32>) -> Result<Report> {
    let mut outcomes = Vec::new();
    let mut projects = Vec::with_capacity(groups.len());
    let mut durations = Vec::with_capacity(groups.len());
    for (dir, paths) in groups {
        let report =
            merge_group(&paths, expect_shards).with_context(|| format!("project {dir}"))?;
        let counts = report.counts();
        // The vacuous-100% bug one level down: a scoreless project would vanish
        // inside the pooled score.
        if counts.scored() == 0 {
            bail!("project {dir} scored no mutant: its score is N/A, not 100%");
        }
        for mut o in report.outcomes {
            let m = Arc::make_mut(o.mutant_mut());
            let file = repo_relative(&m.file, &dir).with_context(|| {
                format!(
                    "project {dir}: mutant file {} is not under it",
                    m.file.display()
                )
            })?;
            // Ids start with the file path; swap it so ids match the file.
            let old = m.file.display().to_string();
            m.id = match m.id.strip_prefix(&old) {
                // `/` on every OS: ids are compared across platforms.
                Some(rest) => format!("{}{rest}", file.display().to_string().replace('\\', "/")),
                None => format!("{dir}/{}", m.id),
            };
            m.file = file;
            outcomes.push(o);
        }
        durations.push(report.duration_ms);
        projects.push(ProjectSummary {
            dir,
            config_hash: report.config_hash,
            counts,
        });
    }
    projects.sort_by(|a, b| {
        a.counts
            .mutation_score()
            .total_cmp(&b.counts.mutation_score())
            .then_with(|| a.dir.cmp(&b.dir))
    });
    let mut merged = Report::new(outcomes);
    merged.config_hash = aggregate_hash(&projects);
    merged.duration_ms = durations.into_iter().sum();
    merged.projects = projects;
    Ok(merged)
}

/// `file` re-rooted at the repo, starting at project `dir`. Mutant paths are
/// absolute (`/ci/checkout/packages/a/src/x.py`), so the repo root is where
/// `dir`'s components first appear; a relative path is project-relative.
/// `None` when an absolute path doesn't contain `dir`.
fn repo_relative(file: &Path, dir: &str) -> Option<PathBuf> {
    // `has_root`, not `is_absolute`: `/ci/x.py` has no drive on Windows.
    if !file.has_root() {
        return Some(Path::new(dir).join(file));
    }
    let parts: Vec<Component> = file.components().collect();
    let want: Vec<Component> = Path::new(dir).components().collect();
    let i = parts.windows(want.len()).position(|w| w == want)?;
    Some(parts[i..].iter().collect())
}

/// Hash over the sorted `(dir, config_hash)` pairs: changes when any project's
/// config changes or a project is added or removed, so the aggregate trend
/// re-baselines instead of reading a different universe as a regression.
/// `None` when any project predates `config_hash`.
fn aggregate_hash(projects: &[ProjectSummary]) -> Option<String> {
    let mut pairs = projects
        .iter()
        .map(|p| Some((p.dir.as_str(), p.config_hash.as_deref()?)))
        .collect::<Option<Vec<_>>>()?;
    pairs.sort_unstable();
    let mut h = Sha256::new();
    h.update(b"agg-v1");
    for (dir, hash) in pairs {
        // NUL can't appear in a command-line path: unambiguous separator.
        h.update(format!("\0{dir}\0{hash}"));
    }
    Some(hex::encode(h.finalize()))
}

/// The `config_hash` every input report carries, ignoring `--shard` (see
/// [`Report::config_hash`]). Shards that disagree ran different configs, so
/// their union is not one run and merging it would record a meaningless score.
/// `None` when any input predates the field.
fn shared_config_hash(hashes: Vec<Option<String>>) -> Result<Option<String>> {
    let Some(hashes) = hashes.into_iter().collect::<Option<Vec<_>>>() else {
        return Ok(None);
    };
    if let Some(other) = hashes.iter().find(|h| **h != hashes[0]) {
        bail!(
            "input reports were produced with different configs (config_hash {} vs {other}). \
             Every shard must run with the same flags and fermut.toml; re-run the odd one out.",
            hashes[0]
        );
    }
    Ok(hashes.into_iter().next())
}

/// Combine per-report outcome lists into one, in first-seen id order.
///
/// The rule that fixes sharded merges: a real verdict always wins over a
/// `Skipped { filter: "shard" }` placeholder, regardless of input order. Two
/// *different* real outcomes for one id are a conflict (disjoint shards cannot
/// produce it) and error out; identical ones are an idempotent re-merge. Any
/// placeholder still standing at the end means a shard is missing, which also
/// errors — scoring a partial universe as if whole is the failure mode this
/// guards against.
///
/// This is stricter than the old last-write-wins merge on purpose: overlapping
/// or non-shard inputs whose verdicts disagree can no longer be silently
/// collapsed — they must be resolved by the caller.
///
/// Takes a fallible iterator so callers can read+parse each report lazily; only
/// one report's outcomes are live at a time alongside the running map.
fn combine_outcomes(
    reports: impl IntoIterator<Item = Result<Vec<MutantOutcome>>>,
) -> Result<Vec<MutantOutcome>> {
    let mut by_id: HashMap<String, MutantOutcome> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut report_count = 0usize;
    for outcomes in reports {
        let outcomes = outcomes?;
        report_count += 1;
        for outcome in outcomes {
            let Some(existing) = by_id.get_mut(&outcome.mutant().id) else {
                // First sighting of this id — record it and its order slot.
                let id = outcome.mutant().id.clone();
                order.push(id.clone());
                by_id.insert(id, outcome);
                continue;
            };
            match (
                is_shard_placeholder(existing),
                is_shard_placeholder(&outcome),
            ) {
                // Incoming placeholder must never overwrite a verdict a prior
                // shard already produced — this is the bug.
                (false, true) => {}
                // Upgrade a placeholder to the owning shard's verdict.
                (true, false) => *existing = outcome,
                // Both placeholders — keep the first, still a placeholder.
                (true, true) => {}
                // Two real outcomes for one id: the same verdict is an
                // idempotent re-merge; a differing verdict — including same
                // status with a different skip filter or error message — is a
                // genuine conflict and must not be silently resolved.
                (false, false) => {
                    if !same_verdict(existing, &outcome) {
                        bail!(
                            "conflicting verdicts for mutant {:?}: {} vs {}. \
                             Shard reports must cover disjoint mutant sets; overlapping or \
                             non-shard inputs cannot be merged unambiguously. If every input \
                             is a different shard of one `fermut run --shard i/n` config, \
                             fermut split them wrongly: please report it at {}",
                            outcome.mutant().id,
                            existing.status_label(),
                            outcome.status_label(),
                            crate::ISSUES_URL,
                        );
                    }
                }
            }
        }
    }

    let outcomes: Vec<MutantOutcome> = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();

    // A surviving placeholder means no input ever supplied a real verdict for
    // that mutant — a shard is missing. Folds the union-completeness check the
    // CI reimplements in jq into `fermut merge` itself.
    let residual = outcomes.iter().filter(|o| is_shard_placeholder(o)).count();
    if residual > 0 {
        bail!(
            "{residual} mutant(s) remain shard-skipped after merging {report_count} report(s): no \
             input supplied a real verdict for them, so a shard is missing. The merged score would \
             cover only part of the mutant universe. Include every shard's report and re-run. \
             If every shard's report is already included, fermut split them wrongly: please \
             report it at {}",
            crate::ISSUES_URL
        );
    }

    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{Mutant, Operator};
    use crate::report::{MutantOutcome, Report};
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    fn mutant(id: &str) -> Mutant {
        Mutant {
            id: id.into(),
            file: PathBuf::from("test.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 1,
            stmt_line: 1,
            site: None,
        }
    }

    #[test]
    fn merge_history_entry_carries_version_hash_and_merged_counts() {
        let tmp = tempfile::tempdir().unwrap();
        // Two shard reports: 2 killed + 1 survived across the union.
        let r1 = Report::new(vec![
            MutantOutcome::killed(mutant("a")),
            MutantOutcome::survived(mutant("b")),
        ]);
        let r2 = Report::new(vec![MutantOutcome::killed(mutant("c"))]);
        let p1 = tmp.path().join("s1.json");
        let p2 = tmp.path().join("s2.json");
        r1.write_json(&p1).unwrap();
        r2.write_json(&p2).unwrap();

        let combined = tmp.path().join("combined.json");
        let hist = tmp.path().join("entry.json");
        merge_reports(
            &[p1, p2],
            &ReportSinks {
                json: Some(combined),
                ..Default::default()
            },
            Some(&hist),
            Some("deadbeef".into()),
            tmp.path(),
            &MergeGates::default(),
        )
        .unwrap();

        let raw = std::fs::read_to_string(&hist).unwrap();
        let entry: crate::history::HistoryEntry = serde_json::from_str(raw.trim()).unwrap();
        // Auto-stamped by from_report.
        assert_eq!(
            entry.fermut_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        // Passed through (merge can't derive it).
        assert_eq!(entry.config_hash.as_deref(), Some("deadbeef"));
        // Counts are the merged full-universe totals, not one shard's.
        assert_eq!(entry.killed, 2);
        assert_eq!(entry.survived, 1);
        assert_eq!(entry.total, Some(3));
    }

    fn shard(index: u32, total: u32) -> Option<Shard> {
        Some(Shard { index, total })
    }

    fn check(expect: Option<u32>, inputs: &[Option<Shard>]) -> Result<()> {
        let mut c = ShardCheck {
            expect,
            ..ShardCheck::default()
        };
        for s in inputs {
            c.add(Path::new("r.json"), *s)?;
        }
        c.finish()
    }

    #[test]
    fn shard_check_accepts_a_complete_set_and_unsharded_inputs() {
        check(None, &[shard(2, 2), shard(1, 2)]).unwrap();
        check(Some(2), &[shard(2, 2), shard(1, 2)]).unwrap();
        check(None, &[None, None]).unwrap();
        // Re-merging the same shard is idempotent without --expect-shards.
        check(None, &[shard(1, 2), shard(1, 2), shard(2, 2)]).unwrap();
    }

    #[test]
    fn shard_check_rejects_missing_mixed_and_duplicate_shards() {
        let err = |e: Option<u32>, i: &[Option<Shard>]| check(e, i).unwrap_err().to_string();
        assert!(err(None, &[shard(1, 3), shard(3, 3)]).contains("2/3"));
        assert!(err(None, &[shard(1, 16), shard(2, 8)]).contains("16-shard"));
        assert!(err(Some(2), &[shard(1, 2), shard(1, 2)]).contains("more than once"));
        assert!(err(Some(4), &[shard(1, 2), shard(2, 2)]).contains("--expect-shards is 4"));
        assert!(err(Some(1), &[None]).contains("no shard spec"));
        assert!(err(Some(3), &[shard(1, 3), shard(2, 3)]).contains("3/3"));
    }

    #[test]
    fn untagged_input_suspends_the_missing_index_check() {
        // An older report records no spec; it may be the shard that looks missing.
        check(None, &[shard(1, 2), None]).unwrap();
    }

    fn merge_one(report: Report, history: bool, gates: &MergeGates) -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();
        let p = tmp.join("s.json");
        report.write_json(&p).unwrap();
        let hist = tmp.join("entry.json");
        merge_reports(
            &[p],
            &ReportSinks {
                json: Some(tmp.join("c.json")),
                ..Default::default()
            },
            history.then_some(&hist),
            None,
            tmp,
            gates,
        )
        .map(|()| dir)
    }

    #[test]
    fn max_errored_fails_after_writing_reports_but_before_history() {
        let r = Report::new(vec![
            MutantOutcome::killed(mutant("a")),
            MutantOutcome::error(mutant("b"), "boom".into()),
        ]);
        let gates = MergeGates {
            max_errored: Some(0),
            ..MergeGates::default()
        };
        let err = merge_one(r.clone(), true, &gates).unwrap_err();
        assert!(err.to_string().contains("1 mutant(s) errored"), "{err}");
        let ok = MergeGates {
            max_errored: Some(1),
            ..MergeGates::default()
        };
        let dir = merge_one(r, true, &ok).unwrap();
        assert!(dir.path().join("entry.json").exists());
    }

    #[test]
    fn scoreless_merge_writes_no_history_entry() {
        let r = Report::new(vec![MutantOutcome::skipped(mutant("a"), "ty")]);
        let err = merge_one(r.clone(), true, &MergeGates::default()).unwrap_err();
        assert!(err.to_string().contains("N/A"), "{err}");
        // Without --history a scoreless merge is fine: it's just a report.
        merge_one(r, false, &MergeGates::default()).unwrap();
    }

    #[test]
    fn sharded_report_round_trips_its_shard_spec() {
        let mut r = Report::new(vec![MutantOutcome::killed(mutant("a"))]);
        r.shard = shard(2, 4);
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.json");
        r.write_json(&p).unwrap();
        assert_eq!(crate::report::load(&p).unwrap().shard, shard(2, 4));
    }

    #[test]
    fn merge_history_overwrites_rather_than_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Report::new(vec![MutantOutcome::killed(mutant("a"))]);
        let p = tmp.path().join("s.json");
        r.write_json(&p).unwrap();
        let hist = tmp.path().join("entry.json");
        for _ in 0..2 {
            merge_reports(
                std::slice::from_ref(&p),
                &ReportSinks {
                    json: Some(tmp.path().join("c.json")),
                    ..Default::default()
                },
                Some(&hist),
                None,
                tmp.path(),
                &MergeGates::default(),
            )
            .unwrap();
        }
        // Re-running merge leaves exactly one staged line, not two.
        let raw = std::fs::read_to_string(&hist).unwrap();
        assert_eq!(raw.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    /// Writes one report per hash, merges them with `--history`, and returns
    /// the merge result plus the staged entry's `config_hash`.
    fn merge_hashed(hashes: &[Option<&str>], explicit: Option<&str>) -> Result<Option<String>> {
        let tmp = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = hashes
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let mut r = Report::new(vec![MutantOutcome::killed(mutant(&format!("m{i}")))]);
                r.config_hash = h.map(str::to_string);
                let p = tmp.path().join(format!("s{i}.json"));
                r.write_json(&p).unwrap();
                p
            })
            .collect();
        let hist = tmp.path().join("entry.json");
        merge_reports(
            &paths,
            &ReportSinks {
                json: Some(tmp.path().join("c.json")),
                ..Default::default()
            },
            Some(&hist),
            explicit.map(str::to_string),
            tmp.path(),
            &MergeGates::default(),
        )?;
        let raw = std::fs::read_to_string(&hist).unwrap();
        let entry: crate::history::HistoryEntry = serde_json::from_str(raw.trim()).unwrap();
        Ok(entry.config_hash)
    }

    /// Merges one report per duration with `--history` and returns the staged
    /// entry's `duration_ms`.
    fn merge_durations(durations: &[Option<u64>]) -> Option<u64> {
        let tmp = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = durations
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let mut r = Report::new(vec![MutantOutcome::killed(mutant(&format!("m{i}")))]);
                r.duration_ms = *d;
                let p = tmp.path().join(format!("s{i}.json"));
                r.write_json(&p).unwrap();
                p
            })
            .collect();
        let hist = tmp.path().join("entry.json");
        merge_reports(
            &paths,
            &ReportSinks::default(),
            Some(&hist),
            None,
            tmp.path(),
            &MergeGates::default(),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&hist).unwrap();
        let entry: crate::history::HistoryEntry = serde_json::from_str(raw.trim()).unwrap();
        entry.duration_ms
    }

    #[test]
    fn merge_sums_shard_durations() {
        assert_eq!(merge_durations(&[Some(1000), Some(250)]), Some(1250));
    }

    #[test]
    fn merge_leaves_duration_unset_when_a_shard_predates_it() {
        assert_eq!(merge_durations(&[Some(1000), None]), None);
    }

    #[test]
    fn merge_stamps_the_hash_the_shards_agree_on() {
        let got = merge_hashed(&[Some("abc"), Some("abc")], None).unwrap();
        assert_eq!(got.as_deref(), Some("abc"));
    }

    #[test]
    fn merge_refuses_shards_with_different_configs() {
        let err = merge_hashed(&[Some("abc"), Some("xyz")], None).unwrap_err();
        assert!(err.to_string().contains("different configs"), "{err}");
    }

    #[test]
    fn merge_explicit_config_hash_overrides_the_shards() {
        let got = merge_hashed(&[Some("abc"), Some("abc")], Some("override")).unwrap();
        assert_eq!(got.as_deref(), Some("override"));
    }

    #[test]
    fn merge_leaves_hash_unset_when_a_shard_predates_it() {
        let got = merge_hashed(&[Some("abc"), None], None).unwrap();
        assert_eq!(got, None);
    }

    fn shard_skip(id: &str) -> MutantOutcome {
        MutantOutcome::skipped(mutant(id), "shard")
    }

    /// One shard's report over a 3-mutant universe: real verdict for the id it
    /// owns, `shard` placeholders for the rest.
    fn shard_report(owned: &str, outcome: MutantOutcome, universe: &[&str]) -> Vec<MutantOutcome> {
        universe
            .iter()
            .map(|&id| {
                if id == owned {
                    outcome.clone()
                } else {
                    shard_skip(id)
                }
            })
            .collect()
    }

    fn labels(outcomes: &[MutantOutcome]) -> Vec<(&str, &str)> {
        outcomes
            .iter()
            .map(|o| (o.mutant().id.as_str(), o.status_label()))
            .collect()
    }

    #[test]
    fn real_verdicts_survive_shard_placeholders_regardless_of_order() {
        let universe = ["a", "b", "c"];
        // Three shards, each owning one mutant. Placeholders for "a" and "c"
        // appear AFTER their real verdicts here — the exact last-write-wins
        // collapse the old HashMap insert produced.
        let reports = vec![
            shard_report("a", MutantOutcome::killed(mutant("a")), &universe),
            shard_report("b", MutantOutcome::survived(mutant("b")), &universe),
            shard_report("c", MutantOutcome::timed_out(mutant("c")), &universe),
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(
            labels(&merged),
            vec![("a", "killed"), ("b", "survived"), ("c", "timeout")]
        );
    }

    #[test]
    fn missing_shard_leaves_a_placeholder_and_errors() {
        let universe = ["a", "b", "c"];
        // Only two of three shards supplied — "c" is never given a real verdict.
        let reports = vec![
            shard_report("a", MutantOutcome::killed(mutant("a")), &universe),
            shard_report("b", MutantOutcome::survived(mutant("b")), &universe),
        ];
        let err = combine_outcomes(reports.into_iter().map(Ok))
            .unwrap_err()
            .to_string();
        assert!(err.contains("shard is missing"), "got: {err}");
    }

    #[test]
    fn conflicting_real_verdicts_error() {
        let reports = vec![
            vec![MutantOutcome::killed(mutant("a"))],
            vec![MutantOutcome::survived(mutant("a"))],
        ];
        let err = combine_outcomes(reports.into_iter().map(Ok))
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflicting verdicts"), "got: {err}");
    }

    #[test]
    fn identical_verdicts_are_idempotent() {
        let reports = vec![
            vec![MutantOutcome::killed(mutant("a"))],
            vec![MutantOutcome::killed(mutant("a"))],
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(labels(&merged), vec![("a", "killed")]);
    }

    #[test]
    fn non_shard_skips_are_preserved_not_treated_as_placeholders() {
        // A coverage-skip is a real decision by the owning shard and must
        // survive, unlike a shard placeholder.
        let universe = ["a", "b"];
        let reports = vec![
            shard_report(
                "a",
                MutantOutcome::skipped(mutant("a"), "coverage"),
                &universe,
            ),
            shard_report("b", MutantOutcome::killed(mutant("b")), &universe),
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(labels(&merged), vec![("a", "skipped"), ("b", "killed")]);
    }

    #[test]
    fn same_status_different_detail_conflicts() {
        // Two real skips for one id with different filters share the "skipped"
        // label. A label-only conflict check would treat them as idempotent and
        // silently drop one; full-outcome comparison flags the disagreement.
        let reports = vec![
            vec![MutantOutcome::skipped(mutant("a"), "coverage")],
            vec![MutantOutcome::skipped(mutant("a"), "boundary")],
        ];
        let err = combine_outcomes(reports.into_iter().map(Ok))
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflicting verdicts"), "got: {err}");
    }

    /// Writes `reports` (project dir, report) and runs an aggregating merge;
    /// returns the merged JSON report.
    fn aggregate_merge(reports: Vec<(&str, Report)>) -> Result<Report> {
        let tmp = tempfile::tempdir().unwrap();
        let inputs: Vec<PathBuf> = reports
            .into_iter()
            .enumerate()
            .map(|(i, (dir, r))| {
                let p = tmp.path().join(format!("r{i}.json"));
                r.write_json(&p).unwrap();
                PathBuf::from(format!("{dir}={}", p.display()))
            })
            .collect();
        let out = tmp.path().join("agg.json");
        merge_reports(
            &inputs,
            &ReportSinks {
                json: Some(out.clone()),
                ..Default::default()
            },
            None,
            None,
            tmp.path(),
            &MergeGates::default(),
        )?;
        Ok(crate::report::load(&out).unwrap())
    }

    fn verdicts(killed: usize, survived: usize) -> Report {
        let k = (0..killed).map(|i| MutantOutcome::killed(mutant(&format!("k{i}"))));
        let s = (0..survived).map(|i| MutantOutcome::survived(mutant(&format!("s{i}"))));
        Report::new(k.chain(s).collect())
    }

    #[test]
    fn colliding_ids_across_projects_are_rebased_not_deduped() {
        // Same id in both projects, different verdicts: a plain merge would
        // bail on the conflict (or dedup identical ones).
        let agg = aggregate_merge(vec![
            (
                "packages/a",
                Report::new(vec![MutantOutcome::killed(mutant("x"))]),
            ),
            (
                "packages/b",
                Report::new(vec![MutantOutcome::survived(mutant("x"))]),
            ),
        ])
        .unwrap();
        assert_eq!(
            labels(&agg.outcomes),
            vec![("packages/a/x", "killed"), ("packages/b/x", "survived")]
        );
        assert_eq!(
            agg.outcomes[0].mutant().file,
            Path::new("packages/a/test.py")
        );
    }

    #[test]
    fn absolute_mutant_paths_are_rerooted_at_the_project_dir() {
        let mut m = mutant("/ci/repo/packages/a/src/x.py@3:op:+->-");
        m.file = PathBuf::from("/ci/repo/packages/a/src/x.py");
        let agg = aggregate_merge(vec![(
            "packages/a",
            Report::new(vec![MutantOutcome::killed(m.clone())]),
        )])
        .unwrap();
        let got = agg.outcomes[0].mutant();
        assert_eq!(got.file, Path::new("packages/a/src/x.py"));
        assert_eq!(got.id, "packages/a/src/x.py@3:op:+->-");

        let err = aggregate_merge(vec![(
            "packages/b",
            Report::new(vec![MutantOutcome::killed(m)]),
        )])
        .unwrap_err();
        assert!(err.to_string().contains("is not under it"), "{err}");
    }

    #[test]
    fn pooled_score_is_scored_weighted_with_worst_project_first() {
        // 9/10 and 1/90: pooled 10/100 = 10%, not the 50% mean.
        let agg = aggregate_merge(vec![("a", verdicts(9, 1)), ("b", verdicts(1, 89))]).unwrap();
        assert_eq!(agg.counts().mutation_score_opt(), Some(10.0));
        let dirs: Vec<&str> = agg.projects.iter().map(|p| p.dir.as_str()).collect();
        assert_eq!(dirs, ["b", "a"]);
        assert_eq!(agg.projects[1].counts.killed, 9);
    }

    #[test]
    fn missing_shard_in_one_project_names_it() {
        let mut s1 = verdicts(1, 0);
        s1.shard = shard(1, 2);
        let err = aggregate_merge(vec![("a", verdicts(1, 0)), ("b", s1)]).unwrap_err();
        assert!(format!("{err:#}").contains("project b"), "{err:#}");
        assert!(format!("{err:#}").contains("2/2"), "{err:#}");
    }

    #[test]
    fn bad_project_dirs_and_mixed_inputs_are_rejected() {
        for dirs in [["/abs", "b"], ["../up", "b"], ["a", "a/sub"], ["", "b"]] {
            let err = aggregate_merge(vec![(dirs[0], verdicts(1, 0)), (dirs[1], verdicts(1, 0))])
                .unwrap_err();
            assert!(err.to_string().contains("project"), "{dirs:?}: {err}");
        }
        let err =
            group_by_project(&[PathBuf::from("a=x.json"), PathBuf::from("y.json")]).unwrap_err();
        assert!(err.to_string().contains("no `DIR=` prefix"), "{err}");
        // Component-wise nesting: `a/bc` is not inside `a/b`.
        group_by_project(&[PathBuf::from("a/b=x"), PathBuf::from("a/bc=y")]).unwrap();
        // Shards of one project (trailing slash or not) land in one group.
        let groups = group_by_project(&[
            PathBuf::from("a=s1.json"),
            PathBuf::from("b=s.json"),
            PathBuf::from("a/=s2.json"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            groups,
            [
                (
                    "a".into(),
                    vec![PathBuf::from("s1.json"), PathBuf::from("s2.json")]
                ),
                ("b".into(), vec![PathBuf::from("s.json")]),
            ]
        );
    }

    #[test]
    fn scoreless_project_fails_the_aggregate() {
        let none = Report::new(vec![MutantOutcome::skipped(mutant("a"), "ty")]);
        let err = aggregate_merge(vec![("a", verdicts(5, 0)), ("b", none)]).unwrap_err();
        assert!(
            err.to_string().contains("project b scored no mutant"),
            "{err}"
        );
    }

    #[test]
    fn aggregate_hash_is_order_free_and_tracks_each_project() {
        let p = |dir: &str, h: Option<&str>| ProjectSummary {
            dir: dir.into(),
            config_hash: h.map(str::to_string),
            counts: Default::default(),
        };
        let ab = aggregate_hash(&[p("a", Some("1")), p("b", Some("2"))]).unwrap();
        assert_eq!(
            aggregate_hash(&[p("b", Some("2")), p("a", Some("1"))]),
            Some(ab.clone())
        );
        assert_ne!(
            aggregate_hash(&[p("a", Some("1")), p("b", Some("3"))]),
            Some(ab.clone())
        );
        assert_ne!(aggregate_hash(&[p("a", Some("1"))]), Some(ab));
        assert_eq!(aggregate_hash(&[p("a", Some("1")), p("b", None)]), None);
    }

    #[test]
    fn plain_merge_writes_no_projects_key() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s.json");
        verdicts(1, 0).write_json(&p).unwrap();
        let out = tmp.path().join("c.json");
        merge_reports(
            &[p],
            &ReportSinks {
                json: Some(out.clone()),
                ..Default::default()
            },
            None,
            None,
            tmp.path(),
            &MergeGates::default(),
        )
        .unwrap();
        assert!(!std::fs::read_to_string(out).unwrap().contains("projects"));
    }

    #[test]
    fn same_verdict_with_mutant_field_skew_is_idempotent() {
        // Same killed verdict for one id, but the reports disagree on a `Mutant`
        // field (an older report defaults `stmt_line` to 0). Payload-only
        // comparison must treat this as an idempotent re-merge, not a conflict.
        let mut old = mutant("a");
        old.stmt_line = 0;
        let mut new = mutant("a");
        new.stmt_line = 5;
        let reports = vec![
            vec![MutantOutcome::killed(old)],
            vec![MutantOutcome::killed(new)],
        ];
        let merged = combine_outcomes(reports.into_iter().map(Ok)).unwrap();
        assert_eq!(labels(&merged), vec![("a", "killed")]);
    }

    #[test]
    fn same_verdict_compares_payloads_per_status() {
        use MutantOutcome as O;
        let m = || mutant("a");
        assert!(same_verdict(&O::survived(m()), &O::survived(m())));
        assert!(same_verdict(&O::timed_out(m()), &O::timed_out(m())));
        assert!(same_verdict(&O::skipped(m(), "ty"), &O::skipped(m(), "ty")));
        assert!(!same_verdict(
            &O::skipped(m(), "ty"),
            &O::skipped(m(), "ruff")
        ));
        assert!(same_verdict(
            &O::error(m(), "boom".into()),
            &O::error(m(), "boom".into())
        ));
        assert!(!same_verdict(
            &O::error(m(), "boom".into()),
            &O::error(m(), "bang".into())
        ));
        let eq = |r: &str, s: &str| O::equivalent(m(), r, s);
        assert!(same_verdict(&eq("r", "s"), &eq("r", "s")));
        assert!(!same_verdict(&eq("r", "s"), &eq("r2", "s")));
        assert!(!same_verdict(&eq("r", "s"), &eq("r", "s2")));
        assert!(!same_verdict(&O::killed(m()), &O::survived(m())));
    }

    #[test]
    fn parse_shard_spec_accepts_valid_and_rejects_bad_specs() {
        assert_eq!(parse_shard_spec("1/1"), Ok((1, 1)));
        assert_eq!(parse_shard_spec("3/4"), Ok((3, 4)));
        assert!(parse_shard_spec("3")
            .unwrap_err()
            .contains("expected `i/n`"));
        assert!(parse_shard_spec("x/4").is_err());
        assert!(parse_shard_spec("1/y").is_err());
        assert_eq!(
            parse_shard_spec("1/0").unwrap_err(),
            "shard total must be >= 1"
        );
        assert!(parse_shard_spec("0/4")
            .unwrap_err()
            .contains("out of range"));
        assert!(parse_shard_spec("5/4")
            .unwrap_err()
            .contains("out of range"));
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct MergeArgs {
    /// Input JSON reports to combine. At least one required. Prefix each
    /// with `DIR=` (repo-relative project root) to aggregate several
    /// projects into one pooled report with a per-project breakdown.
    #[arg(required = true, num_args = 1..)]
    pub(crate) inputs: Vec<std::path::PathBuf>,

    /// Write the combined JSON report here. Otherwise prints to stdout.
    #[arg(long)]
    pub(crate) json: Option<std::path::PathBuf>,

    /// Also write a JUnit XML report.
    #[arg(long)]
    pub(crate) junit: Option<std::path::PathBuf>,

    /// Also write an HTML report.
    #[arg(long)]
    pub(crate) html: Option<std::path::PathBuf>,

    /// Also write a Markdown report.
    #[arg(long)]
    pub(crate) markdown: Option<std::path::PathBuf>,

    /// Write a single history entry built from the merged report to this
    /// path (overwriting). Git sha/branch are read from the merge checkout
    /// and the fermut version is stamped automatically; the counts are the
    /// merged full-universe totals. Lets a sharded run record its trend
    /// point without harvesting a shard's history line as a template.
    #[arg(long, value_name = "PATH")]
    pub(crate) history: Option<std::path::PathBuf>,

    /// `config_hash` to stamp into the `--history` entry. Merge can't derive
    /// it (the run config lives in the shard jobs), so pass the value the
    /// shards recorded, e.g. `$(jq -r .config_hash shard-1-entry.json)`.
    #[arg(long, value_name = "HEX")]
    pub(crate) config_hash: Option<String>,

    /// Project root for git sha/branch discovery in the `--history` entry.
    /// Defaults to the current directory (the merge checkout).
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub(crate) project: std::path::PathBuf,

    /// Require exactly N shards: every input must record its shard (from
    /// `fermut run --shard i/n`), all with total N, each index once. Without
    /// it, merge still refuses mixed shard totals and a missing index.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) expect_shards: Option<u32>,

    /// Fail when more than N mutants errored in the merged report. The
    /// reports are still written; the `--history` entry is not.
    #[arg(long, value_name = "N")]
    pub(crate) max_errored: Option<usize>,
}

/// Dispatch handler: nests the output-path flags into a [`ReportSinks`] and
/// runs the shard merge.
pub(crate) fn run(args: MergeArgs) -> Result<()> {
    let MergeArgs {
        inputs,
        json,
        junit,
        html,
        markdown,
        history,
        config_hash,
        project,
        expect_shards,
        max_errored,
    } = args;
    merge_reports(
        &inputs,
        &ReportSinks {
            json,
            junit,
            html,
            markdown,
        },
        history.as_ref(),
        config_hash,
        &project,
        &MergeGates {
            expect_shards,
            max_errored,
        },
    )
}
