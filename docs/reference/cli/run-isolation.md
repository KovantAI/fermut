# Isolation

How [`fermut run`](run.md) keeps mutants from touching your real source tree
and your home directory, and keeps tests in different workers from seeing
each other's files.

## Mirror modes (`--isolation`) { #isolation-modes }

Each worker thread builds one mirror of the project tree, reused across
every mutant that worker processes. `--isolation` picks how the mirror
is populated. The patched file is always unlinked and rewritten (never
edited in place), so the patched file can't clobber the real source —
but tests that write to *other* files inside the tree share their writes
with the original when in `hardlink` mode.

| Mode       | What it does                                                                                              | Build cost                              | Safety                                                                                  |
|------------|-----------------------------------------------------------------------------------------------------------|-----------------------------------------|-----------------------------------------------------------------------------------------|
| `auto`     | Reflink/clonefile on supported filesystems (APFS, btrfs, xfs, zfs, ReFS); else plain copy.                | Near-zero on CoW filesystems; full copy otherwise. | Same as `copy` — mirror is independent.                                                |
| `copy`     | Plain recursive `fs::copy`.                                                                                | Slowest; baseline.                       | Fully independent.                                                                       |
| `reflink`  | Force reflink/clonefile, fall back to copy on unsupported filesystems.                                     | Near-zero on CoW filesystems.            | Fully independent.                                                                       |
| `hardlink` | Hardlink every file from source into the mirror.                                                           | Cheapest — just inode entries.           | Shares inodes; safe for the patched file (unlinked before write), unsafe if tests rewrite *other* files. |

You can also set the mode in `fermut.toml` (`isolation = "hardlink"`) or
via the `FERMUT_ISOLATION` environment variable. Precedence is the
standard CLI > env > config-file > default.

!!! note "macOS / APFS"

    Rust's `std::fs::copy` already uses APFS `fclonefileat` under the
    hood, so `copy` and `reflink` perform nearly identically on macOS;
    `hardlink` is often *slower* (extra metadata work per file). The
    mode matters most on Linux ext4 / tmpfs and other filesystems
    without CoW.

## Private `HOME` { #private-home }

By default every worker, and every schemata fork, runs tests with a
private, empty `HOME` (and `XDG_*` directories), removed when it exits.
Tests that create files under `~` then can't fail each other across
workers, which would show up as a false kill, and can't write into your
real home.

Pass `--no-private-home` (or set `private_home = false`) when the suite
needs real home state: `~/.gitconfig`, credentials, caches under `~`. Such
a suite fails the baseline check under a private `HOME`, with a hint naming
the flag.
