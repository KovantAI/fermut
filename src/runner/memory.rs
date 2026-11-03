//! Per-mutant memory cap (Linux).
//!
//! A runaway mutant (an infinite append, a doubling loop) grows until its
//! timeout: single processes reached 4–8 GB and whole runs 10–18 GB. On Linux
//! each per-mutant process gets `RLIMIT_DATA` (private writable memory — not
//! shared libraries or `PROT_NONE` reservations, which make `RLIMIT_AS` fire
//! on innocent code). A mutant past the cap gets `MemoryError` → its test fails
//! → exit 1, or dies by a signal if native code aborts: both already score as
//! killed, so the cap needs no verdict logic of its own. Test subprocesses
//! inherit the limit; the schemata child sets it itself after the fork (see
//! `FM_MEM_CAP` in `_fermut_schema.py`).
//!
//! macOS doesn't enforce `RLIMIT_AS`/`RLIMIT_DATA`, so there a watchdog
//! thread samples each running mutant's resident memory and SIGKILLs its
//! process group past the cap ([`watch`]). A SIGKILLed run is a signal death,
//! which also scores as killed (and isn't one of schemata's crash signals, so
//! a forked run's verdict stands). ponytail: samples the group leader only (the
//! pytest process, or the schemata child), not xdist workers or test
//! subprocesses; sum the group via `proc_listpids` if a real run needs it.
//!
//! ponytail: Windows runs uncapped. The analog is a Job Object with
//! `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (kernel-enforced, inherited by child
//! processes, allocation fails → `MemoryError`): assign the test process
//! after spawn, `windows-sys` feature `Win32_System_JobObjects`. Add it when
//! a Windows user asks.

use std::process::Command;

/// Env var the schemata template reads to cap each forked child.
pub(crate) const CAP_ENV: &str = "FM_MEM_CAP";

/// Auto cap: this many times the baseline's peak RSS…
const CAP_FACTOR: u64 = 4;
/// …but never below this. A mutant's suite runs beside other workers and
/// `RLIMIT_DATA` counts writable mappings, not resident pages, so leave room.
const CAP_FLOOR: u64 = 2 << 30;

/// Whether this platform enforces the cap.
pub(crate) const ENFORCED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// The auto cap for a suite whose baseline peaked at `peak` bytes.
pub(crate) fn auto_cap(peak: u64) -> u64 {
    peak.saturating_mul(CAP_FACTOR).max(CAP_FLOOR)
}

/// Cap `cmd`'s `RLIMIT_DATA` at `bytes` (never above the inherited hard
/// limit). No-op where the cap isn't enforced.
pub(crate) fn cap_memory(cmd: &mut Command, bytes: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs in the forked child before exec and only
        // calls getrlimit/setrlimit, both async-signal-safe syscalls on a
        // stack-local struct; it allocates nothing and touches no shared state.
        #[allow(unsafe_code)] // sole FFI site: set the child's data limit
        unsafe {
            cmd.pre_exec(move || {
                let mut lim = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_DATA, &mut lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let cap = (bytes as libc::rlim_t).min(lim.rlim_max);
                lim.rlim_cur = cap;
                lim.rlim_max = cap;
                if libc::setrlimit(libc::RLIMIT_DATA, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let _ = (cmd, bytes);
}

/// Peak RSS (bytes) of the largest child fermut has reaped so far — after the
/// baseline, at least the whole suite's. `None` where the cap isn't enforced.
#[cfg(target_os = "linux")]
pub(crate) fn children_peak_rss() -> Option<u64> {
    // SAFETY: getrusage fills a caller-owned, zeroed struct; nothing shared.
    #[allow(unsafe_code)] // sole FFI site: read reaped children's peak RSS
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        (libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) == 0).then_some(usage)
    };
    // Linux reports kilobytes.
    usage.map(|u| u64::try_from(u.ru_maxrss).unwrap_or(0) * 1024)
}

#[cfg(target_os = "macos")]
pub(crate) fn children_peak_rss() -> Option<u64> {
    // SAFETY: as on Linux.
    #[allow(unsafe_code)] // sole FFI site: read reaped children's peak RSS
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        (libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) == 0).then_some(usage)
    };
    // macOS reports bytes.
    usage.map(|u| u64::try_from(u.ru_maxrss).unwrap_or(0))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn children_peak_rss() -> Option<u64> {
    None
}

/// Watch the running process group led by `pid` against `cap` until the
/// returned guard drops (drop it once the run is over). macOS only; elsewhere
/// the kernel enforces the cap and this does nothing.
pub(crate) fn watch(pid: u32, cap: Option<u64>) -> Watch {
    #[cfg(target_os = "macos")]
    if let Some(cap) = cap {
        watchdog::add(pid, cap);
        return Watch(Some(pid));
    }
    let _ = (pid, cap);
    Watch(None)
}

/// Stops watching on drop. See [`watch`].
#[must_use]
pub(crate) struct Watch(#[cfg_attr(not(target_os = "macos"), allow(dead_code))] Option<u32>);

impl Drop for Watch {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(pid) = self.0 {
            watchdog::remove(pid);
        }
    }
}

#[cfg(target_os = "macos")]
mod watchdog {
    use crate::sync::lock_recover;
    use std::sync::{Mutex, Once};
    use std::time::Duration;

    /// How often memory is sampled. A doubling loop can overshoot the cap by
    /// what it allocates in one tick; still far below an uncapped run.
    const TICK: Duration = Duration::from_millis(50);

    /// (group leader pid, cap) of every running mutant.
    static WATCHED: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());
    static START: Once = Once::new();

    pub(super) fn add(pid: u32, cap: u64) {
        START.call_once(|| {
            std::thread::Builder::new()
                .name("fermut-memory-watchdog".into())
                .spawn(run)
                .expect("spawning the memory watchdog");
        });
        lock_recover(&WATCHED).push((pid, cap));
    }

    pub(super) fn remove(pid: u32) {
        lock_recover(&WATCHED).retain(|&(p, _)| p != pid);
    }

    fn run() {
        loop {
            std::thread::sleep(TICK);
            // Sampled and killed under the lock, so a run that has removed
            // itself is never signalled after.
            let watched = lock_recover(&WATCHED);
            for &(pid, cap) in watched.iter() {
                if resident(pid).is_some_and(|rss| rss > cap) {
                    crate::runner::process_group::kill_pgid(pid);
                }
            }
        }
    }

    /// Resident memory of `pid` in bytes; `None` once it's gone.
    fn resident(pid: u32) -> Option<u64> {
        let pid = libc::c_int::try_from(pid).ok()?;
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into a caller-owned,
        // zeroed proc_taskinfo; nothing is shared with the process.
        #[allow(unsafe_code)] // sole FFI site: sample a mutant's resident memory
        unsafe {
            let mut info: libc::proc_taskinfo = std::mem::zeroed();
            let n = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                std::ptr::from_mut(&mut info).cast(),
                size,
            );
            (n == size).then_some(info.pti_resident_size)
        }
    }
}

