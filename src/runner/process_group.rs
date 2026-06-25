//! Cross-platform process-group helpers for the test runners.
//!
//! `Child::kill` only kills the direct child. pytest-xdist workers, hypothesis
//! subprocesses, and test-fixture servers (e.g. an httpbin spawned for an
//! integration test) live as grandchildren. On timeout we want them all gone
//! — otherwise a hung test leaks one stuck process per mutant.
//!
//! Approach:
//! - Unix: put the child in its own process group via `process_group(0)`.
//!   Kill the whole group with `killpg(-pid, SIGKILL)`.
//! - Windows: spawn into a new process group; send `CTRL_BREAK_EVENT` to it
//!   on timeout. (TODO — currently a best-effort `child.kill()`.)

use std::process::{Child, Command};

/// Configure `cmd` so that the spawned child becomes the leader of a new
/// process group. Must be called before `spawn()`.
pub(crate) fn with_new_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // pgid 0 = "use the child's pid as the new pgid" — i.e. the child
        // becomes its own process-group leader.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP — needed for GenerateConsoleCtrlEvent to
        // target this child specifically.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd;
}

/// Best-effort kill of `child` and every process in its group. Always called
/// after a timeout; the result is intentionally ignored — we already lost the
/// race against the wall clock, all we can do is clean up.
pub(crate) fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: libc::kill takes a pid and signal; passing a negated pid
        // signals the entire process group. No memory is shared with the
        // child. `child.id()` is u32; we cast to i32 because killpg is i32.
        let pid = child.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}
