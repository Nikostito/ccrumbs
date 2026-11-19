//! The conversation itself, for reading (viewer), exporting (Markdown) and the MCP server.

use crate::text;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    Tool,
    Output,
    Error,
    Meta,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: Kind,
    pub ts: Option<String>,
    pub text: String,
}

const OUTPUT_CAP: usize = 1500;

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

/// One-line summary of a tool call.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let base = name.rsplit("__").next().unwrap_or(name);
    let path = s(input, "file_path")
        .or_else(|| s(input, "notebook_path"))
        .or_else(|| s(input, "path"));
    match name {
        "Bash" => format!("$ {}", s(input, "command").unwrap_or("").trim()),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "Read" => {
            format!("{name} {}", path.unwrap_or(""))
        }
        "Grep" | "Glob" => format!("{name} {}", s(input, "pattern").unwrap_or("")),
        "Task" | "Agent" => format!("Agent: {}", s(input, "description").unwrap_or("")),
        "WebFetch" | "WebSearch" => format!(
            "{name} {}",
            s(input, "url").or_else(|| s(input, "query")).unwrap_or("")
        ),
        _ => {
            let json = input.to_string();
            format!("{base} {}", text::cap_bytes(&json, 200))
        }
    }
}

fn result_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| s(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Load the main thread of a transcript (subagent side-chains are skipped).
pub fn load(path: &str) -> Vec<Entry> {
    let Ok(data) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let mut out = Vec::new();
    let mut seen_text: std::collections::HashSet<String> = Default::default();
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("isSidechain")
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let ts = s(&v, "timestamp").map(str::to_string);
        let ty = s(&v, "type").unwrap_or("");
        if ty == "system" && s(&v, "subtype") == Some("compact_boundary") {
            out.push(Entry {
                kind: Kind::Meta,
                ts,
                text: "— conversation compacted —".into(),
            });
            continue;
        }
        let Some(content) = v.pointer("/message/content") else {
            continue;
        };
        let meta = v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false)
            || v.get("isCompactSummary")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
        match (ty, content) {
            ("user", Value::String(t)) if !meta => {
                if let Some(p) = text::clean_prompt(t) {
                    out.push(Entry {
                        kind: Kind::User,
                        ts,
                        text: p,
                    });
                }
            }
            ("user", Value::Array(blocks)) => {
                for b in blocks {
                    match s(b, "type") {
                        Some("text") if !meta => {
                            if let Some(p) = s(b, "text").and_then(text::clean_prompt) {
                                out.push(Entry {
                                    kind: Kind::User,
                                    ts: ts.clone(),
                                    text: p,
                                });
                            }
                        }
                        Some("tool_result") => {
                            let err = b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                            let t = result_text(b);
                            let t = t.trim();
                            if !t.is_empty() {
                                let mut body = text::cap_bytes(t, OUTPUT_CAP).to_string();
                                if t.len() > OUTPUT_CAP {
                                    body.push_str(" …");
                                }
                                out.push(Entry {
                                    kind: if err { Kind::Error } else { Kind::Output },
                                    ts: ts.clone(),
                                    text: body,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            ("assistant", Value::Array(blocks)) => {
                for b in blocks {
                    match s(b, "type") {
                        Some("text") => {
                            let t = s(b, "text").unwrap_or("").trim();
                            // Streaming can repeat a block across records.
                            if !t.is_empty()
                                && seen_text.insert(format!("{}{}", ts.as_deref().unwrap_or(""), t))
                            {
                                out.push(Entry {
                                    kind: Kind::Assistant,
                                    ts: ts.clone(),
                                    text: t.to_string(),
                                });
                            }
                        }
                        Some("tool_use") => {
                            let name = s(b, "name").unwrap_or("tool");
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            out.push(Entry {
                                kind: Kind::Tool,
                                ts: ts.clone(),
                                text: tool_summary(name, &input),
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn clock(ts: Option<&str>) -> String {
    ts.and_then(text::parse_ts)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Plain-text rendering (for `cctrail view` and the MCP server).
pub fn to_text(entries: &[Entry], outputs: bool) -> String {
    let mut b = String::new();
    for e in entries {
        match e.kind {
            Kind::User => b.push_str(&format!(
                "\n▶ YOU  {}\n{}\n",
                clock(e.ts.as_deref()),
                e.text
            )),
            Kind::Assistant => b.push_str(&format!("\n◀ CLAUDE\n{}\n", e.text)),
            Kind::Tool => b.push_str(&format!("  ⚙ {}\n", text::truncate(&e.text, 160))),
            Kind::Output | Kind::Error if outputs => {
                for l in e.text.lines().take(8) {
                    b.push_str(&format!("    │ {l}\n"));
                }
            }
            Kind::Meta => b.push_str(&format!("\n{}\n", e.text)),
            _ => {}
        }
    }
    b
}

/// Markdown document for a session.
pub fn to_markdown(s: &crate::model::SessionRow, entries: &[Entry], outputs: bool) -> String {
    let mut m = format!("# {}\n\n", s.title());
    m.push_str(&format!("- **Session:** `{}`\n", s.id));
    m.push_str(&format!("- **Directory:** `{}`", s.resume_dir()));
    if let Some(b) = &s.branch {
        m.push_str(&format!(" (branch `{b}`)"));
    }
    m.push('\n');
    m.push_str(&format!(
        "- **When:** {} · {} prompts",
        clock(s.started.as_deref()),
        s.user_prompts
    ));
    if let Some(d) = text::duration(s.started.as_deref(), s.ended.as_deref()) {
        m.push_str(&format!(" · {d}"));
    }
    m.push('\n');
    for l in s.pr_summary() {
        let (l, _) = l;
        m.push_str(&format!(
            "- **PR {}#{}:** {}\n",
            l.repo,
            l.number,
            l.role.as_str()
        ));
    }
    for t in &s.tickets {
        match crate::config::ticket_url(t) {
            Some(u) => m.push_str(&format!("- **Ticket:** [{t}]({u})\n")),
            None => m.push_str(&format!("- **Ticket:** {t}\n")),
        }
    }
    if let Some(n) = &s.note {
        m.push_str(&format!("- **Note:** {n}\n"));
    }
    m.push_str(&format!(
        "\n## Summary\n\n{}\n",
        s.description().lines().collect::<Vec<_>>().join("  \n")
    ));
    if !s.files_edited.is_empty() {
        m.push_str("\n## Files edited\n\n");
        for f in &s.files_edited {
            m.push_str(&format!("- `{f}`\n"));
        }
    }
    m.push_str("\n## Conversation\n");
    let mut tools: Vec<String> = Vec::new();
    let flush = |m: &mut String, tools: &mut Vec<String>| {
        if !tools.is_empty() {
            m.push_str("\n```text\n");
            for t in tools.drain(..) {
                m.push_str(&t);
                m.push('\n');
            }
            m.push_str("```\n");
        }
    };
    for e in entries {
        match e.kind {
            Kind::User => {
                flush(&mut m, &mut tools);
                m.push_str(&format!(
                    "\n### You · {}\n\n{}\n",
                    clock(e.ts.as_deref()),
                    e.text
                ));
            }
            Kind::Assistant => {
                flush(&mut m, &mut tools);
                m.push_str(&format!("\n**Claude:**\n\n{}\n", e.text));
            }
            Kind::Tool => tools.push(text::truncate(&e.text, 200)),
            Kind::Output | Kind::Error if outputs => {
                for l in e.text.lines().take(10) {
                    tools.push(format!("  │ {l}"));
                }
            }
            Kind::Meta => {
                flush(&mut m, &mut tools);
                m.push_str(&format!("\n*{}*\n", e.text));
            }
            _ => {}
        }
    }
    flush(&mut m, &mut tools);
    m
}

/// `a1b2c3d4-rate-limiting-for-the-api.md`
pub fn file_name(s: &crate::model::SessionRow) -> String {
    let slug: String = s
        .title()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|w| !w.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    format!("{}-{}.md", s.short_id(), slug)
}
