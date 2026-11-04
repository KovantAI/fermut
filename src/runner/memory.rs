//! Per-mutant memory cap (Linux, macOS, Windows).
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
//! On Windows each run joins a Job Object with `JOB_OBJECT_LIMIT_PROCESS_MEMORY`
//! ([`watch`]): the kernel caps every process in it (test subprocesses join
//! too) at the same per-process committed memory, so an allocation past the
//! cap fails → `MemoryError`, as on Linux. The baseline's job (no limit)
//! measures its peak for the auto cap ([`children_peak_rss`]). See
//! `plan-windows-memory-cap.md`.

use std::process::Command;

/// Env var the schemata template reads to cap each forked child.
pub(crate) const CAP_ENV: &str = "FM_MEM_CAP";

/// Auto cap: this many times the baseline's peak RSS…
const CAP_FACTOR: u64 = 4;
/// …but never below this. A mutant's suite runs beside other workers and
/// `RLIMIT_DATA` counts writable mappings, not resident pages, so leave room.
const CAP_FLOOR: u64 = 2 << 30;

/// Whether this platform enforces the cap.
pub(crate) const ENFORCED: bool = cfg!(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
));

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

/// Windows: the largest committed memory of any process in a finished run's
/// job (the baseline's, first). See [`watch`].
#[cfg(windows)]
pub(crate) fn children_peak_rss() -> Option<u64> {
    let peak = job::PEAK.load(std::sync::atomic::Ordering::Relaxed);
    (peak > 0).then_some(peak)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn children_peak_rss() -> Option<u64> {
    None
}

/// Watch the running process `pid` (just spawned) against `cap` until the
/// returned guard drops (drop it once the run is over). macOS: the watchdog
/// samples its group. Windows: `pid` joins a Job Object capping each process
/// in it, and the guard records the run's peak; with no cap (the baseline) the
/// job only measures. Linux: nothing, the kernel enforces `RLIMIT_DATA`.
pub(crate) fn watch(pid: u32, cap: Option<u64>) -> Watch {
    #[cfg(target_os = "macos")]
    if let Some(cap) = cap {
        watchdog::add(pid, cap);
        return Watch(Some(pid));
    }
    #[cfg(windows)]
    return Watch(job::assign(pid, cap));
    #[cfg(not(windows))]
    {
        let _ = (pid, cap);
        Watch(None)
    }
}

/// Stops watching on drop. See [`watch`]. Holds the watched pid on macOS and
/// the job handle (as an integer, so the guard stays `Send`) on Windows.
#[must_use]
pub(crate) struct Watch(
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))] Option<WatchId>,
);

#[cfg(windows)]
type WatchId = isize;
#[cfg(not(windows))]
type WatchId = u32;

impl Drop for Watch {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(pid) = self.0 {
            watchdog::remove(pid);
        }
        #[cfg(windows)]
        if let Some(handle) = self.0 {
            job::release(handle);
        }
    }
}

#[cfg(windows)]
mod job {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// Largest committed memory of any process in any finished run's job.
    pub(super) static PEAK: AtomicU64 = AtomicU64::new(0);

    const INFO_LEN: u32 = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;

    /// A new job holding process `pid`, capping each of its processes'
    /// committed memory at `cap`. `None` (warned once, run uncapped) when
    /// Windows refuses, e.g. fermut sits in a job that forbids nesting.
    ///
    /// ponytail: `pid` runs a few microseconds before it joins, far too short
    /// to allocate gigabytes or spawn children; spawn `CREATE_SUSPENDED` and
    /// resume after assigning if that ever matters (needs the thread handle,
    /// which `std::process::Child` doesn't expose).
    pub(super) fn assign(pid: u32, cap: Option<u64>) -> Option<isize> {
        // SAFETY: plain Win32 calls on handles this function owns: the job
        // (closed on failure here, else by `release`) and the process handle
        // (closed before returning). The limit struct is a zeroed,
        // caller-owned local passed with its exact size.
        #[allow(unsafe_code)] // sole FFI site: put a mutant run in a capped job
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                warn_once();
                return None;
            }
            let joined = (|| {
                if let Some(cap) = cap {
                    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                    info.ProcessMemoryLimit = usize::try_from(cap).unwrap_or(usize::MAX);
                    let set = SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        std::ptr::from_ref(&info).cast(),
                        INFO_LEN,
                    );
                    if set == 0 {
                        return false;
                    }
                }
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return false;
                }
                let assigned = AssignProcessToJobObject(job, process) != 0;
                CloseHandle(process);
                assigned
            })();
            if !joined {
                warn_once();
                CloseHandle(job);
                return None;
            }
            Some(job as isize)
        }
    }

    /// Fold the job's peak per-process memory into [`PEAK`], then close it.
    /// Closing doesn't touch the processes (no kill-on-close).
    pub(super) fn release(handle: isize) {
        let job = handle as HANDLE;
        // SAFETY: `job` is the handle `assign` returned, closed exactly once
        // here; the query writes at most `INFO_LEN` bytes into a zeroed local.
        #[allow(unsafe_code)] // sole FFI site: read a finished run's peak, close its job
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let read = QueryInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut info).cast(),
                INFO_LEN,
                std::ptr::null_mut(),
            );
            if read != 0 {
                PEAK.fetch_max(info.PeakProcessMemoryUsed as u64, Ordering::Relaxed);
            }
            CloseHandle(job);
        }
    }

    fn warn_once() {
        static WARNED: AtomicBool = AtomicBool::new(false);
        let err = std::io::Error::last_os_error();
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "couldn't put a test run in a Windows job object ({err}); \
                 runs are not memory-capped"
            );
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

    /// Run `script` under a Windows job capped at `cap` (`None`: measure
    /// only). `None` when no `python` is on PATH (GitHub's Windows images
    /// ship one; skip elsewhere rather than fail).
    #[cfg(windows)]
    fn run_in_job(script: &str, cap: Option<u64>) -> Option<std::process::Output> {
        let Ok(py) = which::which("python") else {
            eprintln!("skipping: no python on PATH");
            return None;
        };
        let child = Command::new(py)
            .args(["-c", script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let guard = watch(child.id(), cap);
        let out = child.wait_with_output().unwrap();
        drop(guard);
        Some(out)
    }

    /// The sleep puts the allocation well after the process joins its job.
    #[cfg(windows)]
    const ALLOC_512M: &str = "import time; time.sleep(0.3); x = bytearray(512 << 20)";

    #[cfg(windows)]
    #[test]
    fn windows_job_turns_an_oversized_allocation_into_memory_error() {
        let Some(out) = run_in_job(ALLOC_512M, Some(64 << 20)) else {
            return;
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        assert!(stderr.contains("MemoryError"), "{stderr}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_without_a_cap_only_measures_the_peak() {
        let Some(out) = run_in_job(ALLOC_512M, None) else {
            return;
        };
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let peak = children_peak_rss().expect("peak recorded");
        assert!(peak >= 500 << 20, "peak {peak}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_caps_a_test_subprocess_too() {
        let script = "import subprocess, sys, time; time.sleep(0.3); \
                      sys.exit(subprocess.call([sys.executable, '-c', 'bytearray(512 << 20)']))";
        let Some(out) = run_in_job(script, Some(64 << 20)) else {
            return;
        };
        assert!(
            !out.status.success(),
            "the child joined the job and its cap"
        );
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
