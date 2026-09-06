//! Mutation report types and outputs.
//!
//! - `MutantOutcome` / `Counts` / `Report` (this file) — core data types.
//! - `writers` — one writer per output format (json, junit, html, markdown).
//! - `annotations` — GitHub Actions workflow-command emission.
//! - `diff` — unified-diff regeneration for survivors (used by `show` and HTML).

pub mod annotations;
pub mod diff;
pub mod writers;

use serde::{Deserialize, Serialize, Serializer};

use crate::mutator::Mutant;

pub use diff::unified_diff_for;

#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum ReportFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MutantOutcome {
    Killed {
        mutant: Mutant,
    },
    Survived {
        mutant: Mutant,
    },
    TimedOut {
        mutant: Mutant,
    },
    Skipped {
        mutant: Mutant,
        filter: String,
    },
    Error {
        mutant: Mutant,
        message: String,
    },
    /// Equivalent-mutant detector proved the mutation is a no-op. Excluded
    /// from the score denominator like `Skipped`. `source` identifies which
    /// detector layer found it (e.g. `"bytecode-identity"`, `"arith-zero"`).
    Equivalent {
        mutant: Mutant,
        reason: String,
        source: String,
    },
}

impl MutantOutcome {
    pub fn killed(m: Mutant) -> Self {
        Self::Killed { mutant: m }
    }
    pub fn survived(m: Mutant) -> Self {
        Self::Survived { mutant: m }
    }
    pub fn timed_out(m: Mutant) -> Self {
        Self::TimedOut { mutant: m }
    }
    pub fn skipped(m: Mutant, filter: impl Into<String>) -> Self {
        Self::Skipped {
            mutant: m,
            filter: filter.into(),
        }
    }
    pub fn error(m: Mutant, message: String) -> Self {
        Self::Error { mutant: m, message }
    }
    pub fn equivalent(m: Mutant, reason: impl Into<String>, source: impl Into<String>) -> Self {
        Self::Equivalent {
            mutant: m,
            reason: reason.into(),
            source: source.into(),
        }
    }

    pub fn mutant(&self) -> &Mutant {
        match self {
            Self::Killed { mutant }
            | Self::Survived { mutant }
            | Self::TimedOut { mutant }
            | Self::Skipped { mutant, .. }
            | Self::Error { mutant, .. }
            | Self::Equivalent { mutant, .. } => mutant,
        }
    }

    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Killed { .. } => "killed",
            Self::Survived { .. } => "survived",
            Self::TimedOut { .. } => "timeout",
            Self::Skipped { .. } => "skipped",
            Self::Error { .. } => "error",
            Self::Equivalent { .. } => "equivalent",
        }
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    pub killed: usize,
    pub survived: usize,
    pub timed_out: usize,
    pub skipped: usize,
    pub errored: usize,
    #[serde(default)]
    pub equivalent: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.killed + self.survived + self.timed_out + self.skipped + self.errored + self.equivalent
    }

    /// Denominator of the mutation score: mutants that produced a real verdict
    /// (`killed`, `timed_out`, `survived`). Skipped, errored and equivalent
    /// mutants are excluded. Zero means the score is *undefined*, not perfect.
    pub fn scored(&self) -> usize {
        self.killed + self.timed_out + self.survived
    }

    /// Mutation score in percent, or `None` when nothing was scored. A zero
    /// denominator makes the ratio undefined — an all-errored or all-skipped
    /// run has no score, which is not the same as 100%. Prefer this wherever
    /// "nothing to score" must be told apart from a genuine perfect run (gates,
    /// N/A reporting); use [`mutation_score`](Self::mutation_score) only where a
    /// bare `f64` is required and the empty-denominator floor is acceptable.
    pub fn mutation_score_opt(&self) -> Option<f64> {
        let scored = self.scored();
        if scored == 0 {
            return None;
        }
        let detected = self.killed + self.timed_out;
        Some(100.0 * (detected as f64) / (scored as f64))
    }

    /// Percent score with the historical empty-denominator floor of `100.0`.
    /// Retained for the JSON summary and history schema, which carry a bare
    /// `f64`. New gate/reporting logic should use
    /// [`mutation_score_opt`](Self::mutation_score_opt) instead.
    pub fn mutation_score(&self) -> f64 {
        self.mutation_score_opt().unwrap_or(100.0)
    }

    /// Human display of the score: a one-decimal percent, or `N/A` when
    /// nothing was scored (a zero denominator is undefined, not 100%). Use
    /// this for any surface a person reads (`show`, HTML/Markdown reports); the
    /// bare floored [`mutation_score`](Self::mutation_score) stays only for the
    /// JSON/history schema.
    pub fn score_label(&self) -> String {
        match self.mutation_score_opt() {
            Some(s) => format!("{s:.1}%"),
            None => "N/A".to_string(),
        }
    }
}

