//! Polls GitHub, turns new events into jobs (each at most once), and launches one detached
//! runner at a time. This thread is the only writer of the watch state; job files are written
//! by the watcher only while no runner owns them.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use crate::config::Config;
use crate::github;
use crate::state::{
    Job, JobKind, JobStatus, PrComment, PrInfo, WatchState, list_jobs, load_state, pid_alive, save_job, save_state,
};

pub enum Cmd {
    Retry(String),
    Cancel(String),
    MarkSeen(String),
    TogglePause,
    PollNow,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub state: WatchState,
    pub jobs: Vec<Job>,
    pub polling: bool,
    pub next_poll: Option<DateTime<Utc>>,
    pub messages: Vec<(DateTime<Local>, String)>,
}

pub type Shared = Arc<Mutex<Snapshot>>;

pub fn spawn(cfg: Config) -> Result<(Shared, Sender<Cmd>)> {
    let state = load_state()?;
    let shared: Shared = Arc::new(Mutex::new(Snapshot {
        state: state.clone(),
        ..Default::default()
    }));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Watcher {
        cfg,
        state,
        jobs: vec![],
        me: None,
        child: None,
        next_poll: Instant::now(),
        polling: false,
        shared: shared.clone(),
        last_status: HashMap::new(),
        messages: vec![],
        loaded: false,
        warned_blocked: HashSet::new(),
    };
    std::thread::spawn(move || w.run(rx));
    Ok((shared, tx))
}

struct Watcher {
    cfg: Config,
    state: WatchState,
    jobs: Vec<Job>,
    me: Option<String>,
    /// Runner launched by this process; reaped here.
    child: Option<(String, Child)>,
    next_poll: Instant,
    polling: bool,
    shared: Shared,
    last_status: HashMap<String, JobStatus>,
    messages: Vec<(DateTime<Local>, String)>,
    loaded: bool,
    warned_blocked: HashSet<String>,
}

impl Watcher {
    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            self.tick();
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn notify(&mut self, msg: String) {
        if self.cfg.desktop_notifications && cfg!(target_os = "macos") {
            let script = format!("display notification {:?} with title \"pr-watch\"", msg);
            std::thread::spawn(move || {
                let _ = Command::new("osascript")
                    .args(["-e", &script])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            });
        }
        self.messages.push((Local::now(), msg));
        let excess = self.messages.len().saturating_sub(100);
        self.messages.drain(..excess);
    }

    fn publish(&self) {
        let mut s = self.shared.lock().unwrap();
        s.state = self.state.clone();
        s.jobs = self.jobs.clone();
        s.polling = self.polling;
        s.next_poll = Some(Utc::now() + (self.next_poll.saturating_duration_since(Instant::now())));
        s.messages = self.messages.clone();
    }

    fn save(&mut self) {
        if let Err(e) = save_state(&self.state) {
            self.notify(format!("saving state failed: {e:#}"));
        }
    }

    fn save_job(&mut self, job: &Job) {
        if let Err(e) = save_job(job) {
            self.notify(format!("saving job {} failed: {e:#}", job.id));
        }
    }

    fn runner_alive(&self, job: &Job) -> bool {
        self.child.as_ref().is_some_and(|(id, _)| id == &job.id) || job.runner_pid.is_some_and(pid_alive)
    }

    fn tick(&mut self) {
        if let Some((_, child)) = &mut self.child
            && !matches!(child.try_wait(), Ok(None))
        {
            self.child = None;
        }
        self.jobs = list_jobs();

        let dead: Vec<Job> = self
            .jobs
            .iter()
            .filter(|j| j.status.is_active() && !self.runner_alive(j))
            .cloned()
            .collect();
        for mut job in dead {
            job.set(JobStatus::Interrupted, Some("runner exited unexpectedly".into()));
            self.save_job(&job);
        }
        if !self.jobs.is_empty() {
            self.jobs = list_jobs();
        }
        self.report_transitions();

        if Instant::now() >= self.next_poll {
            self.poll();
        }
        self.schedule();
        self.publish();
    }

