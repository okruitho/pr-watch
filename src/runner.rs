//! Runs one job end to end. Started by the watcher as a detached `pr-watch run-job <id>` process
//! so that agents survive the TUI exiting.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use serde_json::Value;

use crate::config::{Config, home, state_dir};
use crate::github::fetch_job_log;
use crate::proc::{run, run_ok, run_ok_env};
use crate::prompts::{self, LogRef};
use crate::state::{Job, JobStatus, list_jobs, load_job, save_job, tail_file};

fn shim_dir() -> PathBuf {
    state_dir().join("bin")
}

const GH_SHIM: &str = r#"#!/bin/sh
# Read-only gh for pr-watch agents.
deny() { echo "pr-watch: 'gh $*' is blocked; agents may only read from GitHub" >&2; exit 1; }
case "$1" in
  run) case "$2" in view|list|download) ;; *) deny "$@" ;; esac ;;
  pr) case "$2" in view|diff|checks|list|status) ;; *) deny "$@" ;; esac ;;
  issue|repo|release|workflow) case "$2" in view|list) ;; *) deny "$@" ;; esac ;;
  search|status|version|--version|help|--help) ;;
  api)
    for a in "$@"; do
      case "$a" in
        -X*|--method*|-f|-F|--field*|--raw-field*|--input*|graphql) deny "$@" ;;
      esac
    done ;;
  *) deny "$@" ;;
esac
exec "@REAL@" "$@"
"#;

const GIT_SHIM: &str = r#"#!/bin/sh
# Blocks history- and branch-changing git commands for pr-watch agents.
sub=""; next=""; skip=0
for a in "$@"; do
  if [ "$skip" = 1 ]; then skip=0; continue; fi
  if [ -n "$sub" ]; then next="$a"; break; fi
  case "$a" in
    -C|-c|--git-dir|--work-tree|--namespace) skip=1 ;;
    -*) ;;
    *) sub="$a" ;;
  esac
done
[ "$sub $next" = "worktree list" ] && exec "@REAL@" "$@"
case "$sub" in
  push|commit|reset|rebase|stash|clean|checkout|switch|restore|merge|pull|am|cherry-pick|revert|worktree|tag)
    echo "pr-watch: 'git $sub' is blocked; leave changes uncommitted in the worktree" >&2; exit 1 ;;
esac
exec "@REAL@" "$@"
"#;

