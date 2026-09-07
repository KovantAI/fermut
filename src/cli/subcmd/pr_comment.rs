//! `fermut pr-comment` — post a Markdown report to a PR, edit in place on
//! subsequent runs instead of stacking comments.
//!
//! Shells out to `gh` (the GitHub CLI) so we don't drag a HTTP/TLS stack
//! into the binary. `gh` is preinstalled on every `actions/setup-*` GitHub
//! Actions runner image; outside CI install it from
//! <https://cli.github.com>.
//!
//! Discovery is marker-based: every `fermut --markdown` report starts with
//! [`crate::report::writers::markdown::STICKY_MARKER`]. We list the PR's
//! issue comments via `gh api`, find the one whose body starts with that
//! marker (or a custom marker passed via `--marker`), and `PATCH` it. If
//! none matches we fall back to `gh pr comment` to create a fresh one.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::report::writers::markdown::STICKY_MARKER;

/// Inputs for `fermut pr-comment`. Most fields default from environment
/// variables that GitHub Actions sets automatically — keeps the CLI call
/// in the workflow file short.
#[derive(Debug, Clone)]
pub struct PrCommentOpts {
    /// Markdown file produced by a prior `fermut run … --markdown` invocation.
    pub markdown: PathBuf,
    /// `owner/repo`. Defaults to `$GITHUB_REPOSITORY`.
    pub repo: Option<String>,
    /// Pull-request number. Defaults to the PR in `$GITHUB_REF`
    /// (`refs/pull/<N>/merge`) or `$PR_NUMBER`.
    pub pr: Option<u64>,
    /// Override the marker substring used to identify the previous comment.
    /// Defaults to `<!-- fermut:report -->`.
    pub marker: Option<String>,
    /// Print the resolved plan and skip the network calls.
    pub dry_run: bool,
}

pub fn pr_comment(opts: PrCommentOpts) -> Result<()> {
    let body = std::fs::read_to_string(&opts.markdown)
        .with_context(|| format!("reading markdown report {}", opts.markdown.display()))?;
    let repo = resolve_repo(opts.repo.as_deref())?;
    let pr = resolve_pr(opts.pr)?;
    let marker = opts.marker.as_deref().unwrap_or(STICKY_MARKER);

    if opts.dry_run {
        println!("pr-comment plan:");
        println!("  repo   : {repo}");
        println!("  pr     : {pr}");
        println!("  marker : {marker}");
        println!(
            "  body   : {} bytes from {}",
            body.len(),
            opts.markdown.display()
        );
        return Ok(());
    }

    ensure_gh_available()?;

    match find_existing_comment(&repo, pr, marker)? {
        Some(id) => {
            patch_comment(&repo, id, &body)?;
            println!("updated comment {id} on {repo}#{pr}");
        }
        None => {
            create_comment(&repo, pr, &body)?;
            println!("created new comment on {repo}#{pr}");
        }
    }
    Ok(())
}

fn resolve_repo(arg: Option<&str>) -> Result<String> {
    if let Some(r) = arg {
        return Ok(r.to_string());
    }
    std::env::var("GITHUB_REPOSITORY")
        .map_err(|_| anyhow!("--repo not given and $GITHUB_REPOSITORY not set"))
}

fn resolve_pr(arg: Option<u64>) -> Result<u64> {
    if let Some(n) = arg {
        return Ok(n);
    }
    if let Ok(s) = std::env::var("PR_NUMBER") {
        return s
            .parse()
            .with_context(|| format!("PR_NUMBER={s} is not a number"));
    }
    if let Ok(s) = std::env::var("GITHUB_REF") {
        if let Some(n) = pr_from_ref(&s) {
            return Ok(n);
        }
    }
    bail!("--pr not given and could not infer from $PR_NUMBER or $GITHUB_REF")
}

/// `$GITHUB_REF` for pull-request events looks like `refs/pull/<N>/merge`
/// or `refs/pull/<N>/head`. Pull the number out without pulling in a regex.
fn pr_from_ref(ref_str: &str) -> Option<u64> {
    let rest = ref_str.strip_prefix("refs/pull/")?;
    let n = rest.split('/').next()?;
    n.parse().ok()
}

fn ensure_gh_available() -> Result<()> {
    let out = Command::new("gh").arg("--version").output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => bail!(
            "`gh --version` exited {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => bail!(
            "`gh` not found on PATH ({e}); install from https://cli.github.com or run inside a GitHub Actions runner"
        ),
    }
}

