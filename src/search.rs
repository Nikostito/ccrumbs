//! Query parsing, filters and ranking shared by the picker and `ccrumbs go`.
//!
//! Query syntax (all parts optional, combined with AND):
//! - free words: fuzzy on title/project/branch/PR numbers, then substring on description & prompts
//! - `#12`, `pr:12`, `acme/web#12`, PR URL: sessions linked to that PR
//! - `p:api` project, `b:feat` branch, `f:ratelimit` edited file, `repo:acme/web`
//! - `@today`, `@yesterday`, `@week`, `@month`, `@3d`, `@2w`, `@12h`: last activity
//! - `has:pr`: sessions that created/reviewed/... a PR
//! - `task:N`: sessions of task N (what the Tasks tab uses)
//! - `t:ABC-123` / a bare `ABC-123`: sessions linked to that ticket
//! - `is:pinned`, `is:hidden` (hidden sessions are left out otherwise), `has:note`

use crate::context::Context;
use crate::model::SessionRow;
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

#[derive(Debug, Default, Clone)]
pub struct Query {
    pub prs: Vec<(Option<String>, u32)>,
    pub text: String,
    pub project: Vec<String>,
    pub branch: Vec<String>,
    pub file: Vec<String>,
    pub repo: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub has_pr: bool,
    pub task: Option<usize>,
    /// `ctx:>80` / `ctx:<30` (`ctx:80` means at least 80%).
    pub ctx_min: Option<u32>,
    pub ctx_max: Option<u32>,
    pub tickets: Vec<String>,
    pub pinned: bool,
    pub hidden: bool,
    pub has_note: bool,
}

fn start_of_day(days_ago: i64) -> DateTime<Utc> {
    let d = (Local::now() - Duration::days(days_ago)).date_naive();
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn parse_age(spec: &str) -> Option<(DateTime<Utc>, Option<DateTime<Utc>>)> {
    let now = Utc::now();
    Some(match spec {
        "today" => (start_of_day(0), None),
        "yesterday" => (start_of_day(1), Some(start_of_day(0))),
        "week" => (now - Duration::days(7), None),
        "month" => (now - Duration::days(30), None),
        _ => {
            let (n, unit) = spec.split_at(spec.find(|c: char| !c.is_ascii_digit())?);
            // Checked arithmetic: `@99999999d` must not panic.
            let n: i64 = n.parse().ok()?;
            let d = match unit {
                "h" => Duration::try_hours(n)?,
                "d" => Duration::try_days(n)?,
                "w" => Duration::try_weeks(n)?,
                "m" | "mo" => Duration::try_days(n.checked_mul(30)?)?,
                _ => return None,
            };
            (now.checked_sub_signed(d)?, None)
        }
    })
}

pub fn parse(q: &str) -> Query {
    let mut out = Query::default();
    let mut words = Vec::new();
    for w in q.split_whitespace() {
        let lw = w.to_lowercase();
        let val = |prefixes: &[&str]| {
            prefixes
                .iter()
                .find_map(|p| lw.strip_prefix(p))
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        if let Some(v) = val(&["p:", "project:"]) {
            out.project.push(v);
        } else if let Some(v) = val(&["b:", "branch:"]) {
            out.branch.push(v);
        } else if let Some(v) = val(&["f:", "file:"]) {
            out.file.push(v);
        } else if let Some(v) = val(&["repo:"]) {
            out.repo.push(v);
        } else if let Some(v) = val(&["task:"]).and_then(|v| v.parse().ok()) {
            out.task = Some(v);
        } else if let Some(v) = val(&["ctx:", "context:"]) {
            let num = |t: &str| t.trim_end_matches('%').parse::<u32>().ok();
            if let Some(n) = v.strip_prefix('<').and_then(num) {
                out.ctx_max = Some(n);
            } else if let Some(n) = v
                .strip_prefix('>')
                .map(|t| t.trim_start_matches('='))
                .and_then(num)
            {
                out.ctx_min = Some(n);
            } else if let Some(n) = num(&v) {
                out.ctx_min = Some(n);
            }
        } else if let Some(v) = val(&["t:", "ticket:"]) {
            out.tickets.push(v.to_uppercase());
        } else if lw == "is:pinned" || lw == "is:pin" {
            out.pinned = true;
        } else if lw == "is:hidden" {
            out.hidden = true;
        } else if lw == "has:note" {
            out.has_note = true;
        } else if crate::tickets::from_text(w).first().is_some_and(|k| k == w) {
            // An exact `ABC-123` typed on its own.
            out.tickets.push(w.to_string());
        } else if lw == "has:pr" || lw == "has:prs" {
            out.has_pr = true;
        } else if let Some((since, until)) = lw.strip_prefix('@').and_then(parse_age) {
            out.since = Some(since);
            out.until = until;
        } else if let Some(r) = val(&["pr:", "pr#"]).and_then(|n| crate::prdetect::parse_ref(&n)) {
            out.prs.push(r);
        } else if let Some(r) =
            (w.starts_with('#') || w.contains("/pull/") || (w.contains('#') && w.contains('/')))
                .then(|| crate::prdetect::parse_ref(w))
                .flatten()
        {
            out.prs.push(r);
        } else {
            words.push(w);
        }
    }
    out.text = words.join(" ");
    out
}

/// Short, high-signal text a fuzzy match runs against.
pub fn haystack(s: &SessionRow) -> String {
    let mut h = format!(
        "{} {} {} {} {}",
        s.title(),
        s.project_name(),
        s.branch.as_deref().unwrap_or(""),
        s.repo.as_deref().unwrap_or(""),
        s.id
    );
    for l in s.prs.iter().filter(|l| l.role.is_significant()) {
        h.push_str(&format!(" #{}", l.number));
    }
    for t in &s.tickets {
        h.push(' ');
        h.push_str(t);
    }
    h
}

/// Longer text searched by plain substring when the fuzzy match misses.
pub fn body(s: &SessionRow) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        s.note.as_deref().unwrap_or(""),
        s.description(),
        s.auto_desc,
        s.first_prompt.as_deref().unwrap_or(""),
        s.last_prompt.as_deref().unwrap_or(""),
        s.files_edited.join(" ")
    )
    .to_lowercase()
}