pub fn ensure_shims() -> Result<()> {
    let dir = shim_dir();
    std::fs::create_dir_all(&dir)?;
    for (name, script) in [("gh", GH_SHIM), ("git", GIT_SHIM)] {
        let real = run_ok("sh", &["-c", &format!("command -v {name}")], None)?
            .trim()
            .to_string();
        if real.is_empty() || real.starts_with(dir.to_string_lossy().as_ref()) {
            bail!("cannot find the real {name}");
        }
        let path = dir.join(name);
        std::fs::write(&path, script.replace("@REAL@", &real))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub fn main(id: &str) -> Result<()> {
    let term = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(sig, term.clone())?;
    }
    let cfg = Config::load()?;
    let mut job = load_job(id)?;
    job.runner_pid = Some(std::process::id());
    let mut r = Runner {
        log_path: job.dir().join("agent.log"),
        job,
        cfg,
        term,
        fork_from: None,
    };
    r.save();
    if let Err(e) = r.run() {
        if r.term.load(Ordering::SeqCst) {
            r.set(JobStatus::Interrupted, Some("cancelled".into()));
        } else {
            let msg = format!("{e:#}");
            r.set(JobStatus::Failed, Some(msg.chars().take(300).collect()));
        }
    }
    Ok(())
}

struct Runner {
    job: Job,
    cfg: Config,
    term: Arc<AtomicBool>,
    log_path: PathBuf,
    /// Session to fork the job's session from on the next pi start.
    fork_from: Option<PathBuf>,
}

impl Runner {
    fn save(&self) {
        if let Err(e) = save_job(&self.job) {
            eprintln!("saving job: {e:#}");
        }
    }

    fn log(&mut self, line: &str) {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.log_path) {
            let _ = writeln!(f, "[{}] {line}", Local::now().format("%H:%M:%S"));
        }
        self.job.note = Some(line.lines().next().unwrap_or("").chars().take(200).collect());
        self.save();
    }

    fn set(&mut self, status: JobStatus, note: Option<String>) {
        self.log(&format!(
            "status: {}{}",
            status.label(),
            note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default()
        ));
        self.job.set(status, note);
        self.save();
    }

    fn run(&mut self) -> Result<()> {
        let Some(repo_path) = self.cfg.local_repo(&self.job.repo) else {
            self.set(JobStatus::Skipped, Some("no local checkout configured".into()));
            return Ok(());
        };
        ensure_shims()?;

        let wt = self.agent_worktree(&repo_path)?;
        self.job.worktree = Some(wt.clone());
        self.save();

        let index = self.job.dir().join("snapshot.index");
        let dropped = self.sync_worktree(&wt, &index)?;
        let start_sha = rev_parse(&wt, "HEAD")?;
        let start_tree = snapshot(&wt, &index)?;
        let carried = start_tree != rev_parse(&wt, "HEAD^{tree}")?;
        self.set(JobStatus::Triage, Some("collecting context".into()));

        let dir = self.job.dir();
        let mut logs = vec![];
        for c in self.job.failed_checks.clone() {
            let check = match &c.workflow {
                Some(w) => format!("{w} / {}", c.name),
                None => c.name.clone(),
            };
            let Some(job_id) = &c.job_id else {
                logs.push(LogRef {
                    check,
                    path: None,
                    url: c.url,
                });
                continue;
            };
            let safe: String = c
                .name
                .chars()
                .map(|ch| {
                    if ch.is_alphanumeric() || ".-".contains(ch) {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            let path = dir.join("logs").join(format!("{job_id}-{safe}.log"));
            let res = fetch_job_log(&self.job.repo, job_id).and_then(|text| {
                std::fs::create_dir_all(dir.join("logs"))?;
                Ok(std::fs::write(&path, text)?)
            });
            match res {
                Ok(()) => logs.push(LogRef {
                    check,
                    path: Some(path.display().to_string()),
                    url: c.url,
                }),
                Err(e) => {
                    self.log(&format!("could not download log for {check}: {e:#}"));
                    logs.push(LogRef {
                        check,
                        path: None,
                        url: c.url,
                    });
                }
            }
        }

        let triage = dir.join("triage.md");
        let reply = dir.join("reply.md");
        self.job.attempt += 1;
        if self.job.attempt > 1 {
            let n = self.job.attempt - 1;
            for (f, renamed) in [
                (&triage, format!("triage.attempt{n}.md")),
                (&reply, format!("reply.attempt{n}.md")),
                (&dir.join("fix.patch"), format!("fix.attempt{n}.patch")),
                (&dir.join("cumulative.patch"), format!("cumulative.attempt{n}.patch")),
            ] {
                if f.exists() {
                    std::fs::rename(f, dir.join(renamed))?;
                }
            }
            self.job.patch = None;
            self.job.patch_stat = None;
            self.job.cumulative_patch = None;
            self.job.verdict = None;
        }
        self.job.session_id = Some(format!("pr-watch-{}-{}", self.job.id, self.job.attempt));
        let fork = self.previous_session();
        self.job.forked_from = fork.as_ref().map(|(id, _)| id.clone());
        self.fork_from = fork.map(|(_, path)| path);

        let local_head = &start_sha[..start_sha.len().min(12)];
        let ctx = prompts::Continuation {
            forked_from: self.job.forked_from.as_deref(),
            carried,
            dropped: dropped.as_deref(),
        };
        let prompt = prompts::triage(&self.job, &wt, local_head, &triage, &reply, &logs, &ctx);
        self.run_pi("triage", &prompt, &wt)?;

        let text = std::fs::read_to_string(&triage).context("agent did not write triage.md")?;
        let verdict = text.lines().rev().find_map(|l| {
            let l = l.trim().trim_matches('`').trim();
            l.strip_prefix("VERDICT:").map(|v| v.trim().to_string())
        });
        self.job.verdict = Some(verdict.clone().unwrap_or_else(|| "NO_FIX".into()));

        if verdict.as_deref() == Some("FIX") {
            self.set(JobStatus::Fixing, Some("implementing the proposed fix".into()));
            self.run_pi("fix", &prompts::fix(&triage), &wt)?;
        }
        match self.save_patch(&wt, &start_sha, &start_tree, &index)? {
            Some(stat) => {
                self.job.seen = false;
                self.set(JobStatus::Done, Some(stat));
            }
            None => {
                let note = if verdict.is_some() {
                    "triage only, no code change"
                } else {
                    "no VERDICT line; treated as NO_FIX"
                };
                self.set(JobStatus::NoFix, Some(note.into()));
            }
        }
        Ok(())
    }

    fn run_pi(&mut self, phase: &str, prompt: &str, wt: &Path) -> Result<()> {
        let dir = self.job.dir();
        std::fs::write(dir.join(format!("prompt-{phase}.md")), prompt)?;
        let sid = self.job.session_id.clone().unwrap_or_default();
        let name = format!(
            "pr-watch {}#{} {}",
            self.job.repo,
            self.job.number,
            self.job.kind.label()
        );
        let mut args: Vec<String> = [
            "--mode",
            "json",
            "--session-id",
            &sid,
            "--name",
            &name,
            "--append-system-prompt",
            prompts::RULES,
        ]
        .map(String::from)
        .to_vec();
        if let Some(src) = self.fork_from.take() {
            args.extend(["--fork".into(), src.display().to_string()]);
        }
        if let Some(m) = &self.cfg.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(t) = &self.cfg.thinking {
            args.extend(["--thinking".into(), t.clone()]);
        }
        args.extend(self.cfg.pi_args.iter().cloned());
        args.push(prompt.into());

        let stderr_path = dir.join(format!("pi-{phase}.stderr"));
        let path = format!("{}:{}", shim_dir().display(), std::env::var("PATH").unwrap_or_default());
        let mut child = Command::new(&self.cfg.pi_command)
            .args(&args)
            .current_dir(wt)
            .env("PATH", path)
            .env("PR_WATCH_CHILD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(&stderr_path)?)
            .spawn()
            .with_context(|| format!("starting {}", self.cfg.pi_command))?;

        // 1 = cancelled, 2 = timed out.
        let reason = Arc::new(AtomicU8::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let (reason, finished, term) = (reason.clone(), finished.clone(), self.term.clone());
            let pid = child.id() as libc::pid_t;
            let deadline = Instant::now() + Duration::from_secs(self.cfg.phase_timeout_min * 60);
            std::thread::spawn(move || {
                while !finished.load(Ordering::SeqCst) {
                    let why = if term.load(Ordering::SeqCst) {
                        1
                    } else if Instant::now() > deadline {
                        2
                    } else {
                        0
                    };
                    if why != 0 && reason.load(Ordering::SeqCst) == 0 {
                        reason.store(why, Ordering::SeqCst);
                        // SAFETY: plain signal to our own child.
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            })
        };

        let mut events = File::create(dir.join(format!("events-{phase}.jsonl")))?;
        let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut buf = Vec::new();
        let mut last_stop: Option<String> = None;
        let mut last_error: Option<String> = None;
        // JSONL records are split on LF only; strings may contain other line separators.
        while {
            buf.clear();
            reader.read_until(b'\n', &mut buf)? > 0
        } {
            events.write_all(&buf)?;
            let Ok(ev) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            self.handle_event(&ev, &mut last_stop, &mut last_error);
        }
        let status = child.wait()?;
        finished.store(true, Ordering::SeqCst);
        let _ = watchdog.join();

        if reason.load(Ordering::SeqCst) == 1 || self.term.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        if reason.load(Ordering::SeqCst) == 2 {
            bail!("{phase} timed out after {} min", self.cfg.phase_timeout_min);
        }
        if !status.success() {
            let err = tail_file(&stderr_path, 2000);
            bail!("pi exited with {status}: {}", err.trim().lines().last().unwrap_or(""));
        }
        if matches!(last_stop.as_deref(), Some("error" | "aborted")) {
            bail!(
                "{}",
                last_error.unwrap_or_else(|| format!("agent stopped: {}", last_stop.unwrap_or_default()))
            );
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Value, last_stop: &mut Option<String>, last_error: &mut Option<String>) {
        match ev["type"].as_str().unwrap_or_default() {
            "tool_execution_start" => {
                let tool = ev["toolName"].as_str().unwrap_or("?");
                let a = &ev["args"];
                let detail = if tool == "bash" {
                    a["command"].as_str()
                } else {
                    a["path"].as_str().or(a["pattern"].as_str()).or(a["query"].as_str())
                };
                let detail: String = detail.unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
                self.log(&format!("→ {tool} {}", detail.chars().take(200).collect::<String>()));
            }
            "tool_execution_end" if ev["isError"].as_bool() == Some(true) => {
                self.log(&format!("  ✗ {} failed", ev["toolName"].as_str().unwrap_or("?")));
            }
            "message_end" if ev["message"]["role"] == "assistant" => {
                let m = &ev["message"];
                let text: Vec<&str> = m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                let text = text.join("\n");
                let text = text.trim();
                if !text.is_empty() {
                    let short: String = text.chars().take(1500).collect();
                    self.log(&short);
                }
                *last_stop = m["stopReason"].as_str().map(String::from);
                *last_error = m["errorMessage"].as_str().map(String::from);
            }
            "auto_retry_start" => {
                self.log(&format!(
                    "retrying after error: {}",
                    ev["errorMessage"].as_str().unwrap_or("")
                ));
            }
            "compaction_start" => self.log("compacting context"),
            _ => {}
        }
    }

    /// pr-watch's own worktree for this PR, so agent edits never mix with the user's.
    fn agent_worktree(&mut self, repo_path: &Path) -> Result<PathBuf> {
        let branch = format!("pr-watch/{}", self.job.number);
        if let Some(wt) = find_worktree(repo_path, &branch)? {
            return Ok(wt);
        }
        let repo = repo_path.to_string_lossy().to_string();
        let mut args = vec!["-C".to_string(), repo, "switch".into()];
        if branch_exists(repo_path, &branch)? {
            args.push(branch.clone());
        } else {
            let base = if !self.job.head_ref.is_empty() && branch_exists(repo_path, &self.job.head_ref)? {
                self.job.head_ref.clone()
            } else {
                format!("pr:{}", self.job.number)
            };
            self.log(&format!("creating worktree {branch} from {base}"));
            args.extend(["--create".into(), branch.clone(), "--base".into(), base]);
        }
        args.extend(["--no-cd".into(), "-y".into()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_ok("wt", &args, None)?;
        find_worktree(repo_path, &branch)?.ok_or_else(|| anyhow!("wt created no worktree for {branch}"))
    }

    /// Moves the worktree to the latest local commit of the PR branch, carrying earlier
    /// pr-watch changes along when they merge cleanly. Returns the patch of dropped changes.
    fn sync_worktree(&mut self, wt: &Path, index: &Path) -> Result<Option<PathBuf>> {
        if self.job.head_ref.is_empty() {
            return Ok(None);
        }
        let Ok(target) = rev_parse(wt, &format!("refs/heads/{}^{{commit}}", self.job.head_ref)) else {
            return Ok(None);
        };
        let head = rev_parse(wt, "HEAD")?;
        if head == target {
            return Ok(None);
        }
        let snap = snapshot(wt, index)?;
        let mut dropped = None;
        let mut merged = None;
        if snap != rev_parse(wt, "HEAD^{tree}")? {
            let env = [
                ("GIT_AUTHOR_NAME", OsStr::new("pr-watch")),
                ("GIT_AUTHOR_EMAIL", OsStr::new("pr-watch@localhost")),
                ("GIT_COMMITTER_NAME", OsStr::new("pr-watch")),
                ("GIT_COMMITTER_EMAIL", OsStr::new("pr-watch@localhost")),
            ];
            let commit = run_ok_env(
                "git",
                &["commit-tree", &snap, "-p", &head, "-m", "pr-watch"],
                Some(wt),
                &env,
            )?;
            let out = run(
                "git",
                &[
                    "merge-tree",
                    "--write-tree",
                    "--merge-base",
                    &head,
                    &target,
                    commit.trim(),
                ],
                Some(wt),
            )?;
            match out.code {
                0 => merged = Some(out.stdout.lines().next().unwrap_or("").to_string()),
                1 => {
                    let path = self.job.dir().join("dropped.patch");
                    std::fs::write(&path, diff(wt, &head, &snap)?)?;
                    dropped = Some(path);
                }
                _ => bail!("git merge-tree failed: {}", out.stderr.trim()),
            }
        }
        // Track every file first, so the hard reset also removes files the agent created.
        run_ok("git", &["read-tree", &snap], Some(wt))?;
        run_ok("git", &["reset", "-q", "--hard", &target], Some(wt))?;
        if let Some(tree) = &merged {
            run_ok("git", &["read-tree", "--reset", "-u", tree], Some(wt))?;
            run_ok("git", &["reset", "-q"], Some(wt))?;
        }
        let short = &target[..target.len().min(12)];
        self.log(&match (&merged, &dropped) {
            (Some(_), _) => format!("moved worktree to {short}, keeping earlier pr-watch changes"),
            (_, Some(p)) => format!(
                "moved worktree to {short}; earlier changes conflicted, saved to {}",
                p.display()
            ),
            _ => format!("moved worktree to {short}"),
        });
        Ok(dropped)
    }

    /// Latest session of another job for the same PR, to fork the new session from.
    fn previous_session(&self) -> Option<(String, PathBuf)> {
        let mut jobs: Vec<Job> = list_jobs()
            .into_iter()
            .filter(|j| j.id != self.job.id && j.repo == self.job.repo && j.number == self.job.number)
            .collect();
        jobs.sort_by_key(|j| j.updated_at);
        jobs.iter()
            .rev()
            .find_map(|j| Some((j.id.clone(), session_file(j.session_id.as_deref()?)?)))
    }

    fn save_patch(&mut self, wt: &Path, start_sha: &str, start_tree: &str, index: &Path) -> Result<Option<String>> {
        let end_tree = snapshot(wt, index)?;
        let patch = diff(wt, start_tree, &end_tree)?;
        if patch.trim().is_empty() {
            return Ok(None);
        }
        let dir = self.job.dir();
        let path = dir.join("fix.patch");
        std::fs::write(&path, &patch)?;
        if start_tree != rev_parse(wt, &format!("{start_sha}^{{tree}}"))? {
            let cumulative = dir.join("cumulative.patch");
            std::fs::write(&cumulative, diff(wt, start_sha, &end_tree)?)?;
            self.job.cumulative_patch = Some(cumulative);
        }
        let files = patch.lines().filter(|l| l.starts_with("diff --git ")).count();
        let added = patch
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++ "))
            .count();
        let removed = patch
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("--- "))
            .count();
        let stat = format!("{files} file{}, +{added} -{removed}", if files == 1 { "" } else { "s" });
        self.job.patch = Some(path);
        self.job.patch_stat = Some(stat.clone());
        Ok(Some(stat))
    }
}

fn find_worktree(repo_path: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let out: Value = serde_json::from_str(&run_ok(
        "wt",
        &["-C", &repo_path.to_string_lossy(), "list", "--format", "json"],
        None,
    )?)?;
    let items = out.get("items").unwrap_or(&out).as_array().cloned().unwrap_or_default();
    Ok(items
        .iter()
        .find(|i| i["worktree"]["path"].is_string() && i["branch"] == branch)
        .map(|i| PathBuf::from(i["worktree"]["path"].as_str().unwrap())))
}

fn branch_exists(repo_path: &Path, branch: &str) -> Result<bool> {
    let r = format!("refs/heads/{branch}");
    Ok(run("git", &["rev-parse", "--verify", "--quiet", &r], Some(repo_path))?.code == 0)
}

fn rev_parse(wt: &Path, rev: &str) -> Result<String> {
    Ok(run_ok("git", &["rev-parse", "--verify", "--quiet", rev], Some(wt))?
        .trim()
        .to_string())
}

/// Tree of the worktree as it is on disk, untracked files included, without touching the index.
fn snapshot(wt: &Path, index: &Path) -> Result<String> {
    let real = run_ok(
        "git",
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        Some(wt),
    )?;
    // Starting from the real index lets git reuse its stat cache.
    std::fs::copy(real.trim(), index)?;
    let env = [("GIT_INDEX_FILE", index.as_os_str())];
    run_ok_env("git", &["add", "-A"], Some(wt), &env)?;
    Ok(run_ok_env("git", &["write-tree"], Some(wt), &env)?.trim().to_string())
}

fn diff(wt: &Path, from: &str, to: &str) -> Result<String> {
    // Pin the format so user diff settings (prefixes, color, external tools) cannot leak in.
    run_ok(
        "git",
        &[
            "-c",
            "diff.noprefix=false",
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--binary",
            from,
            to,
        ],
        Some(wt),
    )
}

/// Session file for a pi session id, searched in pi's per-project session folders.
fn session_file(sid: &str) -> Option<PathBuf> {
    let root = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".pi/agent/sessions"));
    let suffix = format!("_{sid}.jsonl");
    let matches = |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&suffix));
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if matches(&path) {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = std::fs::read_dir(&path)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| matches(p))
        {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn git(dir: &Path, args: &[&str]) -> String {
        run_ok("git", args, Some(dir)).unwrap()
    }

    /// Commits `files` onto `branch` without checking it out.
    fn commit_on(repo: &Path, branch: &str, files: &[(&str, &str)]) {
        let index = repo.join(".git/test-index");
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        run_ok_env("git", &["read-tree", branch], Some(repo), &env).unwrap();
        for (name, body) in files {
            let path = repo.join(".git/test-blob");
            std::fs::write(&path, body).unwrap();
            let blob = git(repo, &["hash-object", "-w", &path.to_string_lossy()]);
            let info = format!("100644,{},{name}", blob.trim());
            run_ok_env(
                "git",
                &["update-index", "--add", "--cacheinfo", &info],
                Some(repo),
                &env,
            )
            .unwrap();
        }
        let tree = run_ok_env("git", &["write-tree"], Some(repo), &env).unwrap();
        let commit = git(repo, &["commit-tree", tree.trim(), "-p", branch, "-m", "user"]);
        git(repo, &["update-ref", &format!("refs/heads/{branch}"), commit.trim()]);
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn worktree_sync_and_patches() {
        let root = std::env::temp_dir().join(format!("pr-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let wt = root.join("repo");
        std::fs::create_dir_all(&wt).unwrap();
        // SAFETY: only this test reads the state dir.
        unsafe { std::env::set_var("PR_WATCH_STATE_DIR", root.join("state")) };
        git(&wt, &["init", "-q", "-b", "feat"]);
        git(&wt, &["config", "user.name", "t"]);
        git(&wt, &["config", "user.email", "t@t"]);
        std::fs::write(wt.join("a.txt"), "1\n2\n3\n4\n5\n6\n").unwrap();
        git(&wt, &["add", "a.txt"]);
        git(&wt, &["commit", "-q", "-m", "init"]);
        git(&wt, &["switch", "-q", "-c", "pr-watch/1"]);

        let job: Job = serde_json::from_value(serde_json::json!({
            "id": "t", "kind": "comments", "repo": "o/r", "number": 1, "title": "t", "url": "u",
            "headRef": "feat", "headSha": "x", "baseRef": null, "status": "triage", "note": null,
            "createdAt": Utc::now(), "updatedAt": Utc::now(), "worktree": null, "sessionId": null,
            "verdict": null, "patch": null, "patchStat": null, "runnerPid": null
        }))
        .unwrap();
        std::fs::create_dir_all(job.dir()).unwrap();
        let mut r = Runner {
            log_path: job.dir().join("agent.log"),
            job,
            cfg: Config::default(),
            term: Arc::new(AtomicBool::new(false)),
            fork_from: None,
        };
        let index = r.job.dir().join("snapshot.index");
        let start = |r: &mut Runner| {
            let dropped = r.sync_worktree(&wt, &index).unwrap();
            let sha = rev_parse(&wt, "HEAD").unwrap();
            let tree = snapshot(&wt, &index).unwrap();
            r.job.cumulative_patch = None;
            (dropped, sha, tree)
        };

        // First job: fresh tree, agent edits a tracked file and adds one.
        let (dropped, sha, tree) = start(&mut r);
        assert!(dropped.is_none());
        std::fs::write(wt.join("a.txt"), "1 agent\n2\n3\n4\n5\n6\n").unwrap();
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        assert_eq!(
            r.save_patch(&wt, &sha, &tree, &index).unwrap().as_deref(),
            Some("2 files, +2 -1")
        );
        assert!(r.job.cumulative_patch.is_none());

        // The user commits an unrelated change; the agent's edits move along.
        commit_on(&wt, "feat", &[("a.txt", "1\n2\n3\n4\n5\n6 user\n")]);
        let (dropped, sha, tree) = start(&mut r);
        assert!(dropped.is_none());
        assert_eq!(sha, rev_parse(&wt, "refs/heads/feat").unwrap());
        assert_eq!(read(&wt.join("a.txt")), "1 agent\n2\n3\n4\n5\n6 user\n");
        assert_eq!(read(&wt.join("new.txt")), "new\n");
        assert!(git(&wt, &["diff", "--cached"]).is_empty(), "changes stay unstaged");

        // Second job: fix.patch holds only its own change, cumulative.patch everything.
        std::fs::write(wt.join("a.txt"), "1 agent\n2\n3 agent\n4\n5\n6 user\n").unwrap();
        r.save_patch(&wt, &sha, &tree, &index).unwrap();
        let fix = read(&r.job.dir().join("fix.patch"));
        assert!(fix.contains("+3 agent") && !fix.contains("+1 agent") && !fix.contains("new.txt"));
        let all = read(r.job.cumulative_patch.as_ref().unwrap());
        assert!(all.contains("+1 agent") && all.contains("+3 agent") && all.contains("new.txt"));

        // The user takes the changes as they are: the worktree ends up clean.
        commit_on(
            &wt,
            "feat",
            &[("a.txt", "1 agent\n2\n3 agent\n4\n5\n6 user\n"), ("new.txt", "new\n")],
        );
        let (dropped, _, tree) = start(&mut r);
        assert!(dropped.is_none());
        assert_eq!(tree, rev_parse(&wt, "HEAD^{tree}").unwrap());

        // Conflicting user change: agent edits are dropped and saved, created files removed.
        std::fs::write(wt.join("a.txt"), "1 agent\n2 agent\n3 agent\n4\n5\n6 user\n").unwrap();
        std::fs::write(wt.join("other.txt"), "x\n").unwrap();
        commit_on(&wt, "feat", &[("a.txt", "1 agent\n2 user\n3 agent\n4\n5\n6 user\n")]);
        let (dropped, _, tree) = start(&mut r);
        let dropped = read(&dropped.expect("conflict drops changes"));
        assert!(dropped.contains("+2 agent") && dropped.contains("other.txt"));
        assert_eq!(tree, rev_parse(&wt, "refs/heads/feat^{tree}").unwrap());
        assert!(!wt.join("other.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
