//! `fermut completions` argument struct (flattened into `Cmd::Completions`).

use anyhow::Result;
use clap::Command;
use clap_complete::generate;

#[derive(clap::Args, Debug)]
pub(crate) struct CompletionsArgs {
    /// Target shell.
    #[arg(value_enum)]
    pub(crate) shell: clap_complete::Shell,
}

/// Dispatch handler: writes the completion script for the requested shell to
/// stdout. Takes the built clap [`Command`] (the caller owns the `Cli` type)
/// so this module needn't know the parser's concrete type.
pub(crate) fn run(args: CompletionsArgs, mut cmd: Command) -> Result<()> {
    generate(args.shell, &mut cmd, "fermut", &mut std::io::stdout());
    Ok(())
}
