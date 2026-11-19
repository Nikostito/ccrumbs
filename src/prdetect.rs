//! Figuring out which PRs a session touched, and *how* (created, reviewed, ...).
//!
//! Evidence comes from three places in a transcript:
//! - `gh pr ...` / `gh api .../pulls/...` shell commands (and their output),
//! - GitHub MCP tool calls (`mcp__github__create_pull_request`, review tools, ...),
//! - what the human typed (`/review 42`, "review https://github.com/o/r/pull/7").

use crate::model::Role;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static PR_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/pull/(\d+)").unwrap()
});
static REPO_HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)#(\d+)\b").unwrap());
static GH_PR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[;&|(\n`]|\$\()\s*(?:command\s+|env\s+(?:\w+=\S+\s+)*)?gh\s+pr\s+(create|new|review|comment|merge|checkout|co|edit|ready|close|reopen|view|diff|checks)\b([^\n;&|]*)",
    )
    .unwrap()
});
static GH_API: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[;&|(\n`]|\$\()\s*gh\s+api\b([^\n;&|]*)").unwrap());
static API_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"repos/(?:([A-Za-z0-9_.-]+)|\{owner\}|:owner)/(?:([A-Za-z0-9_.-]+)|\{repo\}|:repo)/(pulls|issues)(?:/(\d+))?(?:/(reviews|comments|merge|requested_reviewers|files|commits))?",
    )
    .unwrap()
});
static API_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:-X|--method)\s*=?\s*(POST|PUT|PATCH)|\s(?:-f|-F|--field|--raw-field|--input)\s")
        .unwrap()
});
static HASH_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#(\d{1,7})\b").unwrap());
static PR_WORD_NUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:pr|pull request|pull)\s*#?\s*(\d{1,7})\b").unwrap());
static HAS_PR_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(pr|prs|pull request|pull requests)\b").unwrap());
static REVIEW_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(review|reviews|reviewing|code-review|look over|audit)\b").unwrap()
});
static ADDRESS_REVIEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(review (comments|feedback|threads|findings)|reviewer|address|fix (the )?(review|comments)|autofix|babysit|ci fail)")
        .unwrap()
});
static GIT_REMOTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"github\.com[:/]([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(?:\.git)?(?:[\s/'\x22]|$)")
        .unwrap()
});
static JSON_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""number"\s*:\s*(\d+)"#).unwrap());
static CLOUD_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"claude\.ai/code/(session_[A-Za-z0-9]+)").unwrap());

/// A PR action seen in a tool call, possibly waiting on its result to learn the number.
#[derive(Debug, Clone)]
pub struct Pending {
    pub repo: Option<String>,
    pub number: Option<u32>,
    pub role: Role,
    pub evidence: String,
}

fn pending(
    repo: Option<String>,
    number: Option<u32>,
    role: Role,
    evidence: impl Into<String>,
) -> Pending {
    Pending {
        repo,
        number,
        role,
        evidence: evidence.into(),
    }
}

/// Extract `(repo, number)` from a ref like `42`, `#42`, `o/r#42` or a PR URL.
pub fn parse_ref(s: &str) -> Option<(Option<String>, u32)> {
    let s = s.trim();
    if let Some(c) = PR_URL.captures(s) {
        return Some((
            Some(format!("{}/{}", &c[1], &c[2]).to_lowercase()),
            c[3].parse().ok()?,
        ));
    }
    if let Some(c) = REPO_HASH.captures(s) {
        return Some((Some(c[1].to_lowercase()), c[2].parse().ok()?));
    }
    let n = s.trim_start_matches('#');
    n.parse().ok().map(|n| (None, n))
}

/// All PR URLs in a blob of text.
pub fn urls_in(text: &str) -> Vec<(String, u32)> {
    PR_URL
        .captures_iter(text)
        .filter(|c| !RESERVED_OWNERS.contains(&c[1].to_lowercase().as_str()))
        .filter_map(|c| {
            Some((
                format!("{}/{}", &c[1], &c[2]).to_lowercase(),
                c[3].parse().ok()?,
            ))
        })
        .collect()
}

