use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::Config;
use crate::proc::run_ok;
use crate::state::{CiState, FailedCheck, PrComment, PrInfo};

fn gh_json(args: &[&str]) -> Result<Value> {
    Ok(serde_json::from_str(&run_ok("gh", args, None)?)?)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn time(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key)?.as_str()?.parse().ok()
}

pub fn current_user() -> Result<String> {
    Ok(run_ok("gh", &["api", "user", "--jq", ".login"], None)?
        .trim()
        .to_string())
}

pub fn list_my_prs() -> Result<Vec<PrInfo>> {
    let rows = gh_json(&[
        "search",
        "prs",
        "--author",
        "@me",
        "--state",
        "open",
        "--limit",
        "100",
        "--json",
        "number,repository,title,url,isDraft",
    ])?;
    Ok(rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| PrInfo {
            repo: r["repository"]["nameWithOwner"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            number: r["number"].as_u64().unwrap_or_default(),
            title: s(r, "title").unwrap_or_default(),
            url: s(r, "url").unwrap_or_default(),
            is_draft: r["isDraft"].as_bool().unwrap_or(false),
            ..Default::default()
        })
        .collect())
}

pub fn summarize_checks(rollup: &Value) -> (CiState, bool, Vec<FailedCheck>) {
    let mut failed = vec![];
    let mut pending = false;
    let checks = rollup.as_array().map(Vec::as_slice).unwrap_or_default();
    for c in checks {
        if c["__typename"] == "StatusContext" {
            match c["state"].as_str().unwrap_or_default() {
                "PENDING" | "EXPECTED" => pending = true,
                "FAILURE" | "ERROR" => failed.push(FailedCheck {
                    name: s(c, "context").unwrap_or_default(),
                    workflow: None,
                    url: s(c, "targetUrl"),
                    job_id: None,
                }),
                _ => {}
            }
            continue;
        }
        if c["status"] != "COMPLETED" {
            pending = true;
        } else if matches!(
            c["conclusion"].as_str(),
            Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE")
        ) {
            let url = s(c, "detailsUrl");
            let job_id = url.as_deref().and_then(|u| {
                let (_, rest) = u.split_once("/actions/runs/")?;
                let (_, job) = rest.split_once("/job/")?;
                Some(job.chars().take_while(char::is_ascii_digit).collect::<String>())
            });
            failed.push(FailedCheck {
                name: s(c, "name").unwrap_or_default(),
                workflow: s(c, "workflowName"),
                url,
                job_id,
            });
        }
    }
    let ci = if !failed.is_empty() {
        CiState::Failure
    } else if pending {
        CiState::Pending
    } else if checks.is_empty() {
        CiState::None
    } else {
        CiState::Success
    };
    (ci, pending, failed)
}

fn is_member(cfg: &Config, me: &str, author: Option<&str>, association: Option<&str>) -> bool {
    let Some(author) = author else { return false };
    if author == me || author.ends_with("[bot]") || cfg.ignore_authors.iter().any(|a| a == author) {
        return false;
    }
    association.is_some_and(|a| cfg.member_associations.iter().any(|m| m == a))
}

