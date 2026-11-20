//! Core data types shared across the indexer, database, and UI.

use serde::{Deserialize, Serialize};

/// How a session relates to a pull request, ordered from strongest to weakest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    Created,
    Reviewed,
    Merged,
    Commented,
    WorkedOn,
    Viewed,
    Mentioned,
}

impl Role {
    pub const ALL: [Role; 7] = [
        Role::Created,
        Role::Reviewed,
        Role::Merged,
        Role::Commented,
        Role::WorkedOn,
        Role::Viewed,
        Role::Mentioned,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Created => "created",
            Role::Reviewed => "reviewed",
            Role::Merged => "merged",
            Role::Commented => "commented",
            Role::WorkedOn => "worked-on",
            Role::Viewed => "viewed",
            Role::Mentioned => "mentioned",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// Past-tense verb phrase used in human-readable output.
    pub fn verb(self) -> &'static str {
        match self {
            Role::Created => "CREATED",
            Role::Reviewed => "REVIEWED",
            Role::Merged => "MERGED",
            Role::Commented => "COMMENTED",
            Role::WorkedOn => "WORKED ON",
            Role::Viewed => "VIEWED",
            Role::Mentioned => "MENTIONED",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Role::Created => "●",
            Role::Reviewed => "♦",
            Role::Merged => "√",
            Role::Commented => "»",
            Role::WorkedOn => "►",
            Role::Viewed => "·",
            Role::Mentioned => "·",
        }
    }

    /// Roles worth showing as badges in a compact list row.
    pub fn is_significant(self) -> bool {
        matches!(
            self,
            Role::Created | Role::Reviewed | Role::Merged | Role::Commented | Role::WorkedOn
        )
    }
}

/// One link between a session and a PR, discovered in the transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrLink {
    /// `owner/repo`, or empty when it could not be determined.
    pub repo: String,
    pub number: u32,
    pub role: Role,
    pub ts: Option<String>,
    /// Short human-readable note on why this link was recorded.
    pub evidence: String,
}

/// One item of Claude's to-do list (TodoWrite, or TaskCreate/TaskUpdate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub text: String,
    /// `pending`, `in_progress` or `completed`.
    pub status: String,
}

impl Todo {
    pub fn is_open(&self) -> bool {
        self.status != "completed"
    }
}

/// Everything extracted from one transcript file.
#[derive(Debug, Clone, Default)]
pub struct ParsedSession {
    pub id: String,
    pub path: String,
    pub project_dir: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub repo: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
    pub user_prompts: u32,
    pub assistant_msgs: u32,
    pub custom_title: Option<String>,
    pub summary: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    /// First handful of cleaned user prompts (for descriptions).
    pub prompts: Vec<String>,
    pub files_edited: Vec<String>,
    pub commands_run: u32,
    pub tests_ran: bool,
    pub commits: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Context size (tokens) at the last main-thread reply.
    pub context_tokens: u64,
    /// Times the conversation was compacted.
    pub compactions: u32,
    /// Claude Code's own cost tally for the session, summed over runs (resumes).
    pub cost_usd: f64,
    pub tickets: Vec<crate::tickets::Candidate>,
    pub model: Option<String>,
    pub entrypoint: Option<String>,
    /// `session_...` id of the claude.ai cloud session, if the transcript reveals one.
    pub cloud_id: Option<String>,
    pub last_reply: Option<String>,
    /// Claude's to-do list as it stood at the end of the session.
    pub todos: Vec<Todo>,
    /// Local calendar days (`YYYY-MM-DD`) with activity, for the work log.
    pub days: Vec<String>,
    pub fulltext: String,
    pub pr_links: Vec<PrLink>,
    /// Repo hints seen in tool output (e.g. `git push` lines), used to resolve bare `#N`.
    pub repo_hints: Vec<String>,
    pub mtime: i64,
    pub size: i64,
}

/// A session row as loaded from the database for display.
#[derive(Debug, Clone, Default)]
pub struct SessionRow {
    pub id: String,
    pub path: String,
    pub project_dir: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub repo: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
    pub user_prompts: u32,
    pub custom_title: Option<String>,
    pub summary: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub files_edited: Vec<String>,
    pub tests_ran: bool,
    pub commits: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub context_tokens: u64,
    pub compactions: u32,
    pub cost_usd: f64,
    /// Issue-tracker keys (Jira/Linear) linked to the session.
    pub tickets: Vec<String>,
    pub pinned: bool,
    pub hidden: bool,
    pub note: Option<String>,
    pub model: Option<String>,
    pub entrypoint: Option<String>,
    pub cloud_id: Option<String>,
    pub auto_desc: String,
    pub ai_title: Option<String>,
    pub ai_desc: Option<String>,
    pub prs: Vec<PrLink>,
    /// Transcript file no longer on disk (purged by Claude Code's retention).
    pub missing: bool,
    /// Claude's last reply (capped).
    pub last_reply: Option<String>,
    pub todos: Vec<Todo>,
    /// Local calendar days (`YYYY-MM-DD`) with activity.
    pub days: Vec<String>,
    /// Open right now in a running `claude` process (filled in at load time).
    pub live: Option<crate::live::Live>,
    /// Why this session needs you (its PR has failing CI, a conflict, new feedback…).
    pub needs: Vec<String>,
}

