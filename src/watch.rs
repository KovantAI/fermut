//! Watch loop. After an initial run, re-runs the full mutation suite each
//! time a `.py` file changes anywhere under the source root. Debounces rapid
//! event bursts (saves, formatter on-save, batched git operations).
//!
//! Exit with Ctrl+C.

use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{event::EventKind, recommended_watcher, RecursiveMode, Watcher};
use tracing::info;

use crate::config::Config;
use crate::engine;
use crate::history::{self, HistoryEntry};
use crate::report::Report;

const DEBOUNCE: Duration = Duration::from_millis(500);

/// Run once, then loop forever re-running on `.py` changes.
///
/// The callback receives the `Report` plus a snapshot of the history
/// file taken *before* this iteration's `engine::run` — that snapshot
/// is the trend block's "every earlier run" window, and capturing it
/// per-tick keeps watch mode honest if an external writer (a CI shard,
/// another developer) appended between ticks.
pub fn watch_loop<F>(cfg: &Config, on_report: F) -> Result<()>
where
    F: Fn(&Report, &[HistoryEntry]) -> Result<()>,
{
    let prior = load_prior(cfg);
    let (initial, _entry) = engine::run(cfg)?;
    on_report(&initial, &prior)?;

    let (tx, rx) = channel();
    let mut watcher = recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating file watcher")?;
    watcher
        .watch(&cfg.source_root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", cfg.source_root.display()))?;

    info!(path = %cfg.source_root.display(), "watching for .py changes");
    eprintln!("watching {} — Ctrl+C to exit", cfg.source_root.display());

    loop {
        let event = rx.recv().context("watcher channel closed")?;
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "watcher event error");
                continue;
            }
        };
        if !is_interesting(&event) {
            continue;
        }
        drain_debounce(&rx);

        eprintln!("change detected — re-running");
        let prior = load_prior(cfg);
        match engine::run(cfg) {
            Ok((report, _entry)) => {
                if let Err(e) = on_report(&report, &prior) {
                    tracing::warn!(error = %e, "report handler error");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "run failed");
            }
        }
    }
}

fn load_prior(cfg: &Config) -> Vec<HistoryEntry> {
    if cfg.history {
        history::load(&cfg.history_path).unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn is_interesting(event: &notify::Event) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|p| is_python_path(p))
}

fn is_python_path(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()) == Some("py")
        && !path_contains_segment(p, ".fermut")
        && !path_contains_segment(p, "__pycache__")
}

fn path_contains_segment(p: &Path, needle: &str) -> bool {
    p.components()
        .any(|c| c.as_os_str().to_string_lossy() == needle)
}

fn drain_debounce(rx: &std::sync::mpsc::Receiver<notify::Result<notify::Event>>) {
    let deadline = Instant::now() + DEBOUNCE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