/// Parse a size: bytes, or a number with a `K`/`M`/`G` suffix (binary, case-
/// insensitive, optional trailing `B`/`iB`). `0` means off.
pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let body = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower);
    let (num, shift) = match body.chars().last() {
        Some('k') => (&body[..body.len() - 1], 10),
        Some('m') => (&body[..body.len() - 1], 20),
        Some('g') => (&body[..body.len() - 1], 30),
        _ => (body, 0),
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {t:?}: use bytes or e.g. 512M, 4G (0 = off)"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(format!("invalid size {t:?}: must be non-negative"));
    }
    Ok((n * (1u64 << shift) as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_suffixes_and_zero() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("4g"), Ok(4 << 30));
        assert_eq!(parse_size("1.5GiB"), Ok(3 << 29));
        assert_eq!(parse_size("8kb"), Ok(8 << 10));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-1G").is_err());
    }

    #[test]
    fn auto_cap_scales_the_baseline_above_a_floor() {
        assert_eq!(auto_cap(100 << 20), CAP_FLOOR);
        assert_eq!(auto_cap(1 << 30), 4 << 30);
    }

    /// Peak RSS is in bytes on every platform (Linux reports kilobytes).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn children_peak_rss_reports_bytes() {
        let ok = Command::new("python3")
            .args(["-c", "x = b'x' * (64 << 20)"])
            .status()
            .expect("python3 on PATH")
            .success();
        assert!(ok);
        let peak = children_peak_rss().unwrap();
        assert!(peak >= 64 << 20, "{peak}");
    }

    /// The watchdog SIGKILLs a process that grows past the cap, and leaves
    /// one under it alone.
    #[cfg(target_os = "macos")]
    #[test]
    fn watchdog_kills_a_process_past_the_cap() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let spawn = |code: &str| {
            let mut cmd = Command::new("python3");
            cmd.args(["-c", code]).process_group(0);
            cmd.spawn().expect("python3 on PATH")
        };
        let mut big = spawn("import time; x = bytearray(1 << 30); time.sleep(30)");
        let guard = watch(big.id(), Some(256 << 20));
        let status = big.wait().unwrap();
        drop(guard);
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");

        let mut small = spawn("x = bytearray(1 << 20)");
        let _guard = watch(small.id(), Some(256 << 20));
        assert!(small.wait().unwrap().success());
    }

    /// The cap makes a runaway allocation fail fast instead of growing.
    #[cfg(target_os = "linux")]
    #[test]
    fn capped_child_cannot_allocate_past_the_cap() {
        let mut cmd = Command::new("python3");
        cmd.args(["-c", "x = bytearray(1 << 30)"]);
        cap_memory(&mut cmd, 256 << 20);
        let out = cmd.output().expect("python3 on PATH");
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("MemoryError"));
    }
}
