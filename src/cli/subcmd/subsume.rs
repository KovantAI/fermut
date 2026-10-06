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
            "none of the mutated files could be read, so `fermut run` will never treat \
             this store as fresh (run `fermut subsume` on the machine that recorded it)"
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