impl SessionRow {
    /// Best available title: your own rename > AI title > Claude's summary > first prompt.
    pub fn title(&self) -> String {
        self.custom_title
            .clone()
            .or_else(|| self.ai_title.clone())
            .or_else(|| self.summary.clone())
            .or_else(|| {
                self.first_prompt
                    .as_deref()
                    .map(|p| crate::text::truncate(p, 80))
            })
            .unwrap_or_else(|| "(empty session)".to_string())
    }

    /// Best available description: AI-written if present, else the heuristic one.
    pub fn description(&self) -> &str {
        self.ai_desc.as_deref().unwrap_or(&self.auto_desc)
    }

    /// Directory to resume in: exact cwd from the transcript, else decoded folder name.
    pub fn resume_dir(&self) -> String {
        self.cwd
            .clone()
            .unwrap_or_else(|| crate::text::decode_project_dir(&self.project_dir))
    }

    pub fn project_name(&self) -> String {
        crate::text::file_name(&self.resume_dir()).to_string()
    }

    /// One entry per PR: the strongest role, plus the other roles this session had on it.
    /// Mentions are dropped when the session did something more concrete with the PR.
    pub fn pr_summary(&self) -> Vec<(&PrLink, Vec<Role>)> {
        let mut out: Vec<(&PrLink, Vec<Role>)> = Vec::new();
        let mut sorted: Vec<&PrLink> = self.prs.iter().collect();
        sorted.sort_by_key(|l| l.role);
        for l in sorted {
            match out
                .iter_mut()
                .find(|(b, _)| b.repo == l.repo && b.number == l.number)
            {
                Some((_, others)) => {
                    if l.role != Role::Mentioned && !others.contains(&l.role) {
                        others.push(l.role);
                    }
                }
                None => out.push((l, vec![])),
            }
        }
        out.retain(|(l, _)| l.role != Role::Mentioned);
        out
    }

    /// How full the context window was when the session ended, in percent.
    pub fn context_pct(&self) -> Option<u32> {
        if self.context_tokens == 0 {
            return None;
        }
        let window = context_window(self.model.as_deref().unwrap_or(""));
        Some(
            ((self.context_tokens as f64 / window as f64) * 100.0)
                .round()
                .min(999.0) as u32,
        )
    }

    /// "389k / 1M (39%) · compacted 2×", or None without usage data.
    pub fn context_summary(&self) -> Option<String> {
        let pct = self.context_pct()?;
        let window = context_window(self.model.as_deref().unwrap_or(""));
        let mut out = format!(
            "context {} / {} ({pct}%)",
            crate::text::tokens(self.context_tokens),
            crate::text::tokens(window)
        );
        if self.compactions > 0 {
            out.push_str(&format!(" · compacted {}×", self.compactions));
        }
        Some(out)
    }

    /// Pair a freshly loaded row with its stored ticket candidates (resolved later).
    pub fn with_ticket_candidates(
        self,
        json: Option<String>,
    ) -> (SessionRow, Vec<crate::tickets::Candidate>) {
        let c = json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        (self, c)
    }

    /// The first `n` lines of Claude's last reply, without Markdown decoration.
    pub fn reply_excerpt(&self, n: usize) -> Vec<String> {
        self.last_reply
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches(['#', '>', ' '])
                    .replace("**", "")
                    .replace('`', "")
            })
            .filter(|l| !l.is_empty() && !l.starts_with("```") && l != "---")
            .take(n)
            .collect()
    }

    /// To-dos not done yet, the one in progress first.
    pub fn open_todos(&self) -> Vec<&Todo> {
        let mut v: Vec<&Todo> = self.todos.iter().filter(|t| t.is_open()).collect();
        v.sort_by_key(|t| t.status != "in_progress");
        v
    }

    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
}

/// PR metadata pulled from GitHub by `ccrumbs sync`.
#[derive(Debug, Clone, Default)]
pub struct PrMeta {
    pub repo: String,
    pub number: u32,
    pub title: Option<String>,
    pub state: Option<String>,
    pub author: Option<String>,
    pub url: Option<String>,
    pub head: Option<String>,
    pub created_at: Option<String>,
    /// `session_...` id found in the PR body (claude.ai cloud sessions add this).
    pub body_session: Option<String>,
    /// PR body carries the "Generated with Claude Code" footer.
    pub claude_footer: bool,
    /// Live status, from `ccrumbs needs` / the picker's background refresh.
    pub status: Option<PrStatus>,
}

/// What a PR is waiting on, as of `synced`.
#[derive(Debug, Clone, Default)]
pub struct PrStatus {
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED` or empty.
    pub review_decision: String,
    /// `pass`, `fail`, `pending`, or empty when there are no checks.
    pub checks: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    pub mergeable: String,
    /// Latest review or comment by someone other than the PR's author.
    pub feedback_at: Option<String>,
    pub feedback_by: Option<String>,
    pub synced: String,
}

/// Context window of a model, in tokens. `context_windows` in the config overrides this
/// (keys are matched as substrings of the model id).
pub fn context_window(model: &str) -> u64 {
    if let Some(n) = crate::config::context_override(model) {
        return n;
    }
    let m = model.to_lowercase();
    let million = m.ends_with("[1m]")
        || [
            "opus-5",
            "sonnet-5",
            "haiku-5",
            "fable",
            "mythos",
            "opus-4-6",
            "opus-4-7",
            "opus-4-8",
            "sonnet-4-6",
        ]
        .iter()
        .any(|k| m.contains(k));
    if million { 1_000_000 } else { 200_000 }
}
