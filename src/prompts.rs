use std::path::Path;

use crate::state::{Job, JobKind};

pub const RULES: &str = "pr-watch rules (unattended background run, nobody is available to answer questions):
- Never write to GitHub: no comments, reviews, pushes, PR edits, labels, or CI re-runs. The gh CLI is read-only here.
- Never commit, stash, reset, checkout, rebase, or switch branches. Leave all changes as uncommitted edits in the worktree.
- Where your instructions say to ask the user first, take the cheaper safe option instead: build incrementally in an existing build directory and only the targets you need; never delete build directories or start a from-scratch build of a large project. If verification would need such a build, skip it and say so.
- Keep the diff minimal and focused on the problem.";

pub struct LogRef {
    pub check: String,
    pub path: Option<String>,
    pub url: Option<String>,
}

/// How this job relates to earlier pr-watch work on the same PR.
pub struct Continuation<'a> {
    pub forked_from: Option<&'a str>,
    /// The worktree already holds uncommitted changes from earlier jobs.
    pub carried: bool,
    pub dropped: Option<&'a Path>,
}

fn header(job: &Job, worktree: &Path, local_head: &str, ctx: &Continuation) -> String {
    let base = job.base_ref.as_deref().map(|b| format!(" -> {b}")).unwrap_or_default();
    let mut s = format!(
        "PR {}#{} \"{}\" ({})\nBranch {}{base}. You work in pr-watch's own worktree {} (local branch pr-watch/{}, following {}).\nPR head commit: {}. Local HEAD: {local_head}.",
        job.repo,
        job.number,
        job.title,
        job.url,
        job.head_ref,
        worktree.display(),
        job.number,
        job.head_ref,
        job.head_sha
    );
    if !job.head_sha.starts_with(local_head) {
        s.push_str("\nThe local branch differs from the PR head; take that into account (do not pull or reset).");
    }
    if let Some(id) = ctx.forked_from {
        s.push_str(&format!(
            "\nThis session continues from pr-watch job {id} for this PR; earlier messages may refer to another worktree."
        ));
    }
    if ctx.carried {
        s.push_str("\nThe worktree already holds uncommitted changes from earlier pr-watch jobs for this PR (see `git status` and `git diff`). Build on them; only revert them if the new information calls for it.");
    }
    if let Some(p) = ctx.dropped {
        s.push_str(&format!(
            "\nThe PR branch moved and earlier pr-watch changes no longer merged cleanly, so they were dropped (saved in {}).",
            p.display()
        ));
    }
    s
}

fn quote(text: &str) -> String {
    text.trim()
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_comments(job: &Job) -> String {
    job.comments
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let place = match (&c.path, c.line) {
                (Some(p), Some(l)) => format!(" on {p}:{l}"),
                (Some(p), None) => format!(" on {p}"),
                _ => String::new(),
            };
            let hunk = c
                .diff_hunk
                .as_deref()
                .map(|h| {
                    let lines: Vec<&str> = h.lines().collect();
                    format!(
                        "\nDiff context:\n```diff\n{}\n```",
                        lines[lines.len().saturating_sub(12)..].join("\n")
                    )
                })
                .unwrap_or_default();
            format!(
                "### Comment {}: @{} ({}, {}){place}\n{}{hunk}\n{}",
                i + 1,
                c.author,
                c.association,
                c.kind,
                c.url.as_deref().unwrap_or(""),
                quote(&c.body)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn triage(
    job: &Job,
    worktree: &Path,
    local_head: &str,
    triage: &Path,
    reply: &Path,
    logs: &[LogRef],
    ctx: &Continuation,
) -> String {
    let triage = triage.display();
    let verdict = format!(
        "End {triage} with exactly one final line: `VERDICT: FIX` if a code change in this PR is needed, otherwise `VERDICT: NO_FIX`."
    );
    let head = header(job, worktree, local_head, ctx);
    match job.kind {
        JobKind::Ci => {
            let checks = logs
                .iter()
                .map(|l| {
                    let url = l.url.as_deref().map(|u| format!(" ({u})")).unwrap_or_default();
                    let log = l.path.as_deref().map(|p| format!("full job log: {p}")).unwrap_or("log not downloaded".into());
                    format!("- {}{url}\n  {log}", l.check)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let base = job.base_ref.as_deref().unwrap_or("the base branch");
            format!(
                "{head}

CI failed for the PR head commit. Failed checks:
{checks}

Phase 1 of 2: triage only. Do not modify files in the worktree yet.
1. Find the actual error in each log (they are large; use rg and targeted reads, not full reads).
2. Look at the relevant code and the PR's changes (`gh pr diff {n} -R {repo}`, or git against the merge base with {base}).
3. Decide per failure: caused by this PR, flaky/infrastructure, or broken upstream/unrelated.

Write the triage to {triage} (markdown): summary, root cause per failure with evidence (log excerpts, file:line), and the concrete fix you propose.
{verdict}",
                n = job.number,
                repo = job.repo,
            )
        }
        JobKind::Comments => format!(
            "{head}

Repository members left this feedback on the PR:

{comments}

Phase 1 of 2: triage only. Do not modify files in the worktree yet.
For each comment decide: needs a code change, is a question that needs a reply, or needs no action. Read the code the comment refers to before deciding.

Write the triage to {triage} (markdown): per comment, your assessment and the concrete change you propose.
For comments that need a reply, draft replies in {reply} (they are never posted automatically).
{verdict}",
            comments = describe_comments(job),
            reply = reply.display(),
        ),
    }
}

pub fn fix(triage: &Path) -> String {
    let triage = triage.display();
    format!(
        "Phase 2 of 2: implement the fix you proposed in {triage} in this worktree.
- Keep the change minimal; follow the repository's conventions.
- Verify with the narrowest relevant build and tests when an existing build directory makes that feasible (see the rules), and run the formatter on changed files if it is cheap.
- Do not commit. The uncommitted diff becomes the proposed patch.
When done, append a \"## Fix\" section to {triage} describing the change and exactly what you verified (and what you did not)."
    )
}
