//! `fermut list` — enumerate mutants without running tests. Applies the same
//! pre-test filter chain as `run` (minus the ones that need a live suite), so
//! the printed catalogue reflects what `run` would actually test.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::build_config::build_config;
use crate::cli::{FilterArgs, RunConfigArgs};
use crate::filter;

/// `fermut list` argument struct (flattened into `Cmd::List`).
#[derive(clap::Args, Debug)]
pub(crate) struct ListArgs {
    #[arg(default_value = ".")]
    pub(crate) path: PathBuf,

    /// Skip the ty pre-filter — list every generated mutant, including ones
    /// ty would reject as type-invalid. Matches `run --no-ty-filter`.
    #[arg(long)]
    pub(crate) no_ty_filter: bool,

    /// Enable the ruff lint pre-filter. Requires `ruff` on PATH. Matches
    /// `run --ruff-filter`.
    #[arg(long)]
    pub(crate) ruff_filter: bool,

    /// Enable the TCE (bytecode-equivalence) pre-filter — drop mutants that
    /// `compile()` to a byte-identical code object. Matches `run --tce`.
    #[arg(long)]
    pub(crate) tce: bool,

    #[command(flatten)]
    pub(crate) filter: FilterArgs,
}

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
        RunConfigArgs {
            no_ty_filter,
            ruff_filter,
            tce,
            // `list` never runs tests, so disable the run-only machinery
            // (cache, history, equiv detect, baseline).
            no_cache: true,
            no_history: true,
            no_equiv_detect: true,
            no_verify_baseline: true,
            filter: f,
            ..Default::default()
        },
    )?;
    let mutants = crate::mutator::collect_from_tree(&cfg.source_root, &cfg.exclude)?;
    let chain = filter::build_chain(&cfg)?;
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
