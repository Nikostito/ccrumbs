//! Query parsing and ranking shared by the picker and `cctrail go`.

use crate::model::SessionRow;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// A parsed search query: PR filters (`#12`, `pr:12`, `acme/web#12`, PR URLs) plus free text.
#[derive(Debug, Default, Clone)]
pub struct Query {
    pub prs: Vec<(Option<String>, u32)>,
    pub text: String,
}

pub fn parse(q: &str) -> Query {
    let mut out = Query::default();
    let mut words = Vec::new();
    for w in q.split_whitespace() {
        let lw = w.to_lowercase();
        let pr = if let Some(n) = lw.strip_prefix("pr:").or_else(|| lw.strip_prefix("pr#")) {
            crate::prdetect::parse_ref(n)
        } else if w.starts_with('#') || w.contains("/pull/") || (w.contains('#') && w.contains('/'))
        {
            crate::prdetect::parse_ref(w)
        } else {
            None
        };
        match pr {
            Some(r) => out.prs.push(r),
            None => words.push(w),
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
    h
}

/// Longer text searched by plain substring when the fuzzy match misses.
pub fn body(s: &SessionRow) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        s.description(),
        s.auto_desc,
        s.first_prompt.as_deref().unwrap_or(""),
        s.last_prompt.as_deref().unwrap_or(""),
        s.files_edited.join(" ")
    )
    .to_lowercase()
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

impl Ranker {
    /// Indices of matching sessions, best first. `sessions` is assumed newest-first,
    /// so ties (and an empty query) keep recency order.
    pub fn rank(
        &mut self,
        sessions: &[SessionRow],
        hay: &[String],
        bodies: &[String],
        q: &Query,
    ) -> Vec<usize> {
        let pat = Pattern::parse(&q.text, CaseMatching::Ignore, Normalization::Smart);
        let words: Vec<String> = q
            .text
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let mut scored: Vec<(u32, usize)> = Vec::new();
        for (i, s) in sessions.iter().enumerate() {
            if !q.prs.is_empty() && !q.prs.iter().all(|(r, n)| linked(s, r.as_deref(), *n)) {
                continue;
            }
            if q.text.is_empty() {
                // PR-only query: creator first, then reviewer, ... then recency.
                let best = q
                    .prs
                    .iter()
                    .filter_map(|(r, n)| best_role(s, r.as_deref(), *n))
                    .min()
                    .map(|r| 10 - r as u32)
                    .unwrap_or(0);
                scored.push((best, i));
                continue;
            }
            let fuzzy = pat.score(Utf32Str::new(&hay[i], &mut self.buf), &mut self.matcher);
            let score = match fuzzy {
                Some(sc) => sc + 1000,
                None if words.iter().all(|w| bodies[i].contains(w.as_str())) => 500,
                None => continue,
            };
            // Gentle recency boost so that among similar matches, newer wins.
            let recency = 200u32.saturating_sub(i as u32);
            scored.push((score + recency, i));
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
