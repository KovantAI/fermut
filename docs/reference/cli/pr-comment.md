# `fermut pr-comment`

Post a Markdown report to a pull request as a sticky comment. Edits
the previous fermut comment in place across re-runs instead of
stacking.

```sh
fermut pr-comment --markdown <path> [--repo OWNER/REPO] [--pr N] [--marker STRING] [--dry-run]
```

| Flag           | Default                                         | Effect                                                          |
|----------------|-------------------------------------------------|-----------------------------------------------------------------|
| `--markdown`   | required                                        | Markdown file to post.                                          |
| `--repo`       | `$GITHUB_REPOSITORY`                            | `owner/repo`.                                                    |
| `--pr`         | inferred from `$GITHUB_REF` or `$PR_NUMBER`     | Pull-request number.                                            |
| `--marker`     | `<!-- fermut:report -->`                        | Override to keep multiple sticky comments (e.g. per shard).      |
| `--dry-run`    | off                                             | Print the resolved plan, don't call the network.                  |

Requires `gh` on `PATH` (preinstalled on every GitHub Actions
runner; otherwise install from <https://cli.github.com>).

## How the marker works

Every `--markdown` report fermut writes begins with the marker
comment, which renders to nothing in GitHub but is searchable via
the API. `pr-comment` lists the PR's issue comments, finds the one
whose body starts with that marker, and `PATCH`es it. If none
matches it falls back to `gh pr comment` and creates a fresh one.