/// `owner/repo` names mentioned as GitHub remotes in text (e.g. `git push` output).
/// First path parts of github.com URLs that aren't `owner/repo`.
const RESERVED_OWNERS: &[&str] = &[
    "user-attachments",
    "apps",
    "settings",
    "orgs",
    "features",
    "sponsors",
    "marketplace",
    "login",
    "notifications",
    "topics",
    "collections",
    "explore",
    "pulls",
    "issues",
    "search",
    "enterprise",
    "organizations",
    "about",
    "pricing",
    "codespaces",
    "copilot",
    "security",
    "site",
    "trending",
    "users",
];

pub fn repo_hints(text: &str) -> Vec<String> {
    GIT_REMOTE
        .captures_iter(text)
        .filter(|c| !RESERVED_OWNERS.contains(&c[1].to_lowercase().as_str()))
        .map(|c| format!("{}/{}", &c[1], &c[2]).to_lowercase())
        .collect()
}

pub fn cloud_ids(text: &str) -> Vec<String> {
    CLOUD_ID
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

/// PR references in a human prompt, as `(repo?, number)`.
pub fn refs_in_prompt(text: &str) -> Vec<(Option<String>, u32)> {
    let mut out: Vec<(Option<String>, u32)> = Vec::new();
    for (r, n) in urls_in(text) {
        out.push((Some(r), n));
    }
    for c in REPO_HASH.captures_iter(text) {
        if let Ok(n) = c[2].parse() {
            out.push((Some(c[1].to_string()), n));
        }
    }
    for c in PR_WORD_NUM.captures_iter(text) {
        if let Ok(n) = c[1].parse() {
            out.push((None, n));
        }
    }
    if HAS_PR_WORD.is_match(text) {
        for c in HASH_NUM.captures_iter(text) {
            if let Ok(n) = c[1].parse() {
                out.push((None, n));
            }
        }
    }
    // Drop bare numbers that are already covered by a repo-qualified ref.
    let qualified: Vec<u32> = out
        .iter()
        .filter(|(r, _)| r.is_some())
        .map(|(_, n)| *n)
        .collect();
    out.retain(|(r, n)| r.is_some() || !qualified.contains(n));
    out.sort();
    out.dedup();
    out
}

/// What a human prompt says about PRs: "/review 42" or "review <url>" => Reviewed.
pub fn from_prompt(prompt: &str) -> Vec<Pending> {
    let is_review_cmd = prompt.starts_with("/review")
        || prompt.starts_with("/code-review")
        || prompt.starts_with("/pr-review");
    let mut refs = refs_in_prompt(prompt);
    if is_review_cmd {
        // `/review 42`, `/code-review high 42 --comment`, but not `--max-findings 5`
        let words = shell_words(prompt);
        let (pos, _) = positionals(words.get(1..).unwrap_or(&[]));
        for (r, n) in pos.iter().filter_map(|w| parse_ref(w)) {
            if !refs.iter().any(|(_, m)| *m == n) {
                refs.push((r, n));
            }
        }
    }
    if refs.is_empty() {
        return vec![];
    }
    let role = if is_review_cmd {
        Role::Reviewed
    } else if REVIEW_WORD.is_match(prompt) {
        if ADDRESS_REVIEW.is_match(prompt) {
            Role::WorkedOn
        } else {
            Role::Reviewed
        }
    } else if prompt.contains("<event source=\"github\"") || prompt.contains("source=\"github\"") {
        Role::WorkedOn
    } else {
        Role::Mentioned
    };
    let ev = match role {
        Role::Reviewed => format!("you asked: {}", crate::text::truncate(prompt, 70)),
        Role::WorkedOn => format!("prompt: {}", crate::text::truncate(prompt, 70)),
        _ => "mentioned in a prompt".to_string(),
    };
    refs.into_iter()
        .map(|(r, n)| pending(r, Some(n), role, ev.clone()))
        .collect()
}

static HEREDOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<<-?\s*['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?"#).unwrap());

/// The parts of a shell command that actually run: heredoc bodies are dropped
/// and quoted strings blanked, so `cat <<EOF ... gh pr review 1 ... EOF` or
/// `echo "gh pr create"` don't count as PR actions.
pub fn shell_code(cmd: &str) -> String {
    // Same byte length as `cmd`, so positions found here index into the original:
    // heredoc bodies and the contents of quoted strings become spaces.
    let mut mask: Vec<u8> = cmd.as_bytes().to_vec();
    let mut pos = 0usize;
    let mut lines = cmd.split_inclusive('\n');
    while let Some(line) = lines.next() {
        let start = pos;
        pos += line.len();
        let _ = start;
        for c in HEREDOC.captures_iter(line) {
            let delim = c[1].to_string();
            for body in lines.by_ref() {
                let b0 = pos;
                pos += body.len();
                let done = body.trim() == delim;
                if !done {
                    for m in &mut mask[b0..pos] {
                        if *m != b'\n' {
                            *m = b' ';
                        }
                    }
                }
                if done {
                    break;
                }
            }
        }
    }
    let mut quote: Option<u8> = None;
    let mut prev = b' ';
    for m in mask.iter_mut() {
        let ch = *m;
        match quote {
            Some(q) if ch == q && !(q == b'"' && prev == b'\\') => quote = None,
            Some(_) => {
                if ch != b'\n' {
                    *m = b' ';
                }
            }
            None if ch == b'\'' || ch == b'"' => quote = Some(ch),
            None => {}
        }
        prev = ch;
    }
    String::from_utf8(mask).unwrap_or_default()
}

/// Split shell arguments, honouring quotes and backslashes.
pub fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    started = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// gh flags that take no value. Any other flag is assumed to take one, so the
/// next word is its value, not a PR number (`--interval 10`, `--max-findings 5`).
const BOOL_FLAGS: &[&str] = &[
    "-a",
    "--approve",
    "-c",
    "--comment",
    "-r",
    "--request-changes",
    "-s",
    "--squash",
    "-m",
    "--merge",
    "--rebase",
    "-d",
    "--delete-branch",
    "--auto",
    "--admin",
    "--disable-auto",
    "-w",
    "--web",
    "--watch",
    "--fail-fast",
    "--required",
    "--undo",
    "--comments",
    "--patch",
    "--name-only",
    "--detach",
    "-f",
    "--force",
    "--recurse-submodules",
    "--edit-last",
    "-y",
    "--yes",
    "--draft",
    "--fill",
    "--dry-run",
    "--no-maintainer-edit",
];

/// Positional arguments (flags and their values removed) and the `-R/--repo` value.
fn positionals(words: &[String]) -> (Vec<String>, Option<String>) {
    let mut pos = Vec::new();
    let mut repo = None;
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        if let Some(v) = w.strip_prefix("--repo=") {
            repo = Some(v.to_string());
        } else if w == "-R" || w == "--repo" {
            repo = words.get(i + 1).cloned();
            i += 1;
        } else if w.starts_with('-') && w.len() > 1 {
            if !w.contains('=') && !BOOL_FLAGS.contains(&w.as_str()) {
                i += 1; // skip the flag's value
            }
        } else {
            pos.push(w.clone());
        }
        i += 1;
    }
    let repo = repo.map(|r| {
        r.trim_start_matches("https://github.com/")
            .trim_end_matches('/')
            .to_lowercase()
    });
    (pos, repo)
}

/// PR actions in a shell command.
pub fn from_bash(cmd: &str) -> Vec<Pending> {
    let mut out = Vec::new();
    // Find commands in the masked text (so heredocs and strings don't count),
    // then read their arguments from the original, quotes and all.
    let code = shell_code(cmd);
    for c in GH_PR.captures_iter(&code) {
        let sub = &c[1];
        let span = c.get(2).unwrap().range();
        let words = shell_words(cmd.get(span).unwrap_or(""));
        let (pos, repo_flag) = positionals(&words);
        let mut repo = repo_flag;
        let mut number = None;
        if !matches!(sub, "create" | "new")
            && let Some(first) = pos.first()
            && let Some((r, n)) = parse_ref(first)
        {
            repo = repo.or(r);
            number = Some(n);
        }
        let role = match sub {
            "create" | "new" => Role::Created,
            "review" => Role::Reviewed,
            "comment" => Role::Commented,
            "merge" => Role::Merged,
            "checkout" | "co" | "edit" | "ready" | "close" | "reopen" => Role::WorkedOn,
            _ => Role::Viewed,
        };
        out.push(pending(repo, number, role, format!("gh pr {sub}")));
    }
    for c in GH_API.captures_iter(&code) {
        let span = c.get(1).unwrap().range();
        let args = cmd.get(span).unwrap_or("");
        let Some(p) = API_PATH.captures(args) else {
            continue;
        };
        // `{owner}/{repo}` and `:owner/:repo` mean "the current repo".
        let repo = match (p.get(1), p.get(2)) {
            (Some(o), Some(r)) => Some(format!("{}/{}", o.as_str(), r.as_str()).to_lowercase()),
            _ => None,
        };
        let kind = &p[3];
        let number: Option<u32> = p.get(4).and_then(|m| m.as_str().parse().ok());
        let tail = p.get(5).map(|m| m.as_str());
        let write = API_WRITE.is_match(args);
        let role = match (kind, number, tail, write) {
            ("pulls", None, None, true) => Role::Created,
            ("pulls", Some(_), Some("reviews"), true) => Role::Reviewed,
            ("pulls", Some(_), Some("comments"), true) => Role::Reviewed,
            ("pulls", Some(_), Some("merge"), true) => Role::Merged,
            ("issues", Some(_), Some("comments"), true) => Role::Commented,
            ("pulls", Some(_), _, true) => Role::WorkedOn,
            ("pulls", Some(_), _, false) => Role::Viewed,
            _ => continue,
        };
        out.push(pending(repo, number, role, format!("gh api {kind}")));
    }
    out
}

/// PR actions in an MCP / built-in tool call (`name` already stripped of `mcp__server__`).
pub fn from_tool(base: &str, input: &Value) -> Vec<Pending> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let n = |k: &str| {
        input.get(k).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    };
    let repo = match (s("owner"), s("repo")) {
        (Some(o), Some(r)) => Some(format!("{o}/{r}").to_lowercase()),
        _ => None,
    };
    let number = n("pullNumber")
        .or_else(|| n("pull_number"))
        .or_else(|| n("pr_number"))
        .or_else(|| n("prNumber"))
        .or_else(|| n("issue_number"))
        .or_else(|| n("number"))
        .map(|x| x as u32);
    let role = match base {
        "create_pull_request" => Role::Created,
        "pull_request_review_write"
        | "create_pending_pull_request_review"
        | "submit_pending_pull_request_review"
        | "create_and_submit_pull_request_review"
        | "add_comment_to_pending_review"
        | "add_pull_request_review_comment"
        | "add_pull_request_review_comment_to_pending_review"
        | "create_pull_request_review" => Role::Reviewed,
        "add_issue_comment" | "add_reply_to_pull_request_comment" => Role::Commented,
        "merge_pull_request" => Role::Merged,
        "update_pull_request"
        | "update_pull_request_branch"
        | "enable_pr_auto_merge"
        | "disable_pr_auto_merge"
        | "resolve_review_thread"
        | "unresolve_review_thread"
        | "request_copilot_review"
        | "subscribe_pr_activity" => Role::WorkedOn,
        "pull_request_read"
        | "get_pull_request"
        | "get_pull_request_diff"
        | "get_pull_request_files"
        | "get_pull_request_status"
        | "get_pull_request_comments"
        | "get_pull_request_reviews" => Role::Viewed,
        "Skill" | "SlashCommand" => {
            let skill = s("skill").or_else(|| s("command")).unwrap_or_default();
            if !skill.contains("review") {
                return vec![];
            }
            let args = s("args").unwrap_or_default();
            let refs: Vec<_> = if args.is_empty() {
                refs_in_prompt(&skill)
            } else {
                args.split_whitespace().filter_map(parse_ref).collect()
            };
            return refs
                .into_iter()
                .map(|(r, n)| pending(r, Some(n), Role::Reviewed, format!("ran /{skill}")))
                .collect();
        }
        _ => return vec![],
    };
    // An add_issue_comment with no number, or a create without a result, still needs the result.
    vec![pending(repo, number, role, base.replace('_', " "))]
}

