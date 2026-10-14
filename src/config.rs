use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// GitHub `owner/name` -> local checkout managed by wt. Other repos are listed but ignored.
    pub repos: BTreeMap<String, String>,
    pub poll_interval_sec: u64,
    /// Comments from these author associations start a job.
    pub member_associations: Vec<String>,
    pub ignore_authors: Vec<String>,
    /// Wait until a review has been quiet this long so its comments land in one job.
    pub comment_settle_sec: i64,
    /// Only triage CI once no checks are pending for the head commit.
    pub wait_for_ci_completion: bool,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub pi_command: String,
    pub pi_args: Vec<String>,
    pub phase_timeout_min: u64,
    pub desktop_notifications: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repos: BTreeMap::from([
                ("duckdb/duckdb".into(), "~/duckdb".into()),
                ("duckdb/duckdb-rs".into(), "~/duckdb-rs".into()),
            ]),
            poll_interval_sec: 300,
            member_associations: vec!["OWNER".into(), "MEMBER".into(), "COLLABORATOR".into()],
            ignore_authors: vec![],
            comment_settle_sec: 120,
            wait_for_ci_completion: false,
            model: None,
            thinking: None,
            pi_command: "pi".into(),
            pi_args: vec![],
            phase_timeout_min: 90,
            desktop_notifications: true,
        }
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home().join(rest.trim_start_matches('/')),
        _ => PathBuf::from(p),
    }
}

pub fn config_path() -> PathBuf {
    std::env::var("PR_WATCH_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config/pr-watch/config.json"))
}

pub fn state_dir() -> PathBuf {
    std::env::var("PR_WATCH_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state/pr-watch"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            let cfg = Self::default();
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
            std::fs::write(&path, serde_json::to_string_pretty(&cfg)? + "\n")?;
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn local_repo(&self, repo: &str) -> Option<PathBuf> {
        self.repos.get(repo).map(|p| expand_home(p))
    }
}
