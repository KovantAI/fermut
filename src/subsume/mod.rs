//! First-order dominator subsumption over recorded kill-sets (`fermut subsume`).
//!
//! Input is the JSONL a `fermut run --record-kill-sets` writes: one record per
//! mutant with its kill-set `K(m)`, the full set of covering tests that fail on
//! it. Over that fixed, recorded suite, subsumption is decidable (Kurtz, Ammann,
//! Delamaro, Offutt, *Mutant Subsumption Graphs*, ICSTW 2014):
//!
//! - `a` **subsumes** `b` iff `K(a) ⊆ K(b)`: every test that kills `a` also
//!   kills `b`.
//! - Mutants with an identical `K` form an **equivalence class**: no recorded
//!   test tells them apart, so one representative stands for all.
//! - **Dominators** are the ⊆-minimal classes (no other class's kill-set is a
//!   strict subset). A suite that kills every dominator kills every killed
//!   mutant, so testing the dominators certifies the whole killed set.
//!
//! The **dominator score** is `D / (D + S)`: `D` killed dominator classes, `S`
//! survivors. It follows Ammann, Delamaro & Offutt, *Establishing Theoretical
//! Minimal Sets of Mutants* (ICST 2014): redundant, trivially killed mutants
//! inflate the plain score, while each dominator counts once. Survivors have no
//! kill-set (`K = ∅`), so they can't be ordered by containment. In `fermut
//! subsume` each counts as its own class; at report time, survivors another
//! survivor at the same compare / `and`-`or` site subsumes are folded first
//! (see [`crate::report::fold`]), so `S` counts distinct survivor targets.
//! Counting unfolded survivors can only lower the score.
//!
//! Records that carry no usable kill-set are left out of the lattice and counted
//! separately: timeouts and errors, and kills with an empty `K` (a mutant that
//! broke the module import fails collection before any test id is printed).
//! An empty `K` would be a subset of every class and so fake a lone dominator.
//!
//! The derived [`DominatorStore`] (`.fermut/dominators.json`) also records the
//! AST hash of every mutated file. `fermut run` uses it to add a
//! `dominator_score` to the summary only while every file it scores is
//! unchanged since recording (see [`report_dominator_score`]). The gate is per
//! file, not per function: the class map is keyed by mutant id, and ids embed
//! byte offsets, so an edit anywhere above a function already renames its
//! mutants and no finer gate could reuse their classes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::report::MutantOutcome;

/// Schema version of [`DominatorStore`]. Bump on a breaking change; a store
/// with another version is treated as absent.
pub const STORE_VERSION: u32 = 1;

/// Default dominator store location, beside the cache/history/kill-order under
/// `<artifact_root>/.fermut/`.
pub fn default_dominators_path(artifact_root: &Path) -> PathBuf {
    artifact_root.join(".fermut").join("dominators.json")
}

/// The fields of a [`crate::runner::kill_sets::KillSetRecord`] this analysis
/// reads, with owned `status` for deserialization.
#[derive(Debug, Clone, Deserialize)]
pub struct Record {
    pub mutant_id: String,
    pub status: String,
    #[serde(default)]
    pub kill_set: Vec<String>,
}

/// Read a kill-set JSONL. Blank lines are skipped; a malformed line is an
/// error naming its line number, since a silently dropped kill would bend
/// the lattice. A mutant recorded twice keeps its first record.
pub fn load_records(path: &Path) -> Result<Vec<Record>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading kill-sets {}", path.display()))?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let r: Record = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: malformed kill-set record", path.display(), i + 1))?;
        if seen.insert(r.mutant_id.clone()) {
            out.push(r);
        }
    }
    Ok(out)
}

/// Headline numbers of one analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// Distinct mutants in the input.
    pub records: usize,
    /// Killed mutants with a non-empty kill-set: the lattice's population.
    pub killed: usize,
    pub survived: usize,
    /// Killed, but with an empty kill-set (import-breaking mutants). Detected,
    /// yet not placeable in the lattice.
    pub killed_unattributed: usize,
    /// Timed-out and errored mutants. No kill-set, no verdict on subsumption.
    pub other: usize,
    /// Distinct kill-sets among `killed`.
    pub classes: usize,
    /// ⊆-minimal classes among `classes`.
    pub dominators: usize,
    /// Percent of `killed` that need not be tested once the dominators are
    /// killed: `100 · (1 − dominators / killed)`. `0.0` when nothing was killed.
    pub reduction_pct: f64,
    /// `100 · D / (D + S)`, see the module doc. `None` when `D + S == 0`.
    pub dominator_score: Option<f64>,
}