fn find_existing_comment(repo: &str, pr: u64, marker: &str) -> Result<Option<u64>> {
    // `--paginate` walks every page of issue comments so the marker
    // search stays correct on long-lived PRs. `--jq` does the filtering
    // in-process so we don't ship serde_json schemas for the GH API.
    let jq = format!(
        ".[] | select((.body // \"\") | startswith({})) | .id",
        json_quote(marker)
    );
    let out = Command::new("gh")
        .args([
            "api",
            "--paginate",
            &format!("repos/{repo}/issues/{pr}/comments"),
            "--jq",
            &jq,
        ])
        .output()
        .context("invoking `gh api`")?;
    if !out.status.success() {
        bail!(
            "`gh api repos/{repo}/issues/{pr}/comments` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8(out.stdout).context("`gh api` stdout not UTF-8")?;
    // Multiple matches can exist if a previous run created duplicates before
    // this command shipped. Take the first; subsequent runs converge.
    let first = stdout.lines().find(|l| !l.trim().is_empty());
    match first {
        Some(s) => s
            .trim()
            .parse()
            .map(Some)
            .with_context(|| format!("parsing comment id `{s}`")),
        None => Ok(None),
    }
}

fn patch_comment(repo: &str, comment_id: u64, body: &str) -> Result<()> {
    // Pipe the JSON payload via stdin so the report body — which can run
    // to hundreds of KB on large repos — never hits the per-process
    // `ARG_MAX` ceiling (typically 2 MB on Linux, less in some CI envs).
    let payload = format!("{{\"body\":{}}}", json_quote(body));
    let mut child = Command::new("gh")
        .args([
            "api",
            "-X",
            "PATCH",
            &format!("repos/{repo}/issues/comments/{comment_id}"),
            "--input",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("invoking `gh api PATCH`")?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("failed to open stdin for `gh api PATCH`"))?
        .write_all(payload.as_bytes())
        .context("writing payload to `gh api PATCH` stdin")?;
    let out = child
        .wait_with_output()
        .context("waiting for `gh api PATCH`")?;
    if !out.status.success() {
        bail!(
            "patching comment {comment_id} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn create_comment(repo: &str, pr: u64, body: &str) -> Result<()> {
    // `--body-file -` reads the comment body from stdin, avoiding the
    // `ARG_MAX` limit that `--body <large-string>` would hit.
    let mut child = Command::new("gh")
        .args([
            "pr",
            "comment",
            &pr.to_string(),
            "--repo",
            repo,
            "--body-file",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("invoking `gh pr comment`")?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("failed to open stdin for `gh pr comment`"))?
        .write_all(body.as_bytes())
        .context("writing body to `gh pr comment` stdin")?;
    let out = child
        .wait_with_output()
        .context("waiting for `gh pr comment`")?;
    if !out.status.success() {
        bail!(
            "creating comment on {repo}#{pr} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Minimal JSON string literal escaper. Used for the `--jq` marker filter
/// and for the `{"body": "..."}` payload piped to `gh api PATCH`. A
/// `serde_json::Value` detour would be overkill: we only need to escape
/// backslash, double quote, and control bytes — raw UTF-8 above 0x1F is
/// valid inside a JSON string.
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_from_ref_parses_merge_and_head() {
        assert_eq!(pr_from_ref("refs/pull/42/merge"), Some(42));
        assert_eq!(pr_from_ref("refs/pull/7/head"), Some(7));
        assert_eq!(pr_from_ref("refs/heads/main"), None);
        assert_eq!(pr_from_ref("nonsense"), None);
    }

    #[test]
    fn json_quote_escapes_specials() {
        assert_eq!(json_quote("plain"), "\"plain\"");
        assert_eq!(json_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_quote("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn resolve_repo_prefers_explicit_arg() {
        // Don't touch the env: a test running in CI would already have
        // GITHUB_REPOSITORY set, so we only assert the arg-wins path.
        assert_eq!(resolve_repo(Some("o/r")).unwrap(), "o/r");
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct PrCommentArgs {
    /// Markdown file to post (produced by a prior `fermut run --markdown …`).
    #[arg(long)]
    pub(crate) markdown: std::path::PathBuf,

    /// `owner/repo`. Defaults to `$GITHUB_REPOSITORY` (set automatically on GHA).
    #[arg(long)]
    pub(crate) repo: Option<String>,

    /// Pull-request number. Defaults to one inferred from `$GITHUB_REF` or `$PR_NUMBER`.
    #[arg(long)]
    pub(crate) pr: Option<u64>,

    /// Override the comment marker. Defaults to `<!-- fermut:report -->`.
    /// Use to keep multiple gates (e.g. per-shard) as independent comments.
    #[arg(long)]
    pub(crate) marker: Option<String>,

    /// Print the resolved plan without contacting GitHub.
    #[arg(long)]
    pub(crate) dry_run: bool,
}
