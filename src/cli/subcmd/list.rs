//! `fermut list` argument struct (flattened into `Cmd::List`).

#[derive(clap::Args, Debug)]
pub(crate) struct ListArgs {
    #[arg(default_value = ".")]
    pub(crate) path: std::path::PathBuf,

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
    pub(crate) filter: crate::cli::FilterArgs,
}
