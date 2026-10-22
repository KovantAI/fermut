use anyhow::Result;
use clap::Parser;

/// Worker-thread stack size. The mutation visitor walks the Python AST
/// recursively, and the platform default main-thread stack (~1 MiB on
/// Windows MSVC) overflows on otherwise-modest source files. Linux/macOS
/// default to ~8 MiB and never hit it, so the bug only surfaced in Windows
/// CI. Run the whole CLI on a thread with a generous stack instead.
const STACK_SIZE: usize = 256 * 1024 * 1024;

fn main() -> Result<()> {
    // A panic is a fermut bug, never the user's project: say where to report it.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        eprintln!(
            "\nfermut hit an internal error. This is a fermut bug, not a problem with \
             your project: please report it, with the message above, at {}",
            fermut::ISSUES_URL
        );
    }));
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