    fn report_transitions(&mut self) {
        let changes: Vec<(Job, Option<JobStatus>)> = self
            .jobs
            .iter()
            .filter(|j| self.last_status.get(&j.id) != Some(&j.status))
            .map(|j| (j.clone(), self.last_status.get(&j.id).copied()))
            .collect();
        for (job, _) in &changes {
            self.last_status.insert(job.id.clone(), job.status);
        }
        if !self.loaded {
            // Do not replay history on startup.
            self.loaded = true;
            return;
        }
        for (job, _) in changes {
            let what = format!("{} ({})", job.short(), job.kind.label());
            let note = job.note.clone().unwrap_or_default();
            match job.status {
                JobStatus::Done => self.notify(format!("patch ready for {what}: {note}")),
                JobStatus::NoFix => self.notify(format!("triaged {what}, no code change proposed")),
                JobStatus::Failed => self.notify(format!("agent failed for {what}: {note}")),
                JobStatus::Interrupted => self.notify(format!("{what} interrupted: {note}")),
                JobStatus::Blocked if self.warned_blocked.insert(job.id.clone()) => {
                    self.notify(format!("{what} waits for a clean worktree"))
                }
                _ => {}
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::PollNow => self.next_poll = Instant::now(),
            Cmd::TogglePause => {
                self.state.paused = !self.state.paused;
                self.save();
            }
            Cmd::Retry(id) => {
                if let Some(mut job) = self.jobs.iter().find(|j| j.id == id).cloned()
                    && !job.status.is_active()
                    && job.status != JobStatus::Queued
                {
                    job.set(JobStatus::Queued, Some("retry requested".into()));
                    self.save_job(&job);
                }
            }
            Cmd::Cancel(id) => {
                let Some(mut job) = self.jobs.iter().find(|j| j.id == id).cloned() else {
                    return;
                };
                if job.status.is_active() {
                    let pid = job
                        .runner_pid
                        .or(self.child.as_ref().filter(|(cid, _)| cid == &id).map(|(_, c)| c.id()));
                    if let Some(pid) = pid {
                        // The runner leads its own process group, which includes pi.
                        // SAFETY: plain signal to a process group we created.
                        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
                    }
                } else if matches!(job.status, JobStatus::Queued | JobStatus::Blocked) {
                    job.set(JobStatus::Skipped, Some("skipped by user".into()));
                    self.save_job(&job);
                }
            }
            Cmd::MarkSeen(id) => {
                if let Some(mut job) = self.jobs.iter().find(|j| j.id == id).cloned()
                    && !job.seen
                    && !job.status.is_active()
                {
                    job.seen = true;
                    self.save_job(&job);
                }
            }
        }
        self.tick();
    }

    fn poll(&mut self) {
        self.polling = true;
        self.publish();
        let mut errors = vec![];
        if let Err(e) = self.poll_inner(&mut errors) {
            errors.push(format!("{e:#}"));
        }
        self.state.last_error = (!errors.is_empty()).then(|| errors.join("; ").chars().take(500).collect());
        self.polling = false;
        self.next_poll = Instant::now() + Duration::from_secs(self.cfg.poll_interval_sec);
        self.save();
    }

    fn poll_inner(&mut self, errors: &mut Vec<String>) -> Result<()> {
        if let Ok(cfg) = Config::load() {
            self.cfg = cfg;
        }
        if self.me.is_none() {
            self.me = Some(github::current_user()?);
        }
        let me = self.me.clone().unwrap_or_default();
        let mut prs = vec![];
        for mut pr in github::list_my_prs()? {
            pr.local = self.cfg.local_repo(&pr.repo);
            if pr.local.is_none() {
                prs.push(pr);
                continue;
            }
            match github::pr_details(&pr, &self.cfg, &me) {
                Ok((info, comments)) => {
                    self.consider_ci(&info);
                    self.consider_comments(&info, &comments);
                    prs.push(info);
                }
                Err(e) => {
                    errors.push(format!("{}#{}: {e:#}", pr.repo, pr.number));
                    prs.push(pr);
                }
            }
        }
        prs.sort_by(|a, b| a.repo.cmp(&b.repo).then(b.number.cmp(&a.number)));
        self.state.prs = prs;
        self.state.last_poll = Some(Utc::now());
        self.state.baseline_done = true;
        for job in self.jobs.clone() {
            if job.status == JobStatus::Blocked {
                let mut job = job;
                job.set(JobStatus::Queued, Some("re-checking worktree".into()));
                self.save_job(&job);
            }
        }
        self.jobs = list_jobs();
        Ok(())
    }

    fn new_job(&mut self, kind: JobKind, pr: &PrInfo, keys: Vec<String>, f: impl FnOnce(&mut Job)) -> Job {
        let now = Utc::now();
        let name = pr.repo.rsplit('/').next().unwrap_or("repo");
        let mut id = format!("{}-{name}-{}-{}", now.format("%Y%m%d-%H%M%S"), pr.number, kind.label());
        while self.jobs.iter().any(|j| j.id == id) || crate::state::job_dir(&id).exists() {
            id.push('x');
        }
        let mut job = Job {
            id: id.clone(),
            kind,
            repo: pr.repo.clone(),
            number: pr.number,
            title: pr.title.clone(),
            url: pr.url.clone(),
            head_ref: pr.head_ref.clone().unwrap_or_default(),
            head_sha: pr.head_sha.clone().unwrap_or_default(),
            base_ref: pr.base_ref.clone(),
            status: JobStatus::Queued,
            note: None,
            created_at: now,
            updated_at: now,
            failed_checks: vec![],
            comments: vec![],
            worktree: None,
            session_id: None,
            verdict: None,
            patch: None,
            patch_stat: None,
            cumulative_patch: None,
            forked_from: None,
            attempt: 0,
            seen: true,
            runner_pid: None,
        };
        f(&mut job);
        // Record the event before anything can run, so a crash never repeats it.
        for k in keys {
            self.state.handled.insert(k, id.clone());
        }
        self.save();
        self.save_job(&job);
        self.jobs.push(job.clone());
        job
    }

    fn consider_ci(&mut self, pr: &PrInfo) {
        let Some(sha) = &pr.head_sha else { return };
        if pr.ci != crate::state::CiState::Failure || (self.cfg.wait_for_ci_completion && pr.ci_pending) {
            return;
        }
        let key = format!("ci:{}#{}@{sha}", pr.repo, pr.number);
        if self.state.handled.contains_key(&key) {
            return;
        }
        let baseline = !self.state.baseline_done;
        let checks = pr.failed_checks.clone();
        self.new_job(JobKind::Ci, pr, vec![key], |j| {
            j.failed_checks = checks;
            if baseline {
                j.status = JobStatus::Skipped;
                j.note = Some("was already failing when pr-watch first started; press r to run".into());
            }
        });
        if !baseline {
            self.notify(format!("CI failed on {}#{}; queued triage", pr.repo, pr.number));
        }
    }

    fn consider_comments(&mut self, pr: &PrInfo, comments: &[PrComment]) {
        if pr.head_sha.is_none() {
            return;
        }
        let key = |c: &PrComment| format!("comment:{}#{}:{}", pr.repo, pr.number, c.id);
        let fresh: Vec<PrComment> = comments
            .iter()
            .filter(|c| c.created_at >= self.state.since && !self.state.handled.contains_key(&key(c)))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return;
        }
        let settle = chrono::Duration::seconds(self.cfg.comment_settle_sec);
        if fresh.iter().any(|c| Utc::now() - c.created_at < settle) {
            return;
        }
        let keys = fresh.iter().map(key).collect();
        let mut who: Vec<String> = fresh.iter().map(|c| format!("@{}", c.author)).collect();
        who.dedup();
        let n = fresh.len();
        self.new_job(JobKind::Comments, pr, keys, |j| j.comments = fresh);
        self.notify(format!(
            "{n} comment(s) from {} on {}#{}; queued triage",
            who.join(", "),
            pr.repo,
            pr.number
        ));
    }

