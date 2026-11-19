//! End-to-end tests: build a small `~/.claude/projects` tree and run the real binary.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = std::env::temp_dir().join(format!("ccrumbs-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("claude/projects")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        let env = Env { root };
        env.fixtures();
        env
    }

    fn projects(&self) -> PathBuf {
        self.root.join("claude/projects")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ccrumbs"));
        c.args(args)
            .env("CCRUMBS_HOME", self.root.join("data"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("HOME", self.root.join("home"))
            .env("NO_COLOR", "1")
            .env_remove("CCTRAIL_HOME")
            .env_remove("TMUX")
            .current_dir(&self.root);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    /// stdout of a command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "ccrumbs {args:?} failed:\n{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn session(&self, cwd: &str, id: &str, records: Vec<Value>) {
        let dir = self.projects().join(cwd.replace('/', "-"));
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<String> = records.iter().map(|r| r.to_string()).collect();
        std::fs::write(dir.join(format!("{id}.jsonl")), body.join("\n") + "\n").unwrap();
    }

    fn fixtures(&self) {
        let api = "/work/api";
        let ts = |d: u32, h: u32| format!("2026-03-{d:02}T{h:02}:00:00Z");
        let user = |cwd: &str, branch: &str, t: String, text: &str| json!({"type":"user","cwd":cwd,"gitBranch":branch,"timestamp":t,"message":{"role":"user","content":text}});
        let tool = |t: String, id: &str, name: &str, input: Value| {
            json!({"type":"assistant","timestamp":t,"message":{"id":format!("m-{id}"),"model":"claude-opus-5-5",
                "content":[{"type":"tool_use","id":id,"name":name,"input":input}],
                "usage":{"input_tokens":10,"cache_read_input_tokens":400000,"output_tokens":100}}})
        };
        let result = |t: String, id: &str, out: &str| json!({"type":"user","timestamp":t,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":out}]}});
        // A creates acme/api#88 on a feature branch.
        self.session(
            api,
            "aaaaaaaa-0000-4000-8000-000000000001",
            vec![
                user(
                    api,
                    "feat/rate-limit",
                    ts(1, 9),
                    "Add per-key rate limiting to the public API",
                ),
                tool(
                    ts(1, 9),
                    "t1",
                    "Edit",
                    json!({"file_path": "/work/api/src/ratelimit.rs"}),
                ),
                result(ts(1, 9), "t1", "ok"),
                tool(
                    ts(1, 10),
                    "t2",
                    "Bash",
                    json!({"command": "cargo test && git commit -am 'rate limit'"}),
                ),
                result(ts(1, 10), "t2", "test result: ok"),
                tool(
                    ts(1, 11),
                    "t3",
                    "Bash",
                    json!({"command": "gh pr create --title 'Rate limiting' --body 'see #3'"}),
                ),
                result(ts(1, 11), "t3", "https://github.com/acme/api/pull/88\n"),
            ],
        );
        // B reviews #88 in a different session.
        self.session(
            api,
            "bbbbbbbb-0000-4000-8000-000000000002",
            vec![
                user(
                    api,
                    "main",
                    ts(2, 9),
                    "<command-name>/review</command-name><command-args>88</command-args>",
                ),
                tool(
                    ts(2, 9),
                    "t1",
                    "mcp__github__pull_request_review_write",
                    json!({"owner":"acme","repo":"api","pullNumber":88,"method":"create"}),
                ),
                result(ts(2, 9), "t1", "{\"id\":1}"),
            ],
        );
        // C opens and reviews its own PR; works on ticket WEB-142.
        self.session(
            "/work/web",
            "cccccccc-0000-4000-8000-000000000003",
            vec![
                user(
                    "/work/web",
                    "feature/WEB-142-dark-mode",
                    ts(3, 9),
                    "Implement WEB-142 dark mode",
                ),
                tool(
                    ts(3, 9),
                    "t1",
                    "mcp__github__create_pull_request",
                    json!({"owner":"acme","repo":"web","title":"Dark mode"}),
                ),
                result(
                    ts(3, 9),
                    "t1",
                    "{\"number\":7,\"html_url\":\"https://github.com/acme/web/pull/7\"}",
                ),
                tool(
                    ts(3, 10),
                    "t2",
                    "Bash",
                    json!({"command": "gh pr review 7 --comment -b 'self check' -R acme/web"}),
                ),
                result(ts(3, 10), "t2", ""),
            ],
        );
        // D continues WEB-142 from main: same task through the ticket.
        self.session("/work/web", "dddddddd-0000-4000-8000-000000000004", vec![
            user("/work/web", "main", ts(4, 9), "follow up on WEB-142: link contrast"),
            json!({"type":"assistant","timestamp":ts(4,9),"message":{"id":"m-d","content":[{"type":"text","text":"Fixed the contrast."}]}}),
        ]);
        // E: heredoc text that mentions gh commands must not count.
        self.session("/work/docs", "eeeeeeee-0000-4000-8000-000000000005", vec![
            user("/work/docs", "main", ts(5, 9), "write the docs page"),
            tool(ts(5, 9), "t1", "Bash", json!({"command": "cat > doc.md <<'EOF'\nrun gh pr create, then gh pr merge 5\nEOF"})),
            result(ts(5, 9), "t1", ""),
        ]);
    }
}

fn mcp(env: &Env, requests: &[Value]) -> Vec<Value> {
    use std::io::Write;
    let mut child = env
        .cmd(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn lists_and_describes_sessions() {
    let e = Env::new("list");
    let out = e.ok(&["list"]);
    assert!(out.contains("Add per-key rate limiting"), "{out}");
    assert!(out.contains("Edited 1 file: ratelimit.rs"), "{out}");
    assert!(out.contains("ran tests"), "{out}");
    let show = e.ok(&["show", "aaaa"]);
    assert!(show.contains("context 400k / 1.0M (40%)"), "{show}");
    assert!(
        show.contains(
            "Resume: cd /work/api && claude --resume aaaaaaaa-0000-4000-8000-000000000001"
        ),
        "{show}"
    );
}

#[test]
fn pr_story_distinguishes_creator_and_reviewer_sessions() {
    let e = Env::new("pr");
    let out = e.ok(&["pr", "88"]);
    assert!(out.contains("acme/api#88"), "{out}");
    assert!(out.contains("Created by aaaaaaaa"), "{out}");
    assert!(
        out.contains("reviewed by a DIFFERENT session: bbbbbbbb"),
        "{out}"
    );
    let same = e.ok(&["pr", "acme/web#7"]);
    assert!(
        same.contains("Created and reviewed in the SAME session: cccccccc"),
        "{same}"
    );
    // Heredoc text isn't a PR action.
    assert!(!e.run(&["pr", "5"]).status.success());
}

#[test]
fn tasks_tickets_and_filters() {
    let e = Env::new("tasks");
    let tasks = e.ok(&["tasks"]);
    assert!(tasks.contains("WEB-142: dark mode"), "{tasks}");
    assert!(
        tasks.contains("cccccccc") && tasks.contains("dddddddd"),
        "{tasks}"
    );
    assert!(e.ok(&["list", "-s", "f:ratelimit"]).contains("aaaaaaaa"));
    assert!(!e.ok(&["list", "-s", "f:ratelimit"]).contains("bbbbbbbb"));
    let ticket = e.ok(&["list", "-s", "WEB-142"]);
    assert!(
        ticket.contains("cccccccc") && ticket.contains("dddddddd") && !ticket.contains("aaaaaaaa"),
        "{ticket}"
    );
    let prs = e.ok(&["list", "-s", "has:pr"]);
    assert!(!prs.contains("eeeeeeee"), "{prs}");
}

#[test]
fn marks_view_export_and_go() {
    let e = Env::new("marks");
    e.ok(&["pin", "dddd"]);
    e.ok(&["note", "dddd", "waiting", "on", "design"]);
    let first = e.ok(&["list", "-s", "-n", "1"]);
    assert!(first.contains("dddddddd"), "pinned first: {first}");
    assert!(e.ok(&["list", "-s", "has:note"]).contains("dddddddd"));
    e.ok(&["hide", "eeee"]);
    assert!(!e.ok(&["list", "-s"]).contains("eeeeeeee"));
    assert!(e.ok(&["list", "-s", "is:hidden"]).contains("eeeeeeee"));

    let view = e.ok(&["view", "dddd"]);
    assert!(
        view.contains("follow up on WEB-142") && view.contains("Fixed the contrast."),
        "{view}"
    );
    let md = e.ok(&["export", "aaaa", "-o", "-"]);
    assert!(md.starts_with("# Add per-key rate limiting"), "{md}");
    assert!(md.contains("- **PR acme/api#88:** created"), "{md}");

    let cmd = e.ok(&["go", "bbbb", "--print"]);
    assert_eq!(
        cmd.trim(),
        "cd /work/api && claude --resume bbbbbbbb-0000-4000-8000-000000000002"
    );
    // Ambiguous: nothing on stdout, non-zero exit.
    let amb = e.run(&["go", "work", "--print"]);
    assert!(!amb.status.success() && amb.stdout.is_empty());
}

#[test]
fn deleted_transcripts_survive_a_full_rebuild() {
    let e = Env::new("deleted");
    e.ok(&["index"]);
    std::fs::remove_file(
        e.projects()
            .join("-work-docs/eeeeeeee-0000-4000-8000-000000000005.jsonl"),
    )
    .unwrap();
    e.ok(&["index"]);
    e.ok(&["index", "--full"]);
    let show = e.ok(&["show", "eeee"]);
    assert!(show.contains("Transcript no longer on disk"), "{show}");
    // A wrong projects folder must not mark everything as deleted.
    let o = e
        .cmd(&["index"])
        .env("CLAUDE_CONFIG_DIR", e.root.join("nowhere"))
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(
        !e.ok(&["show", "aaaa"])
            .contains("Transcript no longer on disk")
    );
}

#[test]
fn ticket_links_and_open() {
    let e = Env::new("tickets");
    assert!(!e.run(&["open", "WEB-142", "--print"]).status.success());
    e.ok(&[
        "tickets",
        "set",
        "WEB",
        "https://acme.atlassian.net/browse/WEB-1",
    ]);
    assert_eq!(
        e.ok(&["open", "WEB-142", "--print"]).trim(),
        "https://acme.atlassian.net/browse/WEB-142"
    );
    assert_eq!(
        e.ok(&["open", "aaaa", "--print"]).trim(),
        "https://github.com/acme/api/pull/88"
    );
    // A broken config is never overwritten.
    let cfg = e.root.join("data/config.json");
    std::fs::write(&cfg, "{ broken").unwrap();
    assert!(
        !e.run(&["tickets", "set", "ENG", "https://linear.app/x"])
            .status
            .success()
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{ broken");
}

#[test]
fn mcp_server() {
    let e = Env::new("mcp");
    let r = mcp(
        &e,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"find_pr","arguments":{"pr":"#88"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_transcript","arguments":{"id":"aaaa","query":"rate limiting"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}),
        ],
    );
    assert_eq!(
        r.len(),
        5,
        "one reply per request, none for the notification: {r:?}"
    );
    assert_eq!(r[0]["result"]["protocolVersion"], "2025-06-18");
    let names: Vec<&str> = r[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "search_sessions",
            "get_session",
            "read_transcript",
            "find_pr"
        ]
    );
    let story = r[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(story.contains("reviewed by a DIFFERENT session"), "{story}");
    assert!(
        r[3]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Add per-key rate limiting")
    );
    assert_eq!(r[4]["error"]["code"], -32602);
}