/// One equivalence class of killed mutants (identical kill-sets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Class {
    /// Lexicographically smallest member id, so the choice is deterministic
    /// regardless of the record order the parallel run produced.
    pub rep: String,
    /// All member ids, sorted. Includes `rep`.
    pub members: Vec<String>,
    /// `|K|` shared by every member.
    pub kill_set_size: usize,
    pub dominator: bool,
}

#[derive(Debug, Clone)]
pub struct Analysis {
    pub stats: Stats,
    /// Sorted dominators first, then by ascending kill-set size, then by `rep`.
    pub classes: Vec<Class>,
}

/// A kill-set as a bitset over interned test ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Bits(Vec<u64>);

impl Bits {
    fn with(ids: &[u32], words: usize) -> Self {
        let mut v = vec![0u64; words];
        for &i in ids {
            v[i as usize / 64] |= 1 << (i % 64);
        }
        Bits(v)
    }

    fn count(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// `self ⊆ other`.
    fn is_subset(&self, other: &Bits) -> bool {
        self.0.iter().zip(&other.0).all(|(a, b)| a & !b == 0)
    }
}

/// Build the equivalence classes and find the dominators.
pub fn analyze(records: &[Record]) -> Analysis {
    let mut survived = 0;
    let mut killed_unattributed = 0;
    let mut other = 0;

    // Intern test ids so each kill-set becomes a fixed-width bitset.
    let mut tests: HashMap<&str, u32> = HashMap::new();
    let mut killed: Vec<(&str, Vec<u32>)> = Vec::new();
    for r in records {
        match r.status.as_str() {
            "killed" if r.kill_set.is_empty() => killed_unattributed += 1,
            "killed" => {
                let ids = r
                    .kill_set
                    .iter()
                    .map(|t| {
                        let next = tests.len() as u32;
                        *tests.entry(t.as_str()).or_insert(next)
                    })
                    .collect();
                killed.push((r.mutant_id.as_str(), ids));
            }
            "survived" => survived += 1,
            _ => other += 1,
        }
    }
    let words = tests.len().div_ceil(64);

    // Group identical kill-sets.
    let mut by_set: HashMap<Bits, Vec<&str>> = HashMap::new();
    for (id, ids) in &killed {
        by_set.entry(Bits::with(ids, words)).or_default().push(id);
    }
    let mut groups: Vec<(Bits, usize, Vec<&str>)> = by_set
        .into_iter()
        .map(|(bits, mut members)| {
            members.sort_unstable();
            let n = bits.count();
            (bits, n, members)
        })
        .collect();
    groups.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2[0].cmp(b.2[0])));

    // A class is a dominator iff no other class is a strict subset of it. A
    // strict subset has strictly fewer bits, and the groups are sorted by bit
    // count, so only the groups before the first one of equal size need
    // checking. Distinct classes of equal size can't contain each other.
    let mut classes: Vec<Class> = Vec::with_capacity(groups.len());
    let mut smaller_end = 0;
    for i in 0..groups.len() {
        while groups[smaller_end].1 < groups[i].1 {
            smaller_end += 1;
        }
        let (bits, n, members) = &groups[i];
        let dominator = !groups[..smaller_end]
            .iter()
            .any(|(b, _, _)| b.is_subset(bits));
        classes.push(Class {
            rep: members[0].to_string(),
            members: members.iter().map(|s| s.to_string()).collect(),
            kill_set_size: *n,
            dominator,
        });
    }
    classes.sort_by(|a, b| {
        b.dominator
            .cmp(&a.dominator)
            .then_with(|| a.kill_set_size.cmp(&b.kill_set_size))
            .then_with(|| a.rep.cmp(&b.rep))
    });

    let dominators = classes.iter().filter(|c| c.dominator).count();
    let reduction_pct = if killed.is_empty() {
        0.0
    } else {
        round1(100.0 * (1.0 - dominators as f64 / killed.len() as f64))
    };
    let stats = Stats {
        records: records.len(),
        killed: killed.len(),
        survived,
        killed_unattributed,
        other,
        classes: classes.len(),
        dominators,
        reduction_pct,
        dominator_score: dominator_score(dominators, survived),
    };
    Analysis { stats, classes }
}