/// Top-level run summary, emitted as the `summary` object in JSON output so
/// an agent or CI step can read the headline numbers without re-deriving them
/// from `outcomes`. `mutation_score` is rounded to one decimal to match the
/// human summary line (`score: 86.4%`). Skipped and equivalent mutants are
/// excluded from the score denominator (see [`Counts::mutation_score`]).
///
/// `mutation_score` carries the floored `100.0` when nothing was scored, so a
/// consumer must read `scored` to tell a genuine perfect run from a vacuous
/// N/A: `scored == 0` means the score is undefined, not 100% (see
/// [`Counts::mutation_score_opt`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub total: usize,
    pub killed: usize,
    pub survived: usize,
    pub timed_out: usize,
    pub skipped: usize,
    pub equivalent: usize,
    pub errored: usize,
    pub mutation_score: f64,
    /// Denominator of `mutation_score` — mutants with a real verdict
    /// (`killed + timed_out + survived`). `0` means `mutation_score` is a
    /// vacuous floor, not a genuine 100%. Optional for back-compat with reports
    /// written before this field existed.
    #[serde(default)]
    pub scored: usize,
}

/// Per-operator verdict breakdown for one run. Only real verdicts are counted;
/// skipped/errored/equivalent mutants say nothing about an operator's value.
/// Used to surface "noise" operators whose survivors swamp their kills —
/// candidates for `skip_ops`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorStat {
    pub operator: &'static str,
    pub killed: usize,
    pub survived: usize,
    pub timed_out: usize,
}

impl OperatorStat {
    /// Mutants that produced a real verdict for this operator.
    pub fn scored(&self) -> usize {
        self.killed + self.survived + self.timed_out
    }

    /// Detected mutants (killed or timed out — both count as caught).
    pub fn detected(&self) -> usize {
        self.killed + self.timed_out
    }

    /// True when survivors meet or exceed detections and there is at least one
    /// survivor — the operator produced at least as much noise as signal this
    /// run. Mirrors the `survived >= killed` rule projects hand-tune into
    /// `skip_ops` (e.g. `constant-replace`, `keyword-arg-drop`).
    pub fn is_noisy(&self) -> bool {
        self.survived > 0 && self.survived >= self.detected()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Report {
    pub outcomes: Vec<MutantOutcome>,
}

// Hand-written so JSON output carries a derived `summary` object alongside
// `outcomes`. `Report` stays `{ outcomes }` in memory — the summary is
// computed at serialize time, never stored. The derived `Deserialize` ignores
// the extra `summary` key, so a written report round-trips back to a `Report`.
impl Serialize for Report {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a> {
            summary: Summary,
            outcomes: &'a [MutantOutcome],
        }
        View {
            summary: self.summary(),
            outcomes: &self.outcomes,
        }
        .serialize(serializer)
    }
}

impl Report {
    pub fn new(outcomes: Vec<MutantOutcome>) -> Self {
        Self { outcomes }
    }

    /// Headline counts + score for this run. Mirrors the human summary line.
    pub fn summary(&self) -> Summary {
        let c = self.counts();
        Summary {
            total: c.total(),
            killed: c.killed,
            survived: c.survived,
            timed_out: c.timed_out,
            skipped: c.skipped,
            equivalent: c.equivalent,
            errored: c.errored,
            mutation_score: round1(c.mutation_score()),
            scored: c.scored(),
        }
    }