pub fn pr_details(pr: &PrInfo, cfg: &Config, me: &str) -> Result<(PrInfo, Vec<PrComment>)> {
    let number = pr.number.to_string();
    let view = gh_json(&[
        "pr",
        "view",
        &number,
        "-R",
        &pr.repo,
        "--json",
        "title,url,isDraft,headRefName,headRefOid,baseRefName,statusCheckRollup,comments,reviews",
    ])?;
    let inline = gh_json(&[
        "api",
        &format!("repos/{}/pulls/{}/comments", pr.repo, pr.number),
        "--paginate",
        "--slurp",
    ])?;

    let (ci, ci_pending, failed_checks) = summarize_checks(&view["statusCheckRollup"]);
    let info = PrInfo {
        title: s(&view, "title").unwrap_or_else(|| pr.title.clone()),
        url: s(&view, "url").unwrap_or_else(|| pr.url.clone()),
        is_draft: view["isDraft"].as_bool().unwrap_or(pr.is_draft),
        head_ref: s(&view, "headRefName"),
        head_sha: s(&view, "headRefOid"),
        base_ref: s(&view, "baseRefName"),
        ci,
        ci_pending,
        failed_checks,
        ..pr.clone()
    };

    let mut comments = vec![];
    let body_of = |v: &Value| s(v, "body").filter(|b| !b.trim().is_empty());
    for c in view["comments"].as_array().into_iter().flatten() {
        let author = c["author"]["login"].as_str();
        let assoc = c["authorAssociation"].as_str();
        let (Some(body), Some(created)) = (body_of(c), time(c, "createdAt")) else {
            continue;
        };
        if !is_member(cfg, me, author, assoc) {
            continue;
        }
        comments.push(PrComment {
            id: format!("issue:{}", s(c, "id").unwrap_or_default()),
            kind: "issue".into(),
            author: author.unwrap_or_default().into(),
            association: assoc.unwrap_or_default().into(),
            body,
            created_at: created,
            url: s(c, "url"),
            path: None,
            line: None,
            diff_hunk: None,
        });
    }
    for r in view["reviews"].as_array().into_iter().flatten() {
        // Approval bodies are almost always "LGTM"; inline comments are collected separately.
        if r["state"] == "APPROVED" {
            continue;
        }
        let author = r["author"]["login"].as_str();
        let assoc = r["authorAssociation"].as_str();
        let (Some(body), Some(created)) = (body_of(r), time(r, "submittedAt")) else {
            continue;
        };
        if !is_member(cfg, me, author, assoc) {
            continue;
        }
        comments.push(PrComment {
            id: format!("review:{}", s(r, "id").unwrap_or_default()),
            kind: "review".into(),
            author: author.unwrap_or_default().into(),
            association: assoc.unwrap_or_default().into(),
            body,
            created_at: created,
            url: None,
            path: None,
            line: None,
            diff_hunk: None,
        });
    }
    for c in inline
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|page| page.as_array().into_iter().flatten())
    {
        let author = c["user"]["login"].as_str();
        let assoc = c["author_association"].as_str();
        let (Some(body), Some(created)) = (body_of(c), time(c, "created_at")) else {
            continue;
        };
        if !is_member(cfg, me, author, assoc) {
            continue;
        }
        comments.push(PrComment {
            id: format!("inline:{}", c["id"]),
            kind: "inline".into(),
            author: author.unwrap_or_default().into(),
            association: assoc.unwrap_or_default().into(),
            body,
            created_at: created,
            url: s(c, "html_url"),
            path: s(c, "path"),
            line: c["line"].as_u64().or_else(|| c["original_line"].as_u64()),
            diff_hunk: s(c, "diff_hunk"),
        });
    }
    comments.sort_by_key(|c| c.created_at);
    Ok((info, comments))
}

/// The per-job endpoint works while the rest of the workflow run is still in progress.
pub fn fetch_job_log(repo: &str, job_id: &str) -> Result<String> {
    run_ok(
        "gh",
        &[
            "api",
            "--allow-escape-sequences",
            &format!("repos/{repo}/actions/jobs/{job_id}/logs"),
        ],
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarizes_checks() {
        let rollup = json!([
            {"__typename": "CheckRun", "name": "a", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "name": "b", "workflowName": "Main", "status": "COMPLETED", "conclusion": "FAILURE",
             "detailsUrl": "https://github.com/duckdb/duckdb/actions/runs/1/job/42?pr=3"},
            {"__typename": "CheckRun", "name": "c", "status": "COMPLETED", "conclusion": "CANCELLED"},
            {"__typename": "CheckRun", "name": "d", "status": "IN_PROGRESS"},
        ]);
        let (ci, pending, failed) = summarize_checks(&rollup);
        assert_eq!(ci, CiState::Failure);
        assert!(pending);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].job_id.as_deref(), Some("42"));
        assert_eq!(summarize_checks(&json!([])).0, CiState::None);
        assert_eq!(
            summarize_checks(&json!([{"__typename": "StatusContext", "context": "x", "state": "PENDING"}])).0,
            CiState::Pending
        );
    }
}