/// Fill in a missing PR number (and repo) from the tool's output.
static ISSUE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/issues/(\d+)").unwrap()
});

pub fn complete(mut p: Pending, result: &str) -> Option<Pending> {
    // Commenting on an issue (not a PR) shares the tools and numbering; the result
    // URL tells them apart.
    if urls_in(result).is_empty()
        && let Some(c) = ISSUE_URL.captures(result)
    {
        let n: Option<u32> = c[1].parse().ok();
        if p.number.is_none() || p.number == n {
            return None;
        }
    }
    if p.number.is_none() || p.role == Role::Created {
        if let Some((r, n)) = urls_in(result).into_iter().next() {
            p.repo = p.repo.or(Some(r));
            p.number = Some(n);
        } else if p.number.is_none() {
            // e.g. "✓ Merged pull request owner/repo#12" or JSON {"number": 12}
            if let Some(c) = REPO_HASH.captures(result) {
                p.repo = p.repo.or(Some(c[1].to_lowercase()));
                p.number = c[2].parse().ok();
            } else if let Some(c) = JSON_NUMBER.captures(result) {
                p.number = c[1].parse().ok();
            } else if let Some(c) = HASH_NUM.captures(crate::text::cap_bytes(result, 400)) {
                p.number = c[1].parse().ok();
            }
        }
    }
    p.number.map(|_| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gh_pr_create_takes_number_from_output() {
        let p = from_bash("git push -u origin feat && gh pr create --title 'x' --body 'y'");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].role, Role::Created);
        let done = complete(p[0].clone(), "https://github.com/acme/web/pull/57\n").unwrap();
        assert_eq!(done.repo.as_deref(), Some("acme/web"));
        assert_eq!(done.number, Some(57));
    }

    #[test]
    fn gh_pr_review_with_number_and_repo() {
        let p = from_bash("gh pr review 12 --approve -R acme/web");
        assert_eq!(p[0].role, Role::Reviewed);
        assert_eq!(p[0].number, Some(12));
        assert_eq!(p[0].repo.as_deref(), Some("acme/web"));
    }

    #[test]
    fn gh_pr_list_limit_is_not_a_pr() {
        assert!(from_bash("gh pr list --limit 5").is_empty());
        let v = from_bash("gh pr view --json title -R a/b");
        assert_eq!(v[0].number, None);
    }

    #[test]
    fn gh_api_review_post() {
        let p = from_bash("gh api repos/acme/web/pulls/9/reviews -X POST -f event=APPROVE");
        assert_eq!(p[0].role, Role::Reviewed);
        assert_eq!(p[0].number, Some(9));
    }

    #[test]
    fn mcp_tools() {
        let p = from_tool(
            "create_pull_request",
            &json!({"owner":"a","repo":"b","title":"t"}),
        );
        assert_eq!(p[0].role, Role::Created);
        let done = complete(
            p[0].clone(),
            r#"{"number":3,"html_url":"https://github.com/a/b/pull/3"}"#,
        )
        .unwrap();
        assert_eq!(done.number, Some(3));
        let r = from_tool(
            "pull_request_review_write",
            &json!({"owner":"a","repo":"b","pullNumber":3,"method":"create"}),
        );
        assert_eq!(r[0].role, Role::Reviewed);
        assert_eq!(r[0].number, Some(3));
    }

    #[test]
    fn prompts() {
        let p = from_prompt("/review 42");
        assert_eq!(p[0].role, Role::Reviewed);
        assert_eq!(p[0].number, Some(42));
        let p = from_prompt("can you review https://github.com/a/b/pull/7 please");
        assert_eq!(p[0].role, Role::Reviewed);
        assert_eq!(p[0].repo.as_deref(), Some("a/b"));
        let p = from_prompt("address the review comments on PR #8");
        assert_eq!(p[0].role, Role::WorkedOn);
        assert!(from_prompt("bump version to 2 and fix #3 bug").is_empty());
    }

    #[test]
    fn ignores_heredocs_and_strings() {
        assert!(
            from_bash("cat > f.py <<'EOF'\nrun('gh pr review 13')\ngh pr create\nEOF\nls")
                .is_empty()
        );
        assert!(from_bash("echo \"gh pr create\" && grep 'gh pr merge 3' x").is_empty());
        assert_eq!(
            from_bash("cd x && gh pr merge 3 --squash")[0].number,
            Some(3)
        );
        assert_eq!(
            from_bash("URL=$(gh pr create --title 'a b' --body \"c\")")[0].role,
            Role::Created
        );
    }

    #[test]
    fn placeholders_quotes_and_flag_values() {
        let p = from_bash("gh api repos/{owner}/{repo}/pulls/44/reviews -X POST -f event=COMMENT");
        assert_eq!(
            (p[0].role, p[0].number, p[0].repo.clone()),
            (Role::Reviewed, Some(44), None)
        );
        let p = from_bash("gh pr view \"https://github.com/Acme/API/pull/45\"");
        assert_eq!(
            (p[0].number, p[0].repo.as_deref()),
            (Some(45), Some("acme/api"))
        );
        let p = from_bash("gh pr review 12 --body \"fixes #3 and #4\" --approve");
        assert_eq!(p[0].number, Some(12));
        assert_eq!(
            from_bash("gh pr checks --watch --interval 10")[0].number,
            None
        );
        assert!(from_prompt("/code-review high --max-findings 5").is_empty());
        assert_eq!(
            from_prompt("/code-review high 42 --comment")[0].number,
            Some(42)
        );
    }

    #[test]
    fn issues_are_not_prs() {
        let p = from_tool(
            "add_issue_comment",
            &json!({"owner":"o","repo":"x","issue_number":99}),
        );
        assert!(
            complete(
                p[0].clone(),
                r#"{"html_url":"https://github.com/o/x/issues/99#issuecomment-1"}"#
            )
            .is_none()
        );
        let p = from_tool(
            "add_issue_comment",
            &json!({"owner":"o","repo":"x","issue_number":7}),
        );
        assert!(
            complete(
                p[0].clone(),
                r#"{"html_url":"https://github.com/o/x/pull/7#issuecomment-1"}"#
            )
            .is_some()
        );
    }

    #[test]
    fn reserved_github_paths_are_not_repos() {
        assert!(urls_in("see https://github.com/user-attachments/assets/pull/21").is_empty());
        assert!(repo_hints("https://github.com/settings/tokens ").is_empty());
    }

    #[test]
    fn remotes() {
        assert_eq!(repo_hints("To github.com:acme/web.git\n"), vec!["acme/web"]);
        assert_eq!(
            repo_hints("https://github.com/acme/web.git"),
            vec!["acme/web"]
        );
    }
}