    fn schedule(&mut self) {
        if self.state.paused || self.child.is_some() || self.jobs.iter().any(|j| j.status.is_active()) {
            return;
        }
        while let Some(mut job) = self.jobs.iter().find(|j| j.status == JobStatus::Queued).cloned() {
            if self.state.last_poll.is_some() {
                let pr = self
                    .state
                    .prs
                    .iter()
                    .find(|p| p.repo == job.repo && p.number == job.number);
                let skip = match pr {
                    None => Some("PR is no longer open"),
                    Some(p) if job.kind == JobKind::Ci && p.head_sha.as_deref().is_some_and(|s| s != job.head_sha) => {
                        Some("superseded by a newer commit")
                    }
                    _ => None,
                };
                if let Some(why) = skip {
                    job.set(JobStatus::Skipped, Some(why.into()));
                    self.save_job(&job);
                    self.jobs = list_jobs();
                    continue;
                }
            }
            if job.kind == JobKind::Comments {
                self.merge_comments(&mut job);
            }
            job.set(JobStatus::Starting, Some("starting runner".into()));
            job.runner_pid = None;
            self.save_job(&job);
            match self.launch(&job) {
                Ok(child) => self.child = Some((job.id.clone(), child)),
                Err(e) => {
                    job.set(JobStatus::Failed, Some(format!("could not start runner: {e:#}")));
                    self.save_job(&job);
                }
            }
            self.jobs = list_jobs();
            return;
        }
    }

    /// Folds other waiting comment jobs for the same PR into `job`, so they get one triage.
    fn merge_comments(&mut self, job: &mut Job) {
        let others: Vec<Job> = self
            .jobs
            .iter()
            .filter(|j| {
                j.id != job.id
                    && j.kind == JobKind::Comments
                    && j.repo == job.repo
                    && j.number == job.number
                    && matches!(j.status, JobStatus::Queued | JobStatus::Blocked)
            })
            .cloned()
            .collect();
        for mut other in others {
            for c in &other.comments {
                if !job.comments.iter().any(|x| x.id == c.id) {
                    job.comments.push(c.clone());
                }
            }
            if other.created_at > job.created_at {
                job.head_sha = other.head_sha.clone();
            }
            other.set(JobStatus::Skipped, Some(format!("merged into {}", job.id)));
            self.save_job(&other);
        }
        job.comments.sort_by_key(|c| c.created_at);
    }

    fn launch(&self, job: &Job) -> Result<Child> {
        let exe = std::env::current_exe()?;
        let err = File::create(job.dir().join("runner.err"))?;
        // Own process group: survives the TUI exiting and Ctrl-C, and can be cancelled as a unit.
        Ok(Command::new(exe)
            .args(["run-job", &job.id])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(err)
            .spawn()?)
    }
}
