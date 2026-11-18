//! Parse one Claude Code JSONL transcript into a [`ParsedSession`].
//!
//! The format is internal to Claude Code and changes between releases, so
//! everything here is defensive: unknown record types and malformed lines are
//! skipped, and every field is optional.

use crate::model::{ParsedSession, PrLink, Role};
use crate::prdetect::{self, Pending};
use crate::text;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

const FULLTEXT_CAP: usize = 400_000;
const KEEP_PROMPTS: usize = 12;

static TEST_CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(cargo (test|nextest)|pytest|npm (run )?test|yarn test|pnpm (run )?test|go test|jest|vitest|make (test|check)|rspec|mvn test|gradle\w* test|tox|phpunit|dotnet test|bun test|playwright test)\b").unwrap()
});
static GIT_COMMIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bgit\s+commit\b").unwrap());

#[derive(Default)]
struct State {
    s: ParsedSession,
    seen_msgs: HashSet<String>,
    pending: HashMap<String, Vec<Pending>>,
    links: Vec<PrLink>,
    cloud: HashMap<String, u32>,
    files: Vec<String>,
    files_seen: HashSet<String>,
    cur_ts: Option<String>,
    /// Final `totalCostUSD` per Claude Code process run (keyed by its start time).
    cost_runs: HashMap<i64, f64>,
}

/// Parse a session transcript plus any subagent transcripts stored beside it.
pub fn parse_session(
    path: &Path,
    project_dir: &str,
    extra: &[std::path::PathBuf],
) -> Option<ParsedSession> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    let mut st = State::default();
    st.s.id = id;
    st.s.path = path.to_string_lossy().to_string();
    st.s.project_dir = project_dir.to_string();
    read_file(path, &mut st, false);
    for sub in extra {
        read_file(sub, &mut st, true);
    }
    // Tool calls whose result never arrived: keep only the ones we can place.
    for (_, ps) in st.pending.drain() {
        for p in ps {
            if p.number.is_some() && p.role != Role::Created {
                st.links.push(to_link(p, None));
            }
        }
    }
    st.s.cost_usd = st.cost_runs.values().sum();
    let mut cands: Vec<crate::tickets::Candidate> = Vec::new();
    if let Some(b) = &st.s.branch {
        for key in crate::tickets::from_branch(b) {
            cands.push(crate::tickets::Candidate { key, strong: true });
        }
    }
    let texts =
        st.s.prompts
            .iter()
            .chain(st.s.last_prompt.iter())
            .chain(st.s.custom_title.iter())
            .chain(st.s.summary.iter());
    for t in texts {
        for key in crate::tickets::from_text(t) {
            if !cands.iter().any(|c| c.key == key) {
                cands.push(crate::tickets::Candidate { key, strong: false });
            }
        }
    }
    st.s.tickets = cands;
    st.s.cloud_id = st
        .cloud
        .iter()
        .max_by_key(|(_, c)| **c)
        .map(|(k, _)| k.clone());
    st.s.files_edited = st.files;
    st.s.pr_links = dedupe(st.links);
    if st.s.user_prompts == 0 && st.s.assistant_msgs == 0 && st.s.custom_title.is_none() {
        return None;
    }
    Some(st.s)
}

fn to_link(p: Pending, ts: Option<String>) -> PrLink {
    PrLink {
        repo: p.repo.unwrap_or_default(),
        number: p.number.unwrap_or(0),
        role: p.role,
        ts,
        evidence: p.evidence,
    }
}

fn dedupe(mut links: Vec<PrLink>) -> Vec<PrLink> {
    links.sort_by(|a, b| {
        (&a.repo, a.number, a.role, &a.ts).cmp(&(&b.repo, b.number, b.role, &b.ts))
    });
    links.dedup_by(|b, a| a.repo == b.repo && a.number == b.number && a.role == b.role);
    links
}

