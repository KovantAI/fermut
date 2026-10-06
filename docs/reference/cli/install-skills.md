# `fermut install-skills`

Write the agent skill bundled with this fermut (`fermut-mutation-testing`)
into a skills directory, so a coding agent can drive fermut's
run → triage → kill-survivors loop. The bundled copy matches the binary, so
every flag and subcommand the skill mentions exists in your version. Needs no
interpreter and runs no tests.

```sh
fermut install-skills [--user] [--agents] [--dir <DIR>] [--force]
```

```console
$ fermut install-skills
  fermut-mutation-testing: installed
fermut 0.5.0 skills in /path/to/project/.claude/skills. Claude Code picks up project and user skills live; if they don't show up, start a new session.
```

| Flag          | Effect                                                              |
|---------------|---------------------------------------------------------------------|
| (none)        | Write `./.claude/skills/`.                                          |
| `--user`      | Write `~/.claude/skills/` (`~/.agents/skills/` with `--agents`).    |
| `--agents`    | Write `.agents/skills/`, the layout Codex and other agents read.    |
| `--dir <DIR>` | Write `DIR/<skill>/`; overrides `--user` / `--agents`.              |
| `--force`     | Overwrite an installed skill that differs from the bundled copy.    |

A skill already installed with the same contents is reported up to date. One
whose files differ is left alone and the command exits `1`, unless `--force`.
`--force` overwrites only the files fermut ships; files you added to a skill
directory are kept.

Installing as a Claude Code plugin instead, and what to do when the skill
doesn't appear: **[Claude Code playbook](../../guides/claude-code-skill.md#install-the-skill)**.
