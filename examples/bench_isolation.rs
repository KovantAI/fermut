//! Microbenchmark for `IsolationMode` mirror-build cost.
//!
//! Generates a synthetic project tree (configurable number of files and
//! bytes-per-file), then times `Mirror::build` for each mode `--iters`
//! times and prints a small report.
//!
//! Run:
//!
//! ```sh
//! cargo run --release --example bench_isolation
//! cargo run --release --example bench_isolation -- --files 2000 --bytes 4096 --iters 5
//! ```
//!
//! The numbers are filesystem-dependent. On APFS / btrfs / xfs / ReFS the
//! reflink path should be roughly constant time regardless of total bytes
//! (single CoW clone per file); plain copy scales with bytes-per-file.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use fermut::{time_mirror_build, IsolationMode};

struct Args {
    files: usize,
    bytes: usize,
    iters: usize,
    project: Option<std::path::PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut files = 500usize;
        let mut bytes = 2048usize;
        let mut iters = 3usize;
        let mut project: Option<std::path::PathBuf> = None;
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--files" => files = args.next().and_then(|v| v.parse().ok()).unwrap_or(files),
                "--bytes" => bytes = args.next().and_then(|v| v.parse().ok()).unwrap_or(bytes),
                "--iters" => iters = args.next().and_then(|v| v.parse().ok()).unwrap_or(iters),
                "--project" => project = args.next().map(std::path::PathBuf::from),
                "--help" | "-h" => {
                    eprintln!(
                        "usage: bench_isolation [--project DIR] [--files N] [--bytes N] [--iters N]\n\
                         defaults: --files 500 --bytes 2048 --iters 3\n\
                         --project: skip synthetic tree, bench against a real project root \
                         (must contain pyproject.toml or setup.cfg; assumes <root>/tests/ exists)"
                    );
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown arg: {other}");
                    std::process::exit(2);
                }
            }
        }
        Self {
            files,
            bytes,
            iters,
            project,
        }
    }
}

fn make_project(root: &Path, files: usize, bytes_per_file: usize) -> Result<()> {
    fs::create_dir_all(root.join("src")).context("mkdir src")?;
    fs::create_dir_all(root.join("tests")).context("mkdir tests")?;
    fs::write(root.join("pyproject.toml"), "[project]\nname='bench'\n")?;
    // Synthetic Python files. Content is a comment block padded to size so
    // every file has unique bytes (defeats any inode-content dedup the FS
    // might apply when files are identical).
    let template = "# bench filler ";
    let mut payload = String::with_capacity(bytes_per_file);
    while payload.len() < bytes_per_file {
        payload.push_str(template);
    }
    payload.truncate(bytes_per_file);
    for i in 0..files {
        // Spread across 20 subdirs to avoid one huge directory.
        let dir = root.join("src").join(format!("mod_{:02}", i % 20));
        fs::create_dir_all(&dir).ok();
        fs::write(
            dir.join(format!("f_{i:05}.py")),
            format!("# id {i}\n{payload}\n"),
        )?;
    }
    Ok(())
}

fn mean(durs: &[Duration]) -> Duration {
    let n = durs.len().max(1) as u32;
    durs.iter().sum::<Duration>() / n
}

fn min(durs: &[Duration]) -> Duration {
    durs.iter().copied().min().unwrap_or_default()
}

fn fmt(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    format!("{ms:>9.2} ms")
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut _tmp_holder: Option<tempfile::TempDir> = None;
    let project: std::path::PathBuf = if let Some(p) = &args.project {
        let p = p
            .canonicalize()
            .with_context(|| format!("canonicalize {}", p.display()))?;
        eprintln!("benching against real project: {}", p.display());
        p
    } else {
        let tmp = tempfile::tempdir().context("tempdir")?;
        let p = tmp.path().join("project");
        eprintln!(
            "generating synthetic tree: {} files × {} bytes under {}",
            args.files,
            args.bytes,
            p.display()
        );
        make_project(&p, args.files, args.bytes)?;
        _tmp_holder = Some(tmp);
        p
    };
    // Tests dir is conventional; fall back to project root if absent (mirror
    // build only uses the path to locate the project root).
    let tests = {
        let t = project.join("tests");
        if t.is_dir() {
            t
        } else {
            project.clone()
        }
    };

    let modes = [
        IsolationMode::Copy,
        IsolationMode::Hardlink,
        IsolationMode::Reflink,
        IsolationMode::Auto,
    ];

    if args.project.is_some() {
        println!("\nproject={} iters={}", project.display(), args.iters);
    } else {
        println!(
            "\nfiles={} bytes_per_file={} iters={} total_payload={} KiB",
            args.files,
            args.bytes,
            args.iters,
            (args.files * args.bytes) / 1024,
        );
    }
    println!("{:<10}  {:>12}  {:>12}", "mode", "best", "mean",);
    println!("{}", "-".repeat(40));
    for mode in modes {
        let mut samples = Vec::with_capacity(args.iters);
        // One warm-up to populate the FS cache; not recorded.
        let _ = time_mirror_build(&tests, mode)?;
        for _ in 0..args.iters {
            samples.push(time_mirror_build(&tests, mode)?);
        }
        println!(
            "{:<10}  {:>12}  {:>12}",
            format!("{mode:?}"),
            fmt(min(&samples)),
            fmt(mean(&samples)),
        );
    }
    Ok(())
}
