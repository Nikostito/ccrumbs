//! Optional GitHub enrichment through the `gh` CLI (`ccrumbs sync`).
//!
//! Local transcripts show what *your machine's* sessions did. PRs opened by
//! claude.ai cloud sessions carry a `claude.ai/code/session_…` link in their
//! body, so syncing PR metadata lets ccrumbs show those too.

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
        status: None,
    })
}

const VIEW_FIELDS: &str = "number,title,state,author,url,headRefName,createdAt,body,reviewDecision,mergeable,statusCheckRollup,reviews,comments";

/// One PR with what it's waiting on: review decision, CI, conflicts, latest feedback.
pub fn fetch_status(repo: &str, number: u32) -> Result<PrMeta> {
    let v = gh(&[
        "pr",
        "view",
        &number.to_string(),
        "-R",
        repo,
        "--json",
        VIEW_FIELDS,
    ])?;
    status_from_view(repo, &v, my_login().as_deref())
        .context("unexpected response from `gh pr view`")
}

/// Your GitHub login (asked once), so your own comments don't count as feedback.
fn my_login() -> Option<String> {
    static ME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ME.get_or_init(|| {
        gh(&["api", "user"])
            .ok()
            .and_then(|v| v.get("login")?.as_str().map(str::to_string))
    })
    .clone()
}

/// Parse `gh pr view --json …` output.
pub fn status_from_view(repo: &str, v: &Value, me: Option<&str>) -> Option<PrMeta> {
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
    let body = s("/body").unwrap_or_default();
    let author = s("/author/login");
    let is_bot = |login: &str| login.ends_with("[bot]") || login.starts_with("app/");
    // Feedback: reviews from anyone but the author (review bots count), and comments
    // from people (CI bots comment on every push).
    let mut latest: Option<(String, String)> = None;
    let mut see = |who: Option<&str>, when: Option<&str>, bots_ok: bool| {
        let (Some(who), Some(when)) = (who, when) else {
            return;
        };
        if Some(who) == author.as_deref() || Some(who) == me || (!bots_ok && is_bot(who)) {
            return;
        }
        if latest.as_ref().is_none_or(|(t, _)| when > t.as_str()) {
            latest = Some((when.to_string(), who.to_string()));
        }
    };
    for r in v
        .get("reviews")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        if r.get("state").and_then(|x| x.as_str()) == Some("PENDING") {
            continue;
        }
        see(
            r.pointer("/author/login").and_then(|x| x.as_str()),
            r.get("submittedAt").and_then(|x| x.as_str()),
            true,
        );
    }
    for c in v
        .get("comments")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        see(
            c.pointer("/author/login").and_then(|x| x.as_str()),
            c.get("createdAt").and_then(|x| x.as_str()),
            false,
        );
    }
    let checks = checks_state(v.get("statusCheckRollup"));
    Some(PrMeta {
        repo: repo.to_string(),
        number: v.get("number")?.as_u64()? as u32,
        title: s("/title"),
        state: s("/state").map(|x| x.to_lowercase()),
        author,
        url: s("/url"),
        head: s("/headRefName"),
        created_at: s("/createdAt"),
        body_session: crate::prdetect::cloud_ids(&body).into_iter().next(),
        claude_footer: FOOTER.is_match(&body),
        status: Some(crate::model::PrStatus {
            review_decision: s("/reviewDecision").unwrap_or_default(),
            checks,
            mergeable: s("/mergeable").unwrap_or_default(),
            feedback_at: latest.as_ref().map(|(t, _)| t.clone()),
            feedback_by: latest.map(|(_, w)| w),
            synced: chrono::Utc::now().to_rfc3339(),
        }),
    })
}

/// Sum up `statusCheckRollup`: any failure wins, then anything still running.
fn checks_state(rollup: Option<&Value>) -> String {
    let Some(items) = rollup.and_then(|x| x.as_array()).filter(|a| !a.is_empty()) else {
        return String::new();
    };
    let mut pending = false;
    for c in items {
        let g = |k: &str| c.get(k).and_then(|x| x.as_str()).unwrap_or("");
        // CheckRun: status + conclusion. StatusContext: state.
        let (status, conclusion, state) = (g("status"), g("conclusion"), g("state"));
        if matches!(
            conclusion,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ) || matches!(state, "FAILURE" | "ERROR")
        {
            return "fail".into();
        }
        if (!status.is_empty() && status != "COMPLETED") || matches!(state, "PENDING" | "EXPECTED")
        {
            pending = true;
        }
    }
    if pending { "pending" } else { "pass" }.into()
}

/// Is `gh` installed?
pub fn available() -> bool {
    crate::launch::which("gh")
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
