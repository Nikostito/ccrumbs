//! `cctrail mcp`: a Model Context Protocol server over stdio, so Claude Code can
//! look things up in your past sessions ("how did we fix the redis bucket last time?").
//!
//! Register it with: `claude mcp add cctrail -- cctrail mcp`

use crate::config::Config;
use crate::db::Db;
use crate::model::{Role, SessionRow};
use crate::prview::{self, Actor, PrEntry};
use crate::search;
use crate::text::{local_time, truncate};
use crate::transcript::{self, Kind};
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

fn tools() -> Value {
    json!([
        {
            "name": "search_sessions",
            "description": "Search the user's past Claude Code sessions on this machine. Returns matching sessions (newest/best first) with id, title, directory, branch, PRs they created/reviewed, tickets and a short description. Query syntax: free words; #123 or owner/repo#123 for a PR; p:<project> b:<branch> f:<file edited> repo:<owner/repo>; @today @yesterday @week @3d; has:pr; ABC-123 for a ticket. Set full_text to search every prompt, reply and shell command instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search words and filters. Empty lists recent sessions."},
                    "limit": {"type": "integer", "description": "Max results (default 10, max 50)."},
                    "full_text": {"type": "boolean", "description": "Search the full conversation text."}
                },
                "required": ["query"]
            }
        },
        {
            "name": "get_session",
            "description": "Details of one past session: description, prompts the user typed, files edited, PRs (and which other sessions created/reviewed them), tickets, notes, Claude's last reply and the command to resume it.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Session id or a unique prefix (8+ chars)."}},
                "required": ["id"]
            }
        },
        {
            "name": "read_transcript",
            "description": "Read a past session's conversation (user prompts, Claude replies, tool calls). With `query`, returns only the passages that mention it, which is the cheap way to find how something was done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "query": {"type": "string", "description": "Only return passages containing this text (case-insensitive)."},
                    "max_chars": {"type": "integer", "description": "Truncate the result (default 12000)."}
                },
                "required": ["id"]
            }
        },
        {
            "name": "find_pr",
            "description": "Which past sessions created, reviewed, merged or worked on a pull request, and whether creator and reviewer were different sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {"pr": {"type": "string", "description": "42, #42, owner/repo#42 or a PR URL."}},
                "required": ["pr"]
            }
        }
    ])
}

fn session_line(s: &SessionRow) -> String {
    let mut out = format!(
        "- {} · {} · {}{}\n  {}\n",
        s.id,
        local_time(s.ended.as_deref().or(s.started.as_deref())),
        crate::text::home_short(&s.resume_dir()),
        s.branch
            .as_ref()
            .map(|b| format!(" (branch {b})"))
            .unwrap_or_default(),
        s.title()
    );
    let prs: Vec<String> = s
        .pr_summary()
        .iter()
        .filter(|(l, _)| l.role.is_significant())
        .map(|(l, _)| format!("{} {}#{}", l.role.as_str(), l.repo, l.number))
        .collect();
    if !prs.is_empty() {
        out.push_str(&format!("  PRs: {}\n", prs.join(", ")));
    }
    if !s.tickets.is_empty() {
        out.push_str(&format!("  Tickets: {}\n", s.tickets.join(", ")));
    }
    for l in s.description().lines().take(3) {
        out.push_str(&format!("  {}\n", truncate(l, 200)));
    }
    out
}

fn actor(sessions: &[SessionRow], a: &Actor) -> String {
    match a {
        Actor::Local(i) => format!(
            "{} \"{}\"",
            sessions[*i].id,
            truncate(&sessions[*i].title(), 60)
        ),
        Actor::Cloud(c) => format!("claude.ai session https://claude.ai/code/{c}"),
    }
}

struct Ctx {
    db: Db,
    cfg: Config,
}

impl Ctx {
    fn load(&mut self) -> Result<(Vec<SessionRow>, Vec<PrEntry>)> {
        crate::index::run(&mut self.db, &self.cfg, false)?;
        let sessions = self.db.load_sessions()?;
        let prs = prview::build(&sessions, &self.db.load_prs()?);
        Ok((sessions, prs))
    }

