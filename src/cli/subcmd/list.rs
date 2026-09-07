//! `fermut list` — enumerate mutations without running tests.
//!
//! Applies the same pre-test filters as `run` (ty, coverage, diff scope) so
//! the printed catalogue reflects what `run` would actually test, unless
//! `--no-ty-filter` is passed to see the raw generated set.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::build_config::build_config;
use crate::cli::FilterArgs;
use crate::{filter, mutator};

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Skip the ty pre-filter — list every generated mutant, including ones
    /// ty would reject as type-invalid. Matches `run --no-ty-filter`.
    #[arg(long)]
    pub no_ty_filter: bool,

    /// Enable the ruff lint pre-filter. Requires `ruff` on PATH. Matches
    /// `run --ruff-filter`.
    #[arg(long)]
    pub ruff_filter: bool,

    #[command(flatten)]
    pub filter: FilterArgs,
}

pub fn run(args: ListArgs) -> Result<()> {
    let ListArgs {
        path,
        no_ty_filter,
        ruff_filter,
        filter: f,
    } = args;
    let cfg = build_config(
        path,
        None,
        None,
        None,
        no_ty_filter,
        ruff_filter,
        None,
        Vec::new(),
        true,
        None,
        true,
        None,
        None, // sample
        None, // sample_seed
        None, // shard
        None, // runner
        None, // python
        None, // isolation
        true,
        None,
        None,
        true,
        None,
        false, // no_smart_order (list doesn't run tests)
        false, // smart_order
        None,  // max_time (list doesn't run tests)
        f,
    )?;
    let mutants = mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)?;
    let chain = filter::build_chain_for_list(&cfg)?;
    let mut kept = 0usize;
    for m in &mutants {
        if filter::first_rejector(&chain, m)?.is_none() {
            println!("{}", m.describe());
            kept += 1;
        }
    }
    println!("\n{kept} mutant(s) total");
    Ok(())
}
