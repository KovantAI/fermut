//! `fermut subsume` — derive dominator mutants from recorded kill-sets.
//!
//! Reads a `fermut run --record-kill-sets` JSONL, groups killed mutants into
//! kill-set equivalence classes, keeps the ⊆-minimal ones (the dominators), and
//! writes the class map to `.fermut/dominators.json`. Prints the reduction and
//! the dominator score. Offline: runs no tests. The analysis lives in
//! [`crate::subsume`].

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::Format;
use crate::subsume::{self, DominatorStore, Stats};

/// `fermut subsume` arguments.
#[derive(clap::Args, Debug)]
pub(crate) struct SubsumeArgs {
    /// Kill-set JSONL written by `fermut run --record-kill-sets`.
    pub(crate) kill_sets: PathBuf,

    /// Where to write the dominator store. Default:
    /// `<project root>/.fermut/dominators.json`, where `fermut run` reads it.
    #[arg(short, long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,

    /// Output format for the summary on stdout.
    #[arg(long, value_enum, default_value_t = Format::Human)]
    pub(crate) format: Format,
}

pub(crate) fn run(args: SubsumeArgs) -> Result<()> {
    let SubsumeArgs {
        kill_sets,
        output,
        format,
    } = args;
    let records = subsume::load_records(&kill_sets)?;
    let hashes = subsume::file_hashes(&records);
    let store = DominatorStore::new(subsume::analyze(&records), &kill_sets, hashes);

    let output = output.unwrap_or_else(|| {
        let root = crate::history::resolve_root(&std::env::current_dir().unwrap_or_default());
        subsume::default_dominators_path(&root)
    });
    store.save(&output)?;

    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&store.stats)?),
        Format::Human => print_human(&store.stats, &output),
    }
    if store.file_hashes.is_empty() && store.stats.records > 0 {
        tracing::warn!(
            "no mutated source files found from {}, so `fermut run` will never treat \
             this store as fresh (run `fermut subsume` from the project root of a \
             checkout of the recorded commit; in CI, add a checkout step)",
            std::env::current_dir().unwrap_or_default().display()
        );
    }
    Ok(())
}

fn print_human(s: &Stats, output: &std::path::Path) {
    println!(
        "kill-sets: {} mutants ({} killed, {} survived)",
        s.records,
        s.killed + s.killed_unattributed,
        s.survived
    );
    if s.killed_unattributed + s.other > 0 {
        println!(
            "  left out: {} killed without a kill-set, {} timed out/errored",
            s.killed_unattributed, s.other
        );
    }
    println!("classes:   {} distinct kill-sets", s.classes);
    println!(
        "dominators: {} ({:.1}% fewer mutants to certify the {} killed)",
        s.dominators, s.reduction_pct, s.killed
    );
    match s.dominator_score {
        Some(score) => println!(
            "dominator score: {score:.1}% ({} dominators / {} dominators + {} survivors)",
            s.dominators, s.dominators, s.survived
        ),
        None => println!("dominator score: N/A (nothing killed or survived)"),
    }
    println!("wrote {}", output.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_kill_sets(dir: &std::path::Path, lines: &[&str]) -> PathBuf {
        let p = dir.join("kill-sets.jsonl");
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    #[test]
    fn run_writes_store_to_explicit_output_in_both_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let ks = write_kill_sets(
            tmp.path(),
            &[
                r#"{"mutant_id":"a.py@1:x","status":"killed","kill_set":["t1"]}"#,
                r#"{"mutant_id":"a.py@2:x","status":"killed","kill_set":["t1","t2"]}"#,
                r#"{"mutant_id":"a.py@3:x","status":"killed","kill_set":[]}"#,
                r#"{"mutant_id":"a.py@4:x","status":"timed_out"}"#,
                r#"{"mutant_id":"a.py@5:x","status":"survived"}"#,
            ],
        );
        for format in [Format::Human, Format::Json] {
            let out = tmp.path().join(format!("{format:?}/dominators.json"));
            run(SubsumeArgs {
                kill_sets: ks.clone(),
                output: Some(out.clone()),
                format,
            })
            .unwrap();
            let store = DominatorStore::load(&out).unwrap().unwrap();
            assert_eq!(store.stats.records, 5);
            assert_eq!(store.stats.killed, 2);
            assert_eq!(store.stats.killed_unattributed, 1);
            assert_eq!(store.stats.other, 1);
            assert_eq!(store.stats.dominators, 1);
            assert_eq!(store.stats.dominator_score, Some(50.0));
        }
    }

    #[test]
    fn run_with_nothing_killed_or_survived_has_no_dominator_score() {
        let tmp = tempfile::tempdir().unwrap();
        let ks = write_kill_sets(
            tmp.path(),
            &[r#"{"mutant_id":"a.py@1:x","status":"error"}"#],
        );
        let out = tmp.path().join("dominators.json");
        run(SubsumeArgs {
            kill_sets: ks,
            output: Some(out.clone()),
            format: Format::Human,
        })
        .unwrap();
        let store = DominatorStore::load(&out).unwrap().unwrap();
        assert_eq!(store.stats.dominator_score, None);
    }

    #[test]
    fn run_errors_on_missing_kill_sets() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run(SubsumeArgs {
            kill_sets: tmp.path().join("nope.jsonl"),
            output: Some(tmp.path().join("d.json")),
            format: Format::Json,
        })
        .is_err());
    }
}
