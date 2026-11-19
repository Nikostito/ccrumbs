//! Optional GitHub enrichment through the `gh` CLI (`cctrail sync`).
//!
//! Local transcripts show what *your machine's* sessions did. PRs opened by
//! claude.ai cloud sessions carry a `claude.ai/code/session_…` link in their
//! body, so syncing PR metadata lets cctrail show those too.

use crate::db::Db;
use crate::model::PrMeta;
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;
use std::process::Command;
use std::sync::LazyLock;

static FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)generated with \[?claude code\]?|co-authored-by: claude").unwrap()
});

fn gh(args: &[&str]) -> Result<Value> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("running `gh` (is the GitHub CLI installed?)")?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

pub fn meta_from_json(repo: &str, v: &Value) -> Option<PrMeta> {
    let body = v.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
    let merged = v.get("merged_at").is_some_and(|m| !m.is_null());
    let state = if merged {
        Some("merged".to_string())
    } else {
        s("/state")
    };
    Some(PrMeta {
        repo: repo.to_string(),
        number: v.get("number")?.as_u64()? as u32,
        title: s("/title"),
        state,
        author: s("/user/login"),
        url: s("/html_url"),
        head: s("/head/ref"),
        created_at: s("/created_at"),
        body_session: crate::prdetect::cloud_ids(body).into_iter().next(),
        claude_footer: FOOTER.is_match(body),
    })
}

/// Fetch up to `limit` most recently updated PRs of `repo`.
pub fn sync_repo(db: &Db, repo: &str, limit: usize) -> Result<usize> {
    let mut n = 0;
    let mut page = 1;
    while n < limit {
        let path = format!(
            "repos/{repo}/pulls?state=all&sort=updated&direction=desc&per_page=100&page={page}"
        );
        let v = gh(&["api", &path])?;
        let Some(items) = v.as_array() else { break };
        for it in items {
            if let Some(m) = meta_from_json(repo, it) {
                db.upsert_pr(&m)?;
                n += 1;
            }
        }
        if items.len() < 100 {
            break;
        }
        page += 1;
    }
    Ok(n)
}

pub fn sync_one(db: &Db, repo: &str, number: u32) -> Result<PrMeta> {
    let v = gh(&["api", &format!("repos/{repo}/pulls/{number}")])?;
    let m = meta_from_json(repo, &v).context("unexpected response from GitHub")?;
    db.upsert_pr(&m)?;
    Ok(m)
}
