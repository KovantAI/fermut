use anyhow::Result;
use clap::Parser;

/// Worker-thread stack size. The mutation visitor walks the Python AST
/// recursively, and the platform default main-thread stack (~1 MiB on
/// Windows MSVC) overflows on otherwise-modest source files. Linux/macOS
/// default to ~8 MiB and never hit it, so the bug only surfaced in Windows
/// CI. Run the whole CLI on a thread with a generous stack instead.
const STACK_SIZE: usize = 256 * 1024 * 1024;

fn main() -> Result<()> {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(run)
        .expect("spawn worker thread")
        .join()
        .expect("worker thread panicked")
}

fn run() -> Result<()> {
    let cli = fermut::Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(cli.tracing_filter())
        .init();
    cli.run()
}