    pub fn has_survivors(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o, MutantOutcome::Survived { .. }))
    }

    /// True when the run scored no mutant — the denominator is zero, so the
    /// mutation score is undefined (reported N/A) rather than 100%. Callers gate
    /// and display this case explicitly instead of trusting a floored percent.
    pub fn is_scoreless(&self) -> bool {
        self.counts().scored() == 0
    }

    /// Decide whether the run should exit non-zero.
    ///
    /// With `fail_under` set, round the mutation score to the precision used
    /// by human output (one decimal place) before comparing against the raw
    /// threshold, so a printed `80.0%` never fails against a `--fail-under 80`
    /// due to f64 rounding while sub-decimal thresholds stay honoured exactly.
    /// Without it, any survivor fails the run.
    ///
    /// A zero denominator is *not* a passing 100%. When nothing was scored the
    /// run fails if any mutant errored — an all-errored run otherwise collapses
    /// to a vacuous 100% and slips through — regardless of whether a threshold
    /// was set. It passes only when there was genuinely nothing to score and
    /// nothing errored (e.g. a diff touching no mutable lines), which is a
    /// legitimate N/A rather than a fake perfect score. When there *is* a real
    /// score, a threshold gate compares against it and the default gate fails on
    /// any survivor.
    pub fn should_fail(&self, fail_under: Option<f64>) -> bool {
        let c = self.counts();
        match c.mutation_score_opt() {
            Some(score) => match fail_under {
                Some(threshold) => round1(score) < threshold,
                None => self.has_survivors(),
            },
            // Scoreless: fail iff something errored, in both gate modes.
            None => c.errored > 0,
        }
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for o in &self.outcomes {
            match o {
                MutantOutcome::Killed { .. } => c.killed += 1,
                MutantOutcome::Survived { .. } => c.survived += 1,
                MutantOutcome::TimedOut { .. } => c.timed_out += 1,
                MutantOutcome::Skipped { .. } => c.skipped += 1,
                MutantOutcome::Error { .. } => c.errored += 1,
                MutantOutcome::Equivalent { .. } => c.equivalent += 1,
            }
        }
        c
    }

    /// Skipped-mutant counts grouped by the filter that dropped each one
    /// (`operator`, `parity`, `experimental`, `ty`, `coverage`, ...), sorted
    /// by count descending then name. Lets the summary explain *why* a large
    /// `skipped` total isn't mutants silently going untested.
    pub fn skipped_by_filter(&self) -> Vec<(String, usize)> {
        let mut map: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for o in &self.outcomes {
            if let MutantOutcome::Skipped { filter, .. } = o {
                *map.entry(filter.as_str()).or_default() += 1;
            }
        }
        let mut v: Vec<(String, usize)> =
            map.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Per-operator verdict breakdown, noisiest (most survivors) first. Only
    /// operators that produced at least one real verdict appear. Lets a caller
    /// (or the human summary) surface `is_noisy` operators as `skip_ops`
    /// candidates without re-scanning `outcomes`.
    pub fn operator_stats(&self) -> Vec<OperatorStat> {
        let mut map: std::collections::BTreeMap<&'static str, OperatorStat> =
            std::collections::BTreeMap::new();
        for o in &self.outcomes {
            let name = o.mutant().operator.name();
            let e = map.entry(name).or_insert(OperatorStat {
                operator: name,
                killed: 0,
                survived: 0,
                timed_out: 0,
            });
            match o {
                MutantOutcome::Killed { .. } => e.killed += 1,
                MutantOutcome::Survived { .. } => e.survived += 1,
                MutantOutcome::TimedOut { .. } => e.timed_out += 1,
                // Skipped/Error/Equivalent carry no operator-value signal.
                _ => {}
            }
        }
        let mut v: Vec<OperatorStat> = map.into_values().filter(|s| s.scored() > 0).collect();
        v.sort_by(|a, b| {
            b.survived
                .cmp(&a.survived)
                .then_with(|| a.operator.cmp(b.operator))
        });
        v
    }

    pub fn print(&self, fmt: ReportFormat) {
        match fmt {
            ReportFormat::Json => {
                let s = serde_json::to_string_pretty(self).expect("serialize report");
                println!("{s}");
            }
            ReportFormat::Human => self.print_human(),
        }
    }

    fn print_human(&self) {
        for o in &self.outcomes {
            match o {
                MutantOutcome::Killed { .. } => {}
                MutantOutcome::Survived { mutant } => {
                    println!("SURVIVED  {}", mutant.describe());
                }
                MutantOutcome::TimedOut { mutant } => {
                    println!("TIMEOUT   {}", mutant.describe());
                }
                MutantOutcome::Skipped { .. } => {}
                MutantOutcome::Error { mutant, message } => {
                    println!("ERROR     {} :: {message}", mutant.describe());
                }
                MutantOutcome::Equivalent {
                    mutant,
                    reason,
                    source,
                } => {
                    println!("EQUIV     {} [{source}] {reason}", mutant.describe());
                }
            }
        }
        let c = self.counts();
        // Spell out the skip breakdown — a large `skipped` total is almost
        // always operator/parity/experimental filtering, not mutants quietly
        // escaping coverage. Showing the reasons inline pre-empts the "is it
        // silently not testing things?" reaction.
        let skipped = {
            let detail = self.skipped_by_filter();
            if detail.is_empty() {
                c.skipped.to_string()
            } else {
                let parts: Vec<String> = detail.iter().map(|(k, n)| format!("{k} {n}")).collect();
                format!("{} ({})", c.skipped, parts.join(", "))
            }
        };
        // `score_label`, not the floored `mutation_score`: an all-skipped run
        // (e.g. a `--max-time` budget that expired before any mutant ran) has a
        // zero denominator and reads `N/A`, never a vacuous `100.0%`.
        println!(
            "\n{} mutants — killed: {}, survived: {}, timeout: {}, skipped: {}, equivalent: {}, errored: {}  | score: {}",
            c.total(),
            c.killed,
            c.survived,
            c.timed_out,
            skipped,
            c.equivalent,
            c.errored,
            c.score_label()
        );
        // Flag operators that produced at least as many survivors as kills:
        // they cost test time for little signal, and are the usual `skip_ops`
        // candidates. Only shown when there is something to act on.
        let noisy: Vec<&str> = self
            .operator_stats()
            .iter()
            .filter(|s| s.is_noisy())
            .map(|s| s.operator)
            .collect();
        if !noisy.is_empty() {
            println!(
                "noisy operators (survivors ≥ kills): {} — add to `skip_ops` if their \
                 survivors aren't observably wrong",
                noisy.join(", ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::Operator;
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    #[test]
    fn killed_and_timed_out_score_identically() {
        // The invariant that makes smart ordering verdict-neutral even under a
        // per-mutant timeout (finding #1): `timed_out` and `killed` are both
        // *detected*, so moving a mutant between the two leaves the score and
        // its denominator untouched. Reordering can only ever flip these two
        // (never produce/remove a `survived`), so it can't move the score.
        let with_kill = Counts {
            killed: 3,
            timed_out: 0,
            survived: 2,
            ..Default::default()
        };
        // Same set, but one detection landed as a timeout instead of a kill —
        // exactly what reordering the selected tests under a timeout can do.
        let with_timeout = Counts {
            killed: 2,
            timed_out: 1,
            survived: 2,
            ..Default::default()
        };
        assert_eq!(with_kill.scored(), with_timeout.scored());
        assert_eq!(
            with_kill.mutation_score_opt(),
            with_timeout.mutation_score_opt(),
            "shifting a detection killed↔timed_out must not move the score"
        );
    }

    #[test]
    fn operator_stats_ranks_and_flags_noisy_operators() {
        fn m(op: Operator) -> Mutant {
            Mutant {
                id: "x".into(),
                file: PathBuf::from("t.py"),
                operator: op,
                range: TextRange::new(0u32.into(), 1u32.into()),
                original: "+".into(),
                replacement: "-".into(),
                line: 1,
                stmt_line: 1,
            }
        }
        let r = Report::new(vec![
            MutantOutcome::killed(m(Operator::ArithOpSwap)), // 2 killed, 0 surv → signal
            MutantOutcome::killed(m(Operator::ArithOpSwap)),
            MutantOutcome::survived(m(Operator::ConstantReplace)), // 0 killed, 2 surv → noise
            MutantOutcome::survived(m(Operator::ConstantReplace)),
            MutantOutcome::killed(m(Operator::CompareOpSwap)), // 1 killed, 1 surv → noise (>=)
            MutantOutcome::survived(m(Operator::CompareOpSwap)),
            MutantOutcome::skipped(m(Operator::BoundaryShift), "ty"), // no verdict → excluded
        ]);
        let stats = r.operator_stats();
        let names: Vec<&str> = stats.iter().map(|s| s.operator).collect();
        // Sorted by survivors desc; boundary-shift excluded (only skipped).
        assert_eq!(
            names,
            vec!["constant-replace", "compare-op-swap", "arith-op-swap"]
        );
        let noisy: Vec<&str> = stats
            .iter()
            .filter(|s| s.is_noisy())
            .map(|s| s.operator)
            .collect();
        assert_eq!(noisy, vec!["constant-replace", "compare-op-swap"]);
        let arith = stats
            .iter()
            .find(|s| s.operator == "arith-op-swap")
            .unwrap();
        assert!(
            !arith.is_noisy(),
            "an all-killed operator is signal, not noise"
        );
        assert_eq!(arith.killed, 2);
    }

    fn make_mutant() -> Mutant {
        Mutant {
            id: "id-1".into(),
            file: PathBuf::from("test.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 2,
            stmt_line: 2,
        }
    }

    fn report(outcomes: Vec<MutantOutcome>) -> Report {
        Report::new(outcomes)
    }

    #[test]
    fn counts_aggregate_correctly() {
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
            MutantOutcome::timed_out(make_mutant()),
            MutantOutcome::skipped(make_mutant(), "ty"),
            MutantOutcome::error(make_mutant(), "boom".into()),
        ]);
        let c = r.counts();
        assert_eq!(c.killed, 2);
        assert_eq!(c.survived, 1);
        assert_eq!(c.timed_out, 1);
        assert_eq!(c.skipped, 1);
        assert_eq!(c.errored, 1);
        assert_eq!(c.total(), 6);
    }

    #[test]
    fn mutation_score_excludes_skipped_and_errored() {
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
            MutantOutcome::skipped(make_mutant(), "ty"),
            MutantOutcome::error(make_mutant(), "x".into()),
        ]);
        assert!((r.counts().mutation_score() - 75.0).abs() < 0.001);
    }

    #[test]
    fn mutation_score_is_100_when_nothing_to_score() {
        let r = report(vec![MutantOutcome::skipped(make_mutant(), "ty")]);
        assert!((r.counts().mutation_score() - 100.0).abs() < 0.001);
    }

    #[test]
    fn timeouts_count_as_kills_in_score() {
        let r = report(vec![
            MutantOutcome::timed_out(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert!((r.counts().mutation_score() - 50.0).abs() < 0.001);
    }

    #[test]
    fn has_survivors_reflects_survived_only() {
        let with_survivor = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert!(with_survivor.has_survivors());

        let without = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::timed_out(make_mutant()),
            MutantOutcome::skipped(make_mutant(), "ty"),
        ]);
        assert!(!without.has_survivors());
    }

    #[test]
    fn should_fail_default_fails_on_any_survivor() {
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert!(r.should_fail(None));
    }

    #[test]
    fn should_fail_default_passes_with_no_survivors() {
        let r = report(vec![MutantOutcome::killed(make_mutant())]);
        assert!(!r.should_fail(None));
    }

    #[test]
    fn should_fail_passes_when_score_meets_threshold() {
        // 2 killed, 1 survived → 66.67%
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert!(!r.should_fail(Some(60.0)));
    }

    #[test]
    fn should_fail_fails_when_score_below_threshold() {
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert!(r.should_fail(Some(80.0)));
    }

    #[test]
    fn mutation_score_opt_is_none_when_nothing_scored() {
        // Only skipped/errored mutants — zero denominator.
        let r = report(vec![
            MutantOutcome::skipped(make_mutant(), "coverage"),
            MutantOutcome::error(make_mutant(), "boom".into()),
        ]);
        assert!(r.counts().mutation_score_opt().is_none());
        // The floored accessor still returns the historical 100.0.
        assert!((r.counts().mutation_score() - 100.0).abs() < f64::EPSILON);
        assert!(r.is_scoreless());
    }

    #[test]
    fn all_time_budget_skipped_run_labels_na_not_vacuous_hundred() {
        // A --max-time budget that expired before any mutant ran: every mutant
        // is a time-budget skip, zero denominator. The human summary uses
        // `score_label`, so it must read N/A, never a vacuous 100.0%.
        let r = report(vec![
            MutantOutcome::skipped(make_mutant(), "time-budget"),
            MutantOutcome::skipped(make_mutant(), "time-budget"),
        ]);
        assert!(r.is_scoreless());
        assert_eq!(r.counts().score_label(), "N/A");
    }

    #[test]
    fn score_label_is_na_when_scoreless_else_percent() {
        let scoreless = report(vec![MutantOutcome::error(make_mutant(), "boom".into())]);
        assert_eq!(scoreless.counts().score_label(), "N/A");
        let real = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert_eq!(real.counts().score_label(), "50.0%");
    }

    #[test]
    fn should_fail_fails_vacuous_hundred_when_all_errored() {
        // The reported bug: every mutant errored, denominator is zero, the
        // floored score is a vacuous 100% that must NOT pass a threshold gate.
        let r = report(vec![
            MutantOutcome::error(make_mutant(), "boom".into()),
            MutantOutcome::error(make_mutant(), "boom".into()),
        ]);
        assert!(r.should_fail(Some(50.0)));
    }

    #[test]
    fn should_fail_default_fails_vacuous_hundred_when_all_errored() {
        // Same vacuous 100%, but no --fail-under: the default gate must also
        // fail rather than exit 0 while claiming the gate failed.
        let r = report(vec![
            MutantOutcome::error(make_mutant(), "boom".into()),
            MutantOutcome::error(make_mutant(), "boom".into()),
        ]);
        assert!(r.should_fail(None));
    }

    #[test]
    fn should_fail_default_passes_when_nothing_to_score_and_no_errors() {
        // No threshold, nothing scored, nothing errored — legitimate N/A pass.
        let r = report(vec![MutantOutcome::skipped(make_mutant(), "coverage")]);
        assert!(!r.should_fail(None));
        assert!(r.is_scoreless());
    }

    #[test]
    fn summary_scored_zero_flags_vacuous_score() {
        // JSON consumers read `scored` to tell a real 100% from an N/A floor.
        let r = report(vec![MutantOutcome::error(make_mutant(), "boom".into())]);
        let s = r.summary();
        assert_eq!(s.scored, 0);
        assert!((s.mutation_score - 100.0).abs() < f64::EPSILON);

        let real = report(vec![MutantOutcome::killed(make_mutant())]);
        assert_eq!(real.summary().scored, 1);
    }

    #[test]
    fn should_fail_passes_when_nothing_to_score_and_no_errors() {
        // Nothing scored and nothing errored — a legitimate N/A (e.g. a diff
        // touching no mutable lines), a pass rather than a fake perfect score.
        let r = report(vec![
            MutantOutcome::skipped(make_mutant(), "coverage"),
            MutantOutcome::skipped(make_mutant(), "shard"),
        ]);
        assert!(!r.should_fail(Some(90.0)));
        assert!(r.is_scoreless());
    }

    #[test]
    fn should_fail_empty_report_passes_threshold_gate() {
        // No mutants at all: N/A, not a failing gate.
        let r = report(vec![]);
        assert!(!r.should_fail(Some(90.0)));
        assert!(r.is_scoreless());
    }

    #[test]
    fn should_fail_ignores_survivors_when_score_above() {
        // 9 killed, 1 survived → 90%, threshold 80 → pass
        let mut outcomes: Vec<MutantOutcome> = (0..9)
            .map(|_| MutantOutcome::killed(make_mutant()))
            .collect();
        outcomes.push(MutantOutcome::survived(make_mutant()));
        let r = report(outcomes);
        assert!(!r.should_fail(Some(80.0)));
    }

    #[test]
    fn should_fail_passes_when_score_equals_threshold() {
        // 1 killed, 1 survived → 50.0%, threshold 50.0 → pass.
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        assert!(!r.should_fail(Some(50.0)));
    }

    #[test]
    fn should_fail_passes_at_boundary_when_score_rounds_to_threshold() {
        // 4 killed, 1 survived → 80.0%, threshold 80.0 → pass at boundary.
        // Guards against f64 drift where the printed score equals the
        // threshold but the raw comparison would fail.
        let mut outcomes: Vec<MutantOutcome> = (0..4)
            .map(|_| MutantOutcome::killed(make_mutant()))
            .collect();
        outcomes.push(MutantOutcome::survived(make_mutant()));
        let r = report(outcomes);
        assert!(!r.should_fail(Some(80.0)));
    }

    #[test]
    fn should_fail_honours_sub_decimal_threshold() {
        // 4 killed, 1 survived → 80.0% exactly. Threshold 80.04 must fail
        // (score strictly below threshold). Previously the threshold was
        // rounded to 80.0 and passed, silently shifting the user's bar by up
        // to 0.05 points.
        let mut outcomes: Vec<MutantOutcome> = (0..4)
            .map(|_| MutantOutcome::killed(make_mutant()))
            .collect();
        outcomes.push(MutantOutcome::survived(make_mutant()));
        let r = report(outcomes);
        assert!(r.should_fail(Some(80.04)));
    }

    #[test]
    fn round1_helper_rounds_to_one_decimal() {
        assert!((round1(79.9999) - 80.0).abs() < f64::EPSILON);
        assert!((round1(80.04) - 80.0).abs() < f64::EPSILON);
        assert!((round1(80.05) - 80.1).abs() < f64::EPSILON);
    }

    #[test]
    fn json_roundtrip() {
        let original = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        let s = serde_json::to_string(&original).unwrap();
        let parsed: Report = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.counts().total(), 2);
        assert!(parsed.has_survivors());
    }

    #[test]
    fn skipped_by_filter_groups_and_sorts_by_count_desc() {
        let r = report(vec![
            MutantOutcome::skipped(make_mutant(), "operator"),
            MutantOutcome::skipped(make_mutant(), "operator"),
            MutantOutcome::skipped(make_mutant(), "operator"),
            MutantOutcome::skipped(make_mutant(), "parity"),
            MutantOutcome::skipped(make_mutant(), "parity"),
            MutantOutcome::skipped(make_mutant(), "experimental"),
            MutantOutcome::killed(make_mutant()),
        ]);
        let by = r.skipped_by_filter();
        assert_eq!(
            by,
            vec![
                ("operator".to_string(), 3),
                ("parity".to_string(), 2),
                ("experimental".to_string(), 1),
            ]
        );
    }

    #[test]
    fn skipped_by_filter_empty_when_nothing_skipped() {
        let r = report(vec![MutantOutcome::killed(make_mutant())]);
        assert!(r.skipped_by_filter().is_empty());
    }

    #[test]
    fn skipped_by_filter_ties_break_by_name() {
        // Equal counts must order deterministically (alphabetical) so the
        // summary line is stable across runs.
        let r = report(vec![
            MutantOutcome::skipped(make_mutant(), "ty"),
            MutantOutcome::skipped(make_mutant(), "coverage"),
        ]);
        let by = r.skipped_by_filter();
        assert_eq!(by[0].0, "coverage");
        assert_eq!(by[1].0, "ty");
    }

    #[test]
    fn json_output_carries_top_level_summary() {
        // 3 killed, 1 survived → 75.0%. An agent must read the score straight
        // from `summary` without counting outcomes itself.
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
            MutantOutcome::skipped(make_mutant(), "ty"),
        ]);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        let summary = &v["summary"];
        assert_eq!(summary["mutation_score"], 75.0);
        assert_eq!(summary["total"], 5);
        assert_eq!(summary["killed"], 3);
        assert_eq!(summary["survived"], 1);
        assert_eq!(summary["skipped"], 1);
        // `outcomes` is still present and unchanged.
        assert_eq!(v["outcomes"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn report_with_summary_deserializes_back() {
        // The written form `{summary, outcomes}` must parse back into a
        // `Report` (summary ignored, outcomes authoritative) — proves callers
        // that re-load `last.json` aren't broken by the new field.
        let r = report(vec![
            MutantOutcome::killed(make_mutant()),
            MutantOutcome::survived(make_mutant()),
        ]);
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            s.contains("\"summary\""),
            "serialized form must include summary"
        );
        let parsed: Report = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.counts().total(), 2);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use crate::mutator::{Mutant, Operator};
    use ruff_text_size::TextRange;
    use std::path::PathBuf;

    /// Test-only helper: a stub `Mutant` used by writer tests.
    pub fn make_mutant() -> Mutant {
        Mutant {
            id: "id-1".into(),
            file: PathBuf::from("test.py"),
            operator: Operator::ArithOpSwap,
            range: TextRange::new(0u32.into(), 1u32.into()),
            original: "+".into(),
            replacement: "-".into(),
            line: 2,
            stmt_line: 2,
        }
    }
}
