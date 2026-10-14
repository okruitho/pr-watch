# pr-watch

Watches your open GitHub PRs with `gh`. When CI fails on a PR's head commit, or repository members
comment, it runs a background `pi` agent in the PR's `wt` worktree: first a triage (`triage.md`,
draft replies in `reply.md`), then, if a code change is warranted, a fix saved as `fix.patch`.
Every event is handled at most once, and nothing is ever posted to GitHub.

```
pr-watch              watch with the TUI
pr-watch --headless   watch without a UI
pr-watch status       print PRs and jobs
```

- Config: `~/.config/pr-watch/config.json` (`PR_WATCH_CONFIG` overrides). Maps `owner/repo` to a
  local checkout; PRs in unmapped repos are listed but ignored.
- State, logs and patches: `~/.local/state/pr-watch` (`PR_WATCH_STATE_DIR` overrides).
- One watcher at a time (file lock), one agent at a time. Each job runs as a detached
  `pr-watch run-job <id>` process, so quitting the TUI does not stop it.
- On first start, CI that is already failing is recorded as skipped (press `r` to run it), and
  older comments are ignored.
- Agents run with read-only `gh` and a `git` that refuses push/commit/reset/stash/checkout etc.
  They work in pr-watch's own worktree per PR (branch `pr-watch/<number>`, created with `wt`), so
  your worktrees are never touched. Their edits stay uncommitted there and accumulate across jobs:
  each job's `fix.patch` holds only its own change, `cumulative.patch` everything so far.
- Before each job that worktree moves to the latest local commit of the PR branch; earlier
  pr-watch changes are carried along, or saved to `dropped.patch` if they conflict.
- Each job forks the pi session of the previous job for the same PR, and waiting comment jobs for
  one PR are merged into a single triage.

TUI keys: `tab` focus PRs/jobs, `↑↓` select, `enter` view triage/replies/patch, `o` open the job's
pi session (a fork while the agent is still running), `w` open the PR in the browser, `r` retry,
`x` skip/cancel, `p` pause, `u` poll now, `q` quit.
