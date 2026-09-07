//! `fermut completions` argument struct (flattened into `Cmd::Completions`).

#[derive(clap::Args, Debug)]
pub(crate) struct CompletionsArgs {
    /// Target shell.
    #[arg(value_enum)]
    pub(crate) shell: clap_complete::Shell,
}
