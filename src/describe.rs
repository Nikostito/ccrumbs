//! Session descriptions.
//!
//! Every session gets a heuristic description at index time (free, instant).
//! `ccrumbs describe` can additionally ask Claude for a title + summary,
//! which is cached in the `ai` table and preferred when present.

use crate::db::Db;
use crate::model::{ParsedSession, Role, SessionRow};
use crate::text::{file_name, truncate};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// "Asked: … / Then: … / Edited 4 files … / Opened PR #12 …"
pub fn auto_description(s: &ParsedSession) -> String {
    let mut lines = Vec::new();
    if let Some(p) = &s.first_prompt {
        lines.push(format!("Asked: {}", truncate(p, 220)));
    }
    let follow: Vec<String> = s
        .prompts
        .iter()
        .skip(1)
        .filter(|p| p.len() > 12)
        .take(3)
        .map(|p| truncate(p, 90))
        .collect();
    if !follow.is_empty() {
        lines.push(format!("Then: {}", follow.join(" · ")));
    }

    let mut act = Vec::new();
    if !s.files_edited.is_empty() {
        let names: Vec<&str> = s
            .files_edited
            .iter()
            .take(4)
            .map(|f| file_name(f))
            .collect();
        let more = s.files_edited.len().saturating_sub(4);
        let tail = if more > 0 {
            format!(" +{more}")
        } else {
            String::new()
        };
        let n = s.files_edited.len();
        act.push(format!(
            "Edited {n} file{}: {}{tail}",
            if n == 1 { "" } else { "s" },
            names.join(", ")
        ));
    }
    if s.tests_ran {
        act.push("ran tests".into());
    }
    if s.commits > 0 {
        act.push(format!(
            "{} commit{}",
            s.commits,
            if s.commits == 1 { "" } else { "s" }
        ));
    }
    if !act.is_empty() {
        lines.push(act.join(" · "));
    }

    let pr_phrase = |role: Role, verb: &str| -> Option<String> {
        let nums: Vec<String> = s
            .pr_links
            .iter()
            .filter(|l| l.role == role)
            .map(|l| format!("#{}", l.number))
            .collect();
        (!nums.is_empty()).then(|| format!("{verb} {}", nums.join(", ")))
    };
    let prs: Vec<String> = [
        pr_phrase(Role::Created, "Opened PR"),
        pr_phrase(Role::Reviewed, "reviewed"),
        pr_phrase(Role::Merged, "merged"),
        pr_phrase(Role::WorkedOn, "worked on"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !prs.is_empty() {
        lines.push(prs.join(" · "));
    }
    if s.user_prompts > 1
        && let Some(last) = &s.last_prompt
        && Some(last) != s.first_prompt.as_ref()
        && !s.prompts.iter().skip(1).take(3).any(|p| p == last)
    {
        lines.push(format!("Last: {}", truncate(last, 120)));
    }
    lines.join("\n")
}

fn api_auth() -> Result<(&'static str, String)> {
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY").map(|k| k.trim().to_string())
        && !k.is_empty()
    {
        return Ok(("x-api-key", k));
    }
    if let Ok(t) = std::env::var("ANTHROPIC_AUTH_TOKEN").map(|k| k.trim().to_string())
        && !t.is_empty()
    {
        return Ok(("authorization", format!("Bearer {t}")));
    }
    bail!("set ANTHROPIC_API_KEY to generate AI descriptions (heuristic descriptions need no key)")
}

pub fn check_auth() -> Result<()> {
    api_auth().map(|_| ())
}

/// Models that accept the server-side refusal fallback parameter.
fn supports_fallbacks(model: &str) -> bool {
    matches!(
        model,
        "claude-opus-5-5" | "claude-opus-5" | "claude-sonnet-5-5"
    ) || model.starts_with("claude-fable-5")
}

const SYSTEM: &str = "You write index entries for a developer's history of Claude Code sessions, so they can \
find a session again later. You get excerpts from one session. Reply with exactly two lines and nothing else.\n\
Line 1: a specific title, at most 8 words, no quotes, no trailing period.\n\
Line 2: one or two sentences (max 45 words) on what was worked on and where it ended up. Name concrete \
features, files, bugs and PR numbers. Do not start with \"The user\" or \"This session\".";

fn build_input(s: &SessionRow, prompts: &[String], reply: Option<&str>) -> String {
    let mut b = String::new();
    b.push_str(&format!("Project: {}\n", s.resume_dir()));
    if let Some(br) = &s.branch {
        b.push_str(&format!("Git branch: {br}\n"));
    }
    if let Some(t) = s.custom_title.as_ref().or(s.summary.as_ref()) {
        b.push_str(&format!("Existing title: {t}\n"));
    }
    b.push_str(&format!(
        "Prompts typed by the developer ({} total):\n",
        s.user_prompts
    ));
    for (i, p) in prompts.iter().enumerate() {
        b.push_str(&format!("{}. {}\n", i + 1, crate::text::cap_bytes(p, 1200)));
    }
    if let Some(l) = &s.last_prompt
        && prompts.last() != Some(l)
    {
        b.push_str(&format!(
            "Final prompt: {}\n",
            crate::text::cap_bytes(l, 800)
        ));
    }
    if !s.files_edited.is_empty() {
        let files: Vec<&str> = s.files_edited.iter().take(25).map(String::as_str).collect();
        b.push_str(&format!("Files edited: {}\n", files.join(", ")));
    }
    for l in s.prs.iter().filter(|l| l.role.is_significant()) {
        b.push_str(&format!(
            "PR {}#{}: {}\n",
            l.repo,
            l.number,
            l.role.as_str()
        ));
    }
    if let Some(r) = reply {
        b.push_str(&format!(
            "Claude's last reply (excerpt): {}\n",
            crate::text::cap_bytes(r, 1500)
        ));
    }
    b
}

/// Ask Claude for `(title, description)`.
pub fn ai_describe(
    s: &SessionRow,
    prompts: &[String],
    reply: Option<&str>,
    model: &str,
) -> Result<(String, String)> {
    let (hname, hval) = api_auth()?;
    let base =
        std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let mut body = json!({
        "model": model,
        "max_tokens": 4000,
        "system": SYSTEM,
        "output_config": {"effort": "low"},
        "messages": [{"role": "user", "content": build_input(s, prompts, reply)}],
    });
    let fallbacks = supports_fallbacks(model);
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    let mut req = ureq::post(format!("{}/v1/messages", base.trim_end_matches('/')))
        .header(hname, &hval)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");
    if hname == "authorization" {
        req = req.header(
            "anthropic-beta",
            if fallbacks {
                "oauth-2025-04-20,server-side-fallback-2026-07-01"
            } else {
                "oauth-2025-04-20"
            },
        );
    } else if fallbacks {
        req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let mut resp = req
        .config()
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .send_json(&body)
        .context("calling the Claude API")?;
    let status = resp.status();
    let v: Value = resp
        .body_mut()
        .read_json()
        .context("reading the Claude API response")?;
    if !status.is_success() {
        let msg = v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        bail!("Claude API returned {status}: {msg}");
    }
    if v.get("stop_reason").and_then(|r| r.as_str()) == Some("refusal") {
        bail!("the model declined to describe this session");
    }
    let text: String = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = lines.next().ok_or_else(|| anyhow!("empty response"))?;
    let title = title
        .trim_start_matches("Line 1:")
        .trim()
        .trim_matches('"')
        .to_string();
    let desc = lines.collect::<Vec<_>>().join(" ");
    let desc = desc.trim_start_matches("Line 2:").trim().to_string();
    Ok((title, desc))
}

/// Describe one session and store the result.
pub fn describe_one(db: &Db, s: &SessionRow, model: &str) -> Result<(String, String)> {
    let (prompts, reply, size) = db.prompts_and_reply(&s.id)?;
    let (t, d) = ai_describe(s, &prompts, reply.as_deref(), model)?;
    db.set_ai(&s.id, &t, &d, model, size)?;
    Ok((t, d))
}

/// Sessions that have no AI description yet, or have grown since theirs was written.
pub fn needs_description(db: &Db, s: &SessionRow) -> bool {
    if s.user_prompts == 0 {
        return false;
    }
    let size: i64 = db
        .conn
        .query_row("SELECT size FROM sessions WHERE id=?1", [&s.id], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    match db.ai_size(&s.id) {
        Ok(Some(old)) => size > old + old / 5,
        _ => true,
    }
}