/// A one-off question: one prompt, nothing edited, no commits, no PR work.
pub fn is_quick(s: &SessionRow) -> bool {
    s.user_prompts <= 1
        && s.files_edited.is_empty()
        && s.commits == 0
        && !s.prs.iter().any(|l| l.role.is_significant())
}

/// Per-session data precomputed once per load.
pub struct Corpus {
    pub hay: Vec<String>,
    /// `hay`, lowercased, for plain substring matches the fuzzy scorer rates low
    /// (a word in the middle of a branch or project name).
    pub hay_lower: Vec<String>,
    pub bodies: Vec<String>,
    /// Closeness to the current context (see `context::TIER_*`).
    pub tier: Vec<u8>,
    pub task_of: Vec<Option<usize>>,
}

impl Corpus {
    pub fn new(
        sessions: &[SessionRow],
        ctx: Option<&Context>,
        task_of: Vec<Option<usize>>,
    ) -> Corpus {
        let hay: Vec<String> = sessions.iter().map(haystack).collect();
        Corpus {
            hay_lower: hay.iter().map(|h| h.to_lowercase()).collect(),
            hay,
            bodies: sessions.iter().map(body).collect(),
            tier: sessions
                .iter()
                .map(|s| ctx.map(|c| c.tier(s)).unwrap_or(0))
                .collect(),
            task_of,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    /// Rank sessions from the current repo/branch first.
    pub here_first: bool,
    pub hide_quick: bool,
}

pub struct Ranker {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Default for Ranker {
    fn default() -> Self {
        Ranker {
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
        }
    }
}

fn contains_any(hay: Option<&str>, needles: &[String]) -> bool {
    let h = hay.unwrap_or("").to_lowercase();
    needles.iter().all(|n| h.contains(n.as_str()))
}

/// Does a session pass every non-text filter in the query?
pub fn passes(s: &SessionRow, task: Option<usize>, q: &Query) -> bool {
    if !q.prs.iter().all(|(r, n)| linked(s, r.as_deref(), *n)) {
        return false;
    }
    if !q.project.is_empty() && !contains_any(Some(&s.resume_dir()), &q.project) {
        return false;
    }
    if !q.branch.is_empty() && !contains_any(s.branch.as_deref(), &q.branch) {
        return false;
    }
    if !q.repo.is_empty() && !contains_any(s.repo.as_deref(), &q.repo) {
        return false;
    }
    if !q.file.iter().all(|f| {
        s.files_edited
            .iter()
            .any(|e| e.to_lowercase().contains(f.as_str()))
    }) {
        return false;
    }
    if q.has_pr && !s.prs.iter().any(|l| l.role.is_significant()) {
        return false;
    }
    if q.task.is_some() && q.task != task {
        return false;
    }
    if s.hidden != q.hidden {
        return false;
    }
    if (q.pinned && !s.pinned) || (q.has_note && s.note.is_none()) {
        return false;
    }
    if !q
        .tickets
        .iter()
        .all(|t| s.tickets.iter().any(|k| k.eq_ignore_ascii_case(t)))
    {
        return false;
    }
    if q.ctx_min.is_some() || q.ctx_max.is_some() {
        let Some(pct) = s.context_pct() else {
            return false;
        };
        if q.ctx_min.is_some_and(|m| pct < m) || q.ctx_max.is_some_and(|m| pct > m) {
            return false;
        }
    }
    let last = s
        .ended
        .as_deref()
        .or(s.started.as_deref())
        .and_then(crate::text::parse_ts);
    if let Some(since) = q.since
        && last.is_none_or(|t| t < since)
    {
        return false;
    }
    if let Some(until) = q.until {
        let first = s.started.as_deref().and_then(crate::text::parse_ts);
        if first.is_none_or(|t| t >= until) {
            return false;
        }
    }
    true
}

impl Ranker {
    /// Indices of matching sessions, best first. `sessions` is newest-first,
    /// so ties (and an empty query) keep recency order.
    pub fn rank(&mut self, sessions: &[SessionRow], c: &Corpus, q: &Query, o: Opts) -> Vec<usize> {
        let pat = Pattern::parse(&q.text, CaseMatching::Ignore, Normalization::Smart);
        let words: Vec<String> = q
            .text
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        // Fuzzy matches that scatter letters across a long title ("r…e…d…i…s") score
        // below ~22 per typed character; ignore those and fall back to substring search.
        let min_fuzzy = 22 * q.text.chars().filter(|c| !c.is_whitespace()).count() as u32;
        let mut scored: Vec<(u32, usize)> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            if !passes(s, c.task_of[i], q) {
                continue;
            }
            // Quick one-offs are hidden unless the query asks for them by PR.
            if o.hide_quick && q.prs.is_empty() && is_quick(s) {
                continue;
            }
            let here = if o.here_first { c.tier[i] as u32 } else { 0 };
            // Pinned sessions float to the top of the default view.
            let pin = if s.pinned && q.text.is_empty() && q.prs.is_empty() {
                1_000_000
            } else {
                0
            };
            if q.text.is_empty() {
                // PR query: creator first, then reviewer, ...; then sessions from here; then recency.
                let role = q
                    .prs
                    .iter()
                    .filter_map(|(r, n)| best_role(s, r.as_deref(), *n))
                    .min()
                    .map(|r| 10 - r as u32)
                    .unwrap_or(0);
                scored.push((pin + role * 10 + here, i));
                continue;
            }
            let fuzzy = pat
                .score(Utf32Str::new(&c.hay[i], &mut self.buf), &mut self.matcher)
                .filter(|&sc| sc >= min_fuzzy);
            let score = match fuzzy {
                Some(sc) => sc + 1000,
                None if words.iter().all(|w| c.hay_lower[i].contains(w.as_str())) => 900,
                None if words.iter().all(|w| {
                    c.bodies[i].contains(w.as_str()) || c.hay_lower[i].contains(w.as_str())
                }) =>
                {
                    500
                }
                None => continue,
            };
            // Gentle boosts: newer wins among similar matches, and so do sessions from here.
            let recency = 200u32.saturating_sub(i as u32);
            scored.push((score + recency + here * 150, i));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, i)| i).collect()
    }
}

fn best_role(s: &SessionRow, repo: Option<&str>, n: u32) -> Option<crate::model::Role> {
    s.prs
        .iter()
        .filter(|l| l.number == n && repo_matches(&l.repo, repo))
        .map(|l| l.role)
        .min()
}

fn repo_matches(link_repo: &str, want: Option<&str>) -> bool {
    want.is_none_or(|r| {
        let (lr, r) = (link_repo.to_lowercase(), r.to_lowercase());
        lr == r || lr.ends_with(&format!("/{r}")) || lr.is_empty()
    })
}

pub fn linked(s: &SessionRow, repo: Option<&str>, n: u32) -> bool {
    s.prs
        .iter()
        .any(|l| l.number == n && repo_matches(&l.repo, repo))
}

/// Find a session by id prefix (or claude.ai `session_…` id).
pub fn by_id<'a>(sessions: &'a [SessionRow], q: &str) -> Vec<&'a SessionRow> {
    let q = q.trim().trim_start_matches("https://claude.ai/code/");
    if q.len() < 4 {
        return vec![];
    }
    sessions
        .iter()
        .filter(|s| s.id.starts_with(q) || s.cloud_id.as_deref().is_some_and(|c| c.starts_with(q)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PrLink, Role};

    fn sess(
        id: &str,
        cwd: &str,
        branch: &str,
        files: &[&str],
        prompts: u32,
        ago_h: i64,
    ) -> SessionRow {
        let t = (Utc::now() - Duration::hours(ago_h)).to_rfc3339();
        SessionRow {
            id: id.into(),
            cwd: Some(cwd.into()),
            branch: Some(branch.into()),
            first_prompt: Some(format!("work in {cwd}")),
            files_edited: files.iter().map(|f| f.to_string()).collect(),
            user_prompts: prompts,
            started: Some(t.clone()),
            ended: Some(t),
            ..Default::default()
        }
    }

    #[test]
    fn parses_filters() {
        let q = parse("p:api b:feat f:ratelimit has:pr @3d #88 bucket");
        assert_eq!(q.project, vec!["api"]);
        assert_eq!(q.branch, vec!["feat"]);
        assert_eq!(q.file, vec!["ratelimit"]);
        assert!(q.has_pr && q.since.is_some());
        assert_eq!(q.prs, vec![(None, 88)]);
        assert_eq!(q.text, "bucket");
        let q = parse("ABC-12 t:eng-3 is:pinned");
        assert_eq!(q.tickets, vec!["ABC-12", "ENG-3"]);
        assert!(q.pinned);
        let q = parse("ctx:>80 ctx:<95");
        assert_eq!((q.ctx_min, q.ctx_max), (Some(80), Some(95)));
    }

    #[test]
    fn mid_word_matches() {
        let mut s = sess(
            "aaaa1111",
            "/code/billingservice",
            "feat/ratelimit",
            &[],
            2,
            1,
        );
        s.first_prompt = Some("tweak".into());
        let sessions = vec![s];
        let corpus = Corpus::new(&sessions, None, vec![None]);
        for q in ["limit", "service", "servic", "ratelimit"] {
            assert_eq!(
                Ranker::default()
                    .rank(&sessions, &corpus, &parse(q), Opts::default())
                    .len(),
                1,
                "{q}"
            );
        }
    }

    #[test]
    fn filters_and_context() {
        let mut a = sess(
            "aaaa1111",
            "/code/api",
            "feat/rl",
            &["src/ratelimit.rs"],
            3,
            30,
        );
        a.prs.push(PrLink {
            repo: "acme/api".into(),
            number: 88,
            role: Role::Created,
            ts: None,
            evidence: String::new(),
        });
        let b = sess("bbbb2222", "/code/web", "main", &[], 1, 1);
        let c = sess("cccc3333", "/code/api", "main", &["README.md"], 2, 2);
        let sessions = vec![b, c, a]; // newest first
        let ctx = Context {
            root: "/code/api".into(),
            repo: None,
            branch: Some("feat/rl".into()),
        };
        let corpus = Corpus::new(&sessions, Some(&ctx), vec![None; 3]);
        let mut r = Ranker::default();
        let all = |q: &str, o: Opts| -> Vec<String> {
            let mut r2 = Ranker::default();
            r2.rank(&sessions, &corpus, &parse(q), o)
                .into_iter()
                .map(|i| sessions[i].id[..4].to_string())
                .collect()
        };
        // Empty query, here-first: same branch, then same repo, then the rest.
        let here = Opts {
            here_first: true,
            hide_quick: false,
        };
        let got: Vec<String> = r
            .rank(&sessions, &corpus, &Query::default(), here)
            .into_iter()
            .map(|i| sessions[i].id[..4].to_string())
            .collect();
        assert_eq!(got, ["aaaa", "cccc", "bbbb"]);
        assert_eq!(all("", Opts::default()), ["bbbb", "cccc", "aaaa"]);
        assert_eq!(all("f:ratelimit", Opts::default()), ["aaaa"]);
        assert_eq!(all("p:web", Opts::default()), ["bbbb"]);
        assert_eq!(all("has:pr", Opts::default()), ["aaaa"]);
        assert_eq!(all("@12h", Opts::default()), ["bbbb", "cccc"]);
        assert_eq!(
            all(
                "",
                Opts {
                    here_first: false,
                    hide_quick: true
                }
            ),
            ["cccc", "aaaa"]
        );
        // Scattered letters don't count as a fuzzy match; substring in the description does.
        assert_eq!(all("ratelimit", Opts::default()), ["aaaa"]);
        assert!(all("zqxj", Opts::default()).is_empty());
        assert!(parse("@99999999d").since.is_none());
        assert!(parse("@9999999999999w").since.is_none());
    }
}