fn read_file(path: &Path, st: &mut State, sidechain_file: bool) {
    let Ok(data) = fs::read_to_string(path) else {
        return;
    };
    for line in data.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        handle(&v, st, sidechain_file);
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

fn handle(v: &Value, st: &mut State, sidechain_file: bool) {
    let ty = str_of(v, "type").unwrap_or("");
    let ts = str_of(v, "timestamp").map(str::to_string);
    if let Some(t) = &ts {
        if st.s.started.as_ref().is_none_or(|s| t < s) {
            st.s.started = Some(t.clone());
        }
        if st.s.ended.as_ref().is_none_or(|e| t > e) {
            st.s.ended = Some(t.clone());
        }
        st.cur_ts = Some(t.clone());
    }
    if !sidechain_file {
        if st.s.cwd.is_none() {
            st.s.cwd = str_of(v, "cwd").map(str::to_string);
        }
        if let Some(b) = str_of(v, "gitBranch").filter(|b| !b.is_empty() && *b != "HEAD") {
            st.s.branch = Some(b.to_string());
        }
        if st.s.entrypoint.is_none() {
            st.s.entrypoint = str_of(v, "entrypoint").map(str::to_string);
        }
    }

    if !sidechain_file
        && (v
            .get("isCompactSummary")
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
            || (ty == "system" && str_of(v, "subtype") == Some("compact_boundary")))
    {
        st.s.compactions += 1;
    }

    match ty {
        "cost-state" if !sidechain_file => {
            if let Some(c) = v.get("totalCostUSD").and_then(|x| x.as_f64()) {
                let run = v.get("startTime").and_then(|x| x.as_i64()).unwrap_or(0);
                st.cost_runs.insert(run, c);
            }
        }
        "summary" => {
            if let Some(s) = str_of(v, "summary").filter(|s| !s.trim().is_empty()) {
                st.s.summary = Some(text::one_line(s));
            }
        }
        "custom-title" => {
            if let Some(s) = str_of(v, "customTitle").or_else(|| str_of(v, "title")) {
                st.s.custom_title = Some(text::one_line(s));
            }
        }
        t if t.ends_with("title") => {
            // Newer releases may write AI-generated titles under a different record type.
            let s = str_of(v, "aiTitle")
                .or_else(|| str_of(v, "title"))
                .or_else(|| str_of(v, "customTitle"));
            if let Some(s) = s.filter(|s| !s.trim().is_empty())
                && st.s.summary.is_none()
            {
                st.s.summary = Some(text::one_line(s));
            }
        }
        "user" => handle_user(v, st, sidechain_file),
        "assistant" => handle_assistant(v, st),
        _ => {}
    }
}

fn handle_user(v: &Value, st: &mut State, sidechain_file: bool) {
    let side = sidechain_file
        || v.get("isSidechain")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
    let meta = v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false);
    let Some(content) = v.pointer("/message/content") else {
        return;
    };
    let mut human = String::new();
    match content {
        Value::String(s) => human.push_str(s),
        Value::Array(blocks) => {
            for b in blocks {
                match str_of(b, "type") {
                    Some("text") => {
                        if let Some(t) = str_of(b, "text") {
                            if !human.is_empty() {
                                human.push('\n');
                            }
                            human.push_str(t);
                        }
                    }
                    Some("tool_result") => handle_tool_result(b, st),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if side || meta || human.is_empty() {
        return;
    }
    let Some(prompt) = text::clean_prompt(&human) else {
        return;
    };
    st.s.user_prompts += 1;
    if st.s.first_prompt.is_none() {
        st.s.first_prompt = Some(prompt.clone());
    }
    st.s.last_prompt = Some(prompt.clone());
    if st.s.prompts.len() < KEEP_PROMPTS {
        st.s.prompts
            .push(text::cap_bytes(&prompt, 2000).to_string());
    }
    push_text(&mut st.s.fulltext, &prompt);
    for p in prdetect::from_prompt(&prompt) {
        st.links.push(to_link(p, st.cur_ts.clone()));
    }
}

fn result_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| str_of(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn handle_tool_result(b: &Value, st: &mut State) {
    let Some(id) = str_of(b, "tool_use_id") else {
        return;
    };
    let Some(ps) = st.pending.remove(id) else {
        return;
    };
    let is_error = b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
    let out = result_text(b);
    for h in prdetect::repo_hints(text::cap_bytes(&out, 4000)) {
        st.s.repo_hints.push(h);
    }
    if is_error {
        return;
    }
    for p in ps {
        if let Some(p) = prdetect::complete(p, &out) {
            st.links.push(to_link(p, st.cur_ts.clone()));
        }
    }
}

fn handle_assistant(v: &Value, st: &mut State) {
    let side = v
        .get("isSidechain")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let Some(msg) = v.get("message") else { return };
    let mid = str_of(msg, "id").map(str::to_string);
    let first_time = match &mid {
        Some(id) => st.seen_msgs.insert(id.clone()),
        None => true,
    };
    if first_time && !side {
        st.s.assistant_msgs += 1;
        if let Some(m) = str_of(msg, "model").filter(|m| !m.starts_with('<')) {
            st.s.model = Some(m.to_string());
        }
    }
    if first_time && let Some(u) = msg.get("usage") {
        let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        st.s.tokens_in +=
            g("input_tokens") + g("cache_creation_input_tokens") + g("cache_read_input_tokens");
        st.s.tokens_out += g("output_tokens");
    }
    // Context at the latest main-thread reply: everything sent in, plus what it wrote.
    if !side && let Some(u) = msg.get("usage") {
        let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let ctx = g("input_tokens")
            + g("cache_creation_input_tokens")
            + g("cache_read_input_tokens")
            + g("output_tokens");
        if ctx > 0 {
            st.s.context_tokens = ctx;
        }
    }
    let Some(Value::Array(blocks)) = msg.get("content") else {
        return;
    };
    for b in blocks {
        match str_of(b, "type") {
            Some("text") => {
                if let Some(t) = str_of(b, "text")
                    && !side
                    && !t.trim().is_empty()
                {
                    st.s.last_reply = Some(text::cap_bytes(t, 3000).to_string());
                    push_text(&mut st.s.fulltext, t);
                    for (r, n) in prdetect::urls_in(t) {
                        st.links.push(PrLink {
                            repo: r,
                            number: n,
                            role: Role::Mentioned,
                            ts: st.cur_ts.clone(),
                            evidence: "mentioned in a reply".into(),
                        });
                    }
                }
            }
            Some("tool_use") => handle_tool_use(b, st),
            _ => {}
        }
    }
}

fn handle_tool_use(b: &Value, st: &mut State) {
    let name = str_of(b, "name").unwrap_or("");
    let id = str_of(b, "id").unwrap_or("").to_string();
    let input = b.get("input").cloned().unwrap_or(Value::Null);
    // `mcp__github__create_pull_request` -> `create_pull_request`
    let base = name.rsplit("__").next().unwrap_or(name);

    // claude.ai cloud sessions sign their commits and PR bodies with their session URL.
    let writes_github = match name {
        "Bash" => {
            let code = prdetect::shell_code(str_of(&input, "command").unwrap_or(""));
            GIT_COMMIT.is_match(&code)
                || code.contains("gh pr create")
                || code.contains("gh pr edit")
        }
        _ => matches!(
            base,
            "create_pull_request" | "update_pull_request" | "push_files" | "create_or_update_file"
        ),
    };
    if writes_github {
        for c in prdetect::cloud_ids(&input.to_string()) {
            *st.cloud.entry(c).or_default() += 1;
        }
    }

    let mut pend = Vec::new();
    match name {
        "Bash" => {
            let cmd = str_of(&input, "command").unwrap_or("");
            let code = prdetect::shell_code(cmd);
            st.s.commands_run += 1;
            if TEST_CMD.is_match(&code) {
                st.s.tests_ran = true;
            }
            st.s.commits += GIT_COMMIT.find_iter(&code).count() as u32;
            push_text(&mut st.s.fulltext, text::cap_bytes(cmd, 500));
            pend = prdetect::from_bash(cmd);
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let f = str_of(&input, "file_path").or_else(|| str_of(&input, "notebook_path"));
            if let Some(f) = f {
                let rel = match &st.s.cwd {
                    Some(cwd) => f
                        .strip_prefix(cwd.as_str())
                        .map(|r| r.trim_start_matches('/'))
                        .unwrap_or(f),
                    None => f,
                };
                if st.files_seen.insert(rel.to_string()) {
                    st.files.push(rel.to_string());
                }
            }
        }
        _ => {
            pend = prdetect::from_tool(base, &input);
        }
    }
    if !pend.is_empty() {
        st.pending.entry(id).or_default().extend(pend);
    }
}

fn push_text(buf: &mut String, t: &str) {
    if buf.len() >= FULLTEXT_CAP {
        return;
    }
    let room = FULLTEXT_CAP - buf.len();
    buf.push_str(text::cap_bytes(t, room));
    buf.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_cost_and_compactions() {
        let dir = std::env::temp_dir().join(format!("cctrail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s1.jsonl");
        let lines = [
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01Z","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"text","text":"a"}],"usage":{"input_tokens":10,"cache_read_input_tokens":500000,"cache_creation_input_tokens":0,"output_tokens":90}}}"#,
            r#"{"type":"cost-state","totalCostUSD":1.5,"startTime":1}"#,
            r#"{"type":"cost-state","totalCostUSD":2.5,"startTime":1}"#,
            r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:00:02Z"}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03Z","message":{"id":"m2","model":"claude-opus-5-5","content":[{"type":"text","text":"b"}],"usage":{"input_tokens":5,"cache_read_input_tokens":40000,"output_tokens":5}}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"m3","content":[],"usage":{"input_tokens":900000}}}"#,
            r#"{"type":"cost-state","totalCostUSD":0.75,"startTime":2}"#,
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();
        let s = parse_session(&f, "p", &[]).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(s.context_tokens, 40010); // latest main-thread reply, not the subagent
        assert_eq!(s.compactions, 1);
        assert!((s.cost_usd - 3.25).abs() < 1e-9); // last value per run, summed
    }
}