    fn one<'a>(
        sessions: &'a [SessionRow],
        id: &str,
    ) -> std::result::Result<&'a SessionRow, String> {
        let m = search::by_id(sessions, id);
        match m.len() {
            1 => Ok(m[0]),
            0 => Err(format!("no session with id starting {id:?}")),
            n => Err(format!(
                "{n} sessions start with {id:?}; give more characters"
            )),
        }
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<std::result::Result<String, String>> {
        let arg = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let (sessions, prs) = self.load()?;
        Ok(match name {
            "search_sessions" => {
                let q = arg("query");
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10)
                    .clamp(1, 50) as usize;
                let full = args
                    .get("full_text")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let parsed = search::parse(&q);
                let hits: Vec<usize> = if full && !parsed.text.is_empty() {
                    let ids = self.db.fts(&parsed.text, limit * 3)?;
                    ids.iter()
                        .filter_map(|id| sessions.iter().position(|s| &s.id == id))
                        .collect()
                } else {
                    let (_, task_of) = crate::tasks::build(&sessions, &prs);
                    let corpus = search::Corpus::new(&sessions, None, task_of);
                    search::Ranker::default().rank(
                        &sessions,
                        &corpus,
                        &parsed,
                        search::Opts::default(),
                    )
                };
                if hits.is_empty() {
                    Ok(format!(
                        "No sessions match {q:?}. Try fewer words, or full_text: true."
                    ))
                } else {
                    let mut out = format!("{} matching session(s):\n", hits.len().min(limit));
                    for &i in hits.iter().take(limit) {
                        out.push_str(&session_line(&sessions[i]));
                    }
                    Ok(out)
                }
            }
            "get_session" => match Self::one(&sessions, &arg("id")) {
                Err(e) => Err(e),
                Ok(s) => {
                    let me = sessions.iter().position(|x| x.id == s.id);
                    let mut out = session_line(s);
                    out.push_str(&format!("\nDescription:\n{}\n", s.description()));
                    if let Some(n) = &s.note {
                        out.push_str(&format!("\nUser's note: {n}\n"));
                    }
                    if let Ok((prompts, reply, _)) = self.db.prompts_and_reply(&s.id) {
                        out.push_str("\nPrompts the user typed:\n");
                        for (i, p) in prompts.iter().enumerate() {
                            out.push_str(&format!("{}. {}\n", i + 1, truncate(p, 400)));
                        }
                        if let Some(r) = reply {
                            out.push_str(&format!(
                                "\nClaude's last reply:\n{}\n",
                                crate::text::cap_bytes(&r, 2000)
                            ));
                        }
                    }
                    if !s.files_edited.is_empty() {
                        out.push_str(&format!("\nFiles edited: {}\n", s.files_edited.join(", ")));
                    }
                    for (l, _) in s.pr_summary() {
                        if let Some(e) = prs
                            .iter()
                            .find(|e| e.repo == l.repo && e.number == l.number)
                        {
                            out.push_str(&format!(
                                "\nPR {}: this session {} it. {}\n",
                                e.key(),
                                l.role.as_str(),
                                e.story(&sessions)
                            ));
                            for (role, actors) in [
                                (Role::Created, e.creators()),
                                (Role::Reviewed, e.reviewers()),
                            ] {
                                for a in actors
                                    .iter()
                                    .filter(|a| me.is_none_or(|m| **a != Actor::Local(m)))
                                {
                                    out.push_str(&format!(
                                        "  {} in another session: {}\n",
                                        role.as_str(),
                                        actor(&sessions, a)
                                    ));
                                }
                            }
                        }
                    }
                    if let Some(c) = s.context_summary() {
                        out.push_str(&format!("\n{c}\n"));
                    }
                    out.push_str(&format!(
                        "\nResume with: {}\n",
                        crate::launch::resume_command(s)
                    ));
                    Ok(out)
                }
            },
            "read_transcript" => match Self::one(&sessions, &arg("id")) {
                Err(e) => Err(e),
                Ok(s) => {
                    let entries = transcript::load(&s.path);
                    let max = args
                        .get("max_chars")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(12_000)
                        .clamp(500, 100_000) as usize;
                    let q = arg("query").to_lowercase();
                    let body = if q.is_empty() {
                        transcript::to_text(&entries, false)
                    } else {
                        // Each matching message, with the one before and after for context.
                        let mut keep = vec![false; entries.len()];
                        for (i, e) in entries.iter().enumerate() {
                            if e.kind != Kind::Output && e.text.to_lowercase().contains(&q) {
                                let end = (i + 1).min(entries.len().saturating_sub(1));
                                keep[i.saturating_sub(1)..=end].fill(true);
                            }
                        }
                        let picked: Vec<_> = entries
                            .iter()
                            .zip(keep)
                            .filter(|(_, k)| *k)
                            .map(|(e, _)| e.clone())
                            .collect();
                        if picked.is_empty() {
                            format!("No passages mention {:?}.", arg("query"))
                        } else {
                            transcript::to_text(&picked, false)
                        }
                    };
                    let mut body = crate::text::cap_bytes(&body, max).to_string();
                    if body.len() >= max {
                        body.push_str("\n… (truncated; pass a query or a larger max_chars)");
                    }
                    Ok(format!("Session {} — {}\n{}", s.id, s.title(), body))
                }
            },
            "find_pr" => {
                let hits = prview::lookup(&prs, &arg("pr"));
                if hits.is_empty() {
                    Ok(format!("No local session touched PR {}.", arg("pr")))
                } else {
                    let mut out = String::new();
                    for e in hits {
                        out.push_str(&format!(
                            "{}{}\n{}\n",
                            e.key(),
                            e.title().map(|t| format!(" — {t}")).unwrap_or_default(),
                            e.story(&sessions)
                        ));
                        for (l, also) in e.grouped() {
                            let who = match l.session {
                                Some(i) => actor(&sessions, &Actor::Local(i)),
                                None => l
                                    .cloud_id
                                    .as_ref()
                                    .map(|c| actor(&sessions, &Actor::Cloud(c.clone())))
                                    .unwrap_or_default(),
                            };
                            let also = if also.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    " (also {})",
                                    also.iter()
                                        .map(|r| r.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            };
                            out.push_str(&format!("  {}: {who}{also}\n", l.role.as_str()));
                        }
                        out.push('\n');
                    }
                    Ok(out)
                }
            }
            other => Err(format!("unknown tool {other}")),
        })
    }
}