/// `100 · D / (D + S)`, rounded to one decimal; `None` on a zero denominator.
fn dominator_score(dominators: usize, survivors: usize) -> Option<f64> {
    let denom = dominators + survivors;
    (denom > 0).then(|| round1(100.0 * dominators as f64 / denom as f64))
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The source file a mutant id names. Ids are
/// `<file>@<offset>:<operator>:<original>-><replacement>` (see
/// `mutator::visitor`), so the file is everything before the first
/// `@<digits>:<operator>:` marker. `None` for an id that doesn't match.
pub fn id_file(id: &str) -> Option<&str> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"@\d+:[a-z][a-z0-9-]*:").expect("valid regex"));
    re.find(id)
        .map(|m| &id[..m.start()])
        .filter(|f| !f.is_empty())
}

/// AST hash of every file the killed and survived records name. A file that
/// can't be read is left out, which makes it count as stale later.
pub fn file_hashes(records: &[Record]) -> BTreeMap<String, String> {
    let files: HashSet<&str> = records
        .iter()
        .filter(|r| matches!(r.status.as_str(), "killed" | "survived"))
        .filter_map(|r| id_file(&r.mutant_id))
        .collect();
    files
        .into_iter()
        .filter_map(|f| {
            crate::ast_hash::hash_file_ast(Path::new(f))
                .ok()
                .map(|h| (f.to_string(), h))
        })
        .collect()
}

/// `.fermut/dominators.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DominatorStore {
    pub version: u32,
    /// UTC timestamp of the `fermut subsume` run that wrote this store.
    pub generated_at: String,
    /// The kill-set JSONL it was derived from, as given on the command line.
    pub kill_sets: String,
    pub stats: Stats,
    pub classes: Vec<Class>,
    /// Mutated file (as it appears in mutant ids) → AST hash at derivation
    /// time. The freshness gate for [`report_dominator_score`].
    pub file_hashes: BTreeMap<String, String>,
}

impl DominatorStore {
    pub fn new(
        analysis: Analysis,
        kill_sets: &Path,
        file_hashes: BTreeMap<String, String>,
    ) -> Self {
        Self {
            version: STORE_VERSION,
            generated_at: crate::history::iso8601_now(),
            kill_sets: kill_sets.display().to_string(),
            stats: analysis.stats,
            classes: analysis.classes,
            file_hashes,
        }
    }

    /// `Ok(None)` when no store exists. A store that exists but can't be read,
    /// parsed, or has another version is an error, so the caller can say why
    /// the score is missing.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        let store: Self =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            store.version == STORE_VERSION,
            "{} has version {}, expected {STORE_VERSION}; re-run `fermut subsume`",
            path.display(),
            store.version
        );
        Ok(Some(store))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let raw = serde_json::to_string_pretty(self).context("serializing dominator store")?;
        crate::cache::atomic_write(path, raw)
    }
}

