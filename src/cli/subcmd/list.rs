//! `fermut list` — enumerate mutants without running tests. Applies the same
//! pre-test filter chain as `run` (minus the ones that need a live suite), so
//! the printed catalogue reflects what `run` would actually test.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::build_config::build_config;
use crate::cli::FilterArgs;
use crate::filter;

/// `fermut list` flags, mirrored from the `Cmd::List` clap variant.
#[derive(Debug)]
pub struct ListOpts {
    pub path: PathBuf,
    pub no_ty_filter: bool,
    pub ruff_filter: bool,
    pub tce: bool,
    pub filter: FilterArgs,
}

pub fn run(opts: ListOpts) -> Result<()> {
    let ListOpts {
        path,
        no_ty_filter,
        ruff_filter,
        tce,
        filter: f,
    } = opts;
    let cfg = build_config(
        path,
        None,
        None,
        None,
        no_ty_filter,
        ruff_filter,
        tce,
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
    let mutants = crate::mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)?;
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