fn reply(out: &mut impl Write, id: &Value, result: Value) -> Result<()> {
    writeln!(
        out,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    )?;
    out.flush()?;
    Ok(())
}

fn error(out: &mut impl Write, id: &Value, code: i64, msg: &str) -> Result<()> {
    writeln!(
        out,
        "{}",
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
    )?;
    out.flush()?;
    Ok(())
}

pub fn serve(cfg: Config) -> Result<()> {
    let mut ctx = Ctx {
        db: Db::open()?,
        cfg,
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                error(&mut out, &Value::Null, -32700, &format!("parse error: {e}"))?;
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        }; // notifications
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let version = if VERSIONS.contains(&asked) {
                    asked
                } else {
                    VERSIONS[0]
                };
                reply(
                    &mut out,
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "cctrail", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": "Search the user's past Claude Code sessions. Use search_sessions to find a session (by topic, file, branch, PR or ticket), get_session for its details, read_transcript with a query to see exactly how something was done, and find_pr for which sessions created or reviewed a PR."
                    }),
                )?;
            }
            "ping" => reply(&mut out, &id, json!({}))?,
            "tools/list" => reply(&mut out, &id, json!({"tools": tools()}))?,
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = match ctx.call(name, &args) {
                    Ok(Ok(text)) => {
                        json!({"content": [{"type": "text", "text": text}], "isError": false})
                    }
                    Ok(Err(msg)) => {
                        json!({"content": [{"type": "text", "text": msg}], "isError": true})
                    }
                    Err(e) => {
                        json!({"content": [{"type": "text", "text": format!("cctrail error: {e:#}")}], "isError": true})
                    }
                };
                reply(&mut out, &id, result)?;
            }
            _ => error(
                &mut out,
                &id,
                -32601,
                &format!("method not found: {method}"),
            )?,
        }
    }
    Ok(())
}