/// Dominator score for a finished run, from a stored class map.
///
/// Returns `Err(reason)` when the store can't vouch for this run:
/// - a scored mutant's file is missing from the store or its AST changed;
/// - a detected (killed/timed-out) mutant isn't in the store's class map, so
///   its subsumption relations are unknown.
///
/// Otherwise `D` is the number of dominator classes with at least one member
/// detected in this run, `S` this run's survivors after static folding
/// ([`crate::report::fold`]), and the score is
/// `D / (D + S)`. Classes with no member in the run (e.g. a `--diff-only`
/// subset) drop out. Skipped/equivalent/errored outcomes are ignored, as in the
/// plain score.
pub fn report_dominator_score(
    store: &DominatorStore,
    outcomes: &[MutantOutcome],
) -> std::result::Result<Option<f64>, String> {
    let mut class_of: HashMap<&str, usize> = HashMap::new();
    for (i, c) in store.classes.iter().enumerate() {
        for m in &c.members {
            class_of.insert(m.as_str(), i);
        }
    }

    let mut fresh: HashMap<&Path, bool> = HashMap::new();
    let mut detected_dominators: HashSet<usize> = HashSet::new();
    let mut survivors = Vec::new();
    for o in outcomes {
        let detected = match o {
            MutantOutcome::Killed { .. } | MutantOutcome::TimedOut { .. } => true,
            MutantOutcome::Survived { .. } => false,
            _ => continue,
        };
        let m = o.mutant();
        let ok = *fresh.entry(m.file.as_path()).or_insert_with(|| {
            let key = m.file.display().to_string();
            store.file_hashes.get(&key).is_some_and(|recorded| {
                crate::ast_hash::hash_file_ast(&m.file).is_ok_and(|now| &now == recorded)
            })
        });
        if !ok {
            return Err(format!(
                "{} changed since `fermut subsume` (or was not recorded)",
                m.file.display()
            ));
        }
        if !detected {
            survivors.push(m);
            continue;
        }
        let Some(&ci) = class_of.get(m.id.as_str()) else {
            return Err(format!("detected mutant {} has no recorded kill-set", m.id));
        };
        if store.classes[ci].dominator {
            detected_dominators.insert(ci);
        }
    }
    let survivor_classes = crate::report::fold::fold(&survivors).len();
    Ok(dominator_score(detected_dominators.len(), survivor_classes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn rec(id: &str, status: &str, kills: &[&str]) -> Record {
        Record {
            mutant_id: id.into(),
            status: status.into(),
            kill_set: kills.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn dominator_reps(a: &Analysis) -> Vec<&str> {
        a.classes
            .iter()
            .filter(|c| c.dominator)
            .map(|c| c.rep.as_str())
            .collect()
    }

    #[test]
    fn identical_kill_sets_collapse_and_strict_supersets_are_dominated() {
        let a = analyze(&[
            rec("m1", "killed", &["t1"]),
            rec("m2", "killed", &["t1", "t2"]),
            rec("m3", "killed", &["t2", "t1"]), // same set as m2, different order
            rec("m4", "killed", &["t3"]),
            rec("m5", "survived", &[]),
        ]);
        assert_eq!(a.stats.killed, 4);
        assert_eq!(a.stats.classes, 3);
        assert_eq!(a.stats.dominators, 2);
        assert_eq!(dominator_reps(&a), vec!["m1", "m4"]);
        let m2 = a.classes.iter().find(|c| c.rep == "m2").unwrap();
        assert_eq!(m2.members, vec!["m2", "m3"]);
        assert!(!m2.dominator);
        assert_eq!(a.stats.reduction_pct, 50.0);
        // D=2, S=1.
        assert_eq!(a.stats.dominator_score, Some(66.7));
    }

    #[test]
    fn incomparable_kill_sets_are_all_dominators() {
        let a = analyze(&[
            rec("a", "killed", &["t1", "t2"]),
            rec("b", "killed", &["t2", "t3"]),
            rec("c", "killed", &["t1", "t3"]),
        ]);
        assert_eq!(a.stats.dominators, 3);
        assert_eq!(a.stats.dominator_score, Some(100.0));
    }

    #[test]
    fn empty_kill_sets_and_non_verdicts_stay_out_of_the_lattice() {
        // An empty-K kill would be a subset of everything and become the sole
        // dominator. It must be counted aside instead.
        let a = analyze(&[
            rec("import-kill", "killed", &[]),
            rec("x", "killed", &["t1"]),
            rec("y", "killed", &["t2"]),
            rec("slow", "timed_out", &[]),
            rec("boom", "error", &[]),
        ]);
        assert_eq!(a.stats.killed_unattributed, 1);
        assert_eq!(a.stats.other, 2);
        assert_eq!(a.stats.dominators, 2);
        assert!(a.classes.iter().all(|c| c.rep != "import-kill"));
    }

    #[test]
    fn nothing_killed_gives_no_reduction_and_no_score() {
        let a = analyze(&[]);
        assert_eq!(a.stats.reduction_pct, 0.0);
        assert_eq!(a.stats.dominator_score, None);
        let a = analyze(&[rec("s", "survived", &[])]);
        assert_eq!(a.stats.dominator_score, Some(0.0));
    }

    #[test]
    fn many_tests_span_multiple_bitset_words() {
        // 130 tests → 3 words. `wide` kills all of them, `narrow` only the last.
        let all: Vec<String> = (0..130).map(|i| format!("t{i}")).collect();
        let all_refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let a = analyze(&[
            rec("wide", "killed", &all_refs),
            rec("narrow", "killed", &["t129"]),
        ]);
        assert_eq!(dominator_reps(&a), vec!["narrow"]);
    }

    #[test]
    fn id_file_strips_the_offset_operator_suffix() {
        assert_eq!(
            id_file("/r/src/a.py@73:string-to-empty:\"x@1:y\"->\"\""),
            Some("/r/src/a.py")
        );
        assert_eq!(id_file("f.py@5:boundary-shift:<=-><"), Some("f.py"));
        assert_eq!(id_file("no-marker"), None);
        assert_eq!(id_file("@5:op:x->y"), None);
    }

    #[test]
    fn load_records_skips_blanks_dedups_and_rejects_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("ks.jsonl");
        std::fs::write(
            &p,
            "{\"mutant_id\":\"a\",\"status\":\"killed\",\"kill_set\":[\"t\"]}\n\n\
             {\"mutant_id\":\"a\",\"status\":\"survived\",\"kill_set\":[]}\n",
        )
        .unwrap();
        let rs = load_records(&p).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].status, "killed");

        std::fs::write(&p, "{\"mutant_id\":\"a\"}\nnot json\n").unwrap();
        let err = load_records(&p).unwrap_err().to_string();
        assert!(err.contains(":1:"), "{err}");
    }

    /// Golden numbers from the Python prototype behind
    /// `plan-firstorder-dominator-subsumption.md`, on the kill-sets committed
    /// under `benchmarks/hom-experiment/data/`.
    #[test]
    fn reproduces_prototype_dominator_counts_on_recorded_data() {
        let cases = [
            ("pyjwt-api_jws-fom", 284, 127, 38, 62),
            ("more-itertools-recipes-fom", 412, 113, 62, 68),
            ("markupsafe-native-fom", 19, 3, 1, 0),
        ];
        if which::which("gzip").is_err() {
            eprintln!("skipping: gzip not on PATH");
            return;
        }
        for (name, killed, classes, dominators, survived) in cases {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("benchmarks/hom-experiment/data")
                .join(format!("{name}.jsonl.gz"));
            let records = load_gz(&path);
            let a = analyze(&records);
            assert_eq!(a.stats.killed, killed, "{name} killed");
            assert_eq!(a.stats.classes, classes, "{name} classes");
            assert_eq!(a.stats.dominators, dominators, "{name} dominators");
            assert_eq!(a.stats.survived, survived, "{name} survived");
        }
    }

    fn load_gz(path: &Path) -> Vec<Record> {
        let out = std::process::Command::new("gzip")
            .arg("-dc")
            .arg(path)
            .output()
            .expect("running gzip");
        assert!(out.status.success(), "gzip -dc {}", path.display());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &out.stdout).unwrap();
        load_records(tmp.path()).unwrap()
    }

    fn mutant(file: &Path, id_suffix: &str) -> crate::mutator::Mutant {
        crate::mutator::Mutant {
            id: format!("{}@{id_suffix}", file.display()),
            file: file.to_path_buf(),
            operator: crate::mutator::Operator::CompareOpSwap,
            range: ruff_text_size::TextRange::new(20.into(), 21.into()),
            original: "<".into(),
            replacement: ">=".into(),
            line: 2,
            stmt_line: 2,
            site: None,
        }
    }

    #[test]
    fn report_score_counts_detected_dominator_classes_against_survivors() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        std::fs::write(&file, "def f(x):\n    return x < 1\n").unwrap();
        let m1 = mutant(&file, "1:compare-op-swap:<->>=");
        let m2 = mutant(&file, "2:compare-op-swap:<->>");
        let m3 = mutant(&file, "3:boundary-shift:<-><=");
        let records = [
            rec(&m1.id, "killed", &["t1"]),
            rec(&m2.id, "killed", &["t1", "t2"]),
            rec(&m3.id, "survived", &[]),
        ];
        let store = DominatorStore::new(analyze(&records), Path::new("ks"), file_hashes(&records));
        assert_eq!(store.file_hashes.len(), 1);

        let outcomes = vec![
            MutantOutcome::killed(Arc::new(m1.clone())),
            MutantOutcome::killed(Arc::new(m2.clone())),
            MutantOutcome::survived(Arc::new(m3.clone())),
        ];
        // One dominator class (m1) detected, one survivor.
        assert_eq!(report_dominator_score(&store, &outcomes), Ok(Some(50.0)));

        // A detected mutant the store never saw → no score.
        let stranger = mutant(&file, "9:compare-op-swap:<->==");
        let mut more = outcomes.clone();
        more.push(MutantOutcome::killed(Arc::new(stranger)));
        assert!(report_dominator_score(&store, &more).is_err());

        // Editing the file's AST invalidates the store.
        std::fs::write(&file, "def f(x):\n    return x <= 2\n").unwrap();
        let err = report_dominator_score(&store, &outcomes).unwrap_err();
        assert!(err.contains("changed"), "{err}");
    }

    #[test]
    fn store_round_trips_and_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".fermut/dominators.json");
        assert!(DominatorStore::load(&p).unwrap().is_none());
        let store = DominatorStore::new(
            analyze(&[rec("m", "killed", &["t"])]),
            Path::new("ks.jsonl"),
            BTreeMap::new(),
        );
        store.save(&p).unwrap();
        let back = DominatorStore::load(&p).unwrap().unwrap();
        assert_eq!(back.stats, store.stats);
        assert_eq!(back.classes, store.classes);

        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        raw["version"] = 99.into();
        std::fs::write(&p, raw.to_string()).unwrap();
        assert!(DominatorStore::load(&p).is_err());
    }
}
