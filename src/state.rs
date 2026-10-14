use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::state_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CiState {
    Success,
    Failure,
    Pending,
    None,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedCheck {
    pub name: String,
    pub workflow: Option<String>,
    pub url: Option<String>,
    /// GitHub Actions job id, when the check is an Actions job.
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrComment {
    pub id: String,
    pub kind: String,
    pub author: String,
    pub association: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub url: Option<String>,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub diff_hunk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PrInfo {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub head_ref: Option<String>,
    pub head_sha: Option<String>,
    pub base_ref: Option<String>,
    pub ci: CiState,
    pub ci_pending: bool,
    pub failed_checks: Vec<FailedCheck>,
    pub local: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    Queued,
    Blocked,
    Starting,
    Triage,
    Fixing,
    Done,
    NoFix,
    Failed,
    Skipped,
    Interrupted,
}

impl JobStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Triage | Self::Fixing)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Blocked => "blocked",
            Self::Starting => "starting",
            Self::Triage => "triage",
            Self::Fixing => "fixing",
            Self::Done => "done",
            Self::NoFix => "no-fix",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobKind {
    Ci,
    Comments,
}

impl JobKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Comments => "comments",
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: Option<String>,
    pub status: JobStatus,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub failed_checks: Vec<FailedCheck>,
    #[serde(default)]
    pub comments: Vec<PrComment>,
    pub worktree: Option<PathBuf>,
    pub session_id: Option<String>,
    pub verdict: Option<String>,
    pub patch: Option<PathBuf>,
    pub patch_stat: Option<String>,
    /// All uncommitted pr-watch changes in the worktree, when earlier jobs left some.
    #[serde(default)]
    pub cumulative_patch: Option<PathBuf>,
    /// Job whose pi session this job's session was forked from.
    #[serde(default)]
    pub forked_from: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    /// Result has been opened in the UI.
    #[serde(default = "yes")]
    pub seen: bool,
    pub runner_pid: Option<u32>,
}

impl Job {
    pub fn dir(&self) -> PathBuf {
        job_dir(&self.id)
    }

    pub fn short(&self) -> String {
        format!("{}#{}", self.repo.rsplit('/').next().unwrap_or(&self.repo), self.number)
    }

    pub fn set(&mut self, status: JobStatus, note: Option<String>) {
        self.status = status;
        self.note = note;
        self.updated_at = Utc::now();
    }
}

/// Watcher-owned state. Job files are owned by whichever process runs the job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchState {
    /// Comments older than this are never acted on.
    pub since: DateTime<Utc>,
    pub baseline_done: bool,
    pub paused: bool,
    /// Event key -> job id. An event key becomes a job at most once.
    pub handled: BTreeMap<String, String>,
    pub prs: Vec<PrInfo>,
    pub last_poll: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            since: Utc::now(),
            baseline_done: false,
            paused: false,
            handled: BTreeMap::new(),
            prs: vec![],
            last_poll: None,
            last_error: None,
        }
    }
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

pub fn job_dir(id: &str) -> PathBuf {
    state_dir().join("jobs").join(id)
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_state() -> Result<WatchState> {
    let path = state_path();
    if !path.exists() {
        return Ok(WatchState::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_state(state: &WatchState) -> Result<()> {
    atomic_write(&state_path(), serde_json::to_string_pretty(state)?.as_bytes())
}

pub fn load_job(id: &str) -> Result<Job> {
    let path = job_dir(id).join("job.json");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_job(job: &Job) -> Result<()> {
    atomic_write(
        &job.dir().join("job.json"),
        serde_json::to_string_pretty(job)?.as_bytes(),
    )
}

/// All jobs, oldest first.
pub fn list_jobs() -> Vec<Job> {
    let Ok(entries) = std::fs::read_dir(state_dir().join("jobs")) else {
        return vec![];
    };
    let mut jobs: Vec<Job> = entries
        .flatten()
        .filter_map(|e| load_job(&e.file_name().to_string_lossy()).ok())
        .collect();
    jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    jobs
}

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Holds the single-watcher lock for as long as the returned file is alive.
pub fn acquire_watcher_lock() -> Result<Option<File>> {
    std::fs::create_dir_all(state_dir())?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_dir().join("watcher.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

pub fn tail_file(path: &Path, max_bytes: u64) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Drop the partial first line.
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    }
}
