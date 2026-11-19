//! Group sessions that belong to the same piece of work.
//!
//! Two sessions are in the same task when they worked on the same
//! non-default git branch of the same repo, or when they both did something
//! concrete with the same PR (created, reviewed, merged, commented, worked on).

use crate::model::{Role, SessionRow};
use crate::prview::PrEntry;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Task {
    pub id: usize,
    /// Indices into the sessions slice, newest first.
    pub sessions: Vec<usize>,
    pub branch: Option<String>,
    pub project: String,
    /// Indices into the PR entries slice.
    pub prs: Vec<usize>,
    pub title: String,
    pub tickets: Vec<String>,
    pub cost_usd: f64,
}

pub fn is_default_branch(b: &str) -> bool {
    matches!(
        b,
        "main" | "master" | "develop" | "dev" | "trunk" | "HEAD" | ""
    )
}

struct Dsu(Vec<usize>);
impl Dsu {
    fn find(&mut self, x: usize) -> usize {
        let p = self.0[x];
        if p == x {
            return x;
        }
        let r = self.find(p);
        self.0[x] = r;
        r
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.0[ra.max(rb)] = ra.min(rb);
        }
    }
}

/// Tasks with at least two sessions, newest activity first, plus each session's task id.
pub fn build(sessions: &[SessionRow], prs: &[PrEntry]) -> (Vec<Task>, Vec<Option<usize>>) {
    let mut dsu = Dsu((0..sessions.len()).collect());
    let mut first_by_key: HashMap<String, usize> = HashMap::new();
    let mut join = |dsu: &mut Dsu, key: String, i: usize| match first_by_key.get(&key) {
        Some(&j) => dsu.union(i, j),
        None => {
            first_by_key.insert(key, i);
        }
    };
    for (i, s) in sessions.iter().enumerate() {
        if let Some(b) = s.branch.as_deref().filter(|b| !is_default_branch(b)) {
            let place = s
                .repo
                .clone()
                .unwrap_or_else(|| s.resume_dir())
                .to_lowercase();
            join(&mut dsu, format!("branch|{place}|{b}"), i);
        }
    }
    for (i, s) in sessions.iter().enumerate() {
        for t in &s.tickets {
            join(&mut dsu, format!("ticket|{t}"), i);
        }
    }
    let mut pr_sessions: Vec<Vec<usize>> = vec![vec![]; prs.len()];
    for (pi, e) in prs.iter().enumerate() {
        for l in &e.links {
            let concrete = matches!(
                l.role,
                Role::Created | Role::Reviewed | Role::Merged | Role::Commented | Role::WorkedOn
            );
            // A bare `#3` with no known repo could be any project's PR 3: don't join on it.
            if let (true, Some(i), false) = (concrete, l.session, e.repo.is_empty()) {
                join(
                    &mut dsu,
                    format!("pr|{}|{}", e.repo.to_lowercase(), e.number),
                    i,
                );
                if !pr_sessions[pi].contains(&i) {
                    pr_sessions[pi].push(i);
                }
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..sessions.len() {
        let r = dsu.find(i);
        groups.entry(r).or_default().push(i);
    }
    let mut members: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() >= 2).collect();
    for g in &mut members {
        g.sort(); // sessions are newest-first, so ascending index = newest first
    }
    members.sort_by_key(|g| g[0]);

    let mut task_of = vec![None; sessions.len()];
    let mut tasks = Vec::new();
    for (id, g) in members.into_iter().enumerate() {
        for &i in &g {
            task_of[i] = Some(id);
        }
        let task_prs: Vec<usize> = (0..prs.len())
            .filter(|&pi| pr_sessions[pi].iter().any(|i| g.contains(i)))
            .collect();
        // Most common non-default branch in the group.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for &i in &g {
            if let Some(b) = sessions[i]
                .branch
                .as_deref()
                .filter(|b| !is_default_branch(b))
            {
                *counts.entry(b).or_default() += 1;
            }
        }
        let branch = counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(b, _)| b.to_string());
        let oldest = *g.last().unwrap();
        let title = task_prs
            .iter()
            .filter_map(|&pi| prs[pi].title().map(str::to_string))
            .next()
            .or_else(|| branch.clone())
            .unwrap_or_else(|| sessions[oldest].title());
        let mut tickets: Vec<String> = Vec::new();
        for &i in &g {
            for t in &sessions[i].tickets {
                if !tickets.contains(t) {
                    tickets.push(t.clone());
                }
            }
        }
        let cost_usd = g.iter().map(|&i| sessions[i].cost_usd).sum();
        let no_pr_title = task_prs.iter().all(|&pi| prs[pi].title().is_none());
        let title = match (no_pr_title, tickets.first()) {
            (true, Some(key)) => ticket_title(key, branch.as_deref()),
            _ => title,
        };
        tasks.push(Task {
            tickets,
            cost_usd,
            id,
            project: sessions[oldest].project_name(),
            sessions: g,
            branch,
            prs: task_prs,
            title,
        });
    }
    (tasks, task_of)
}

/// `WEB-142: dark mode` from the key and a branch like `feature/WEB-142-dark-mode`.
fn ticket_title(key: &str, branch: Option<&str>) -> String {
    let rest = branch.and_then(|b| {
        // ASCII-only case folding keeps byte offsets valid in `b` (to_lowercase can
        // change byte lengths, e.g. for `İ`).
        let at = b.to_ascii_lowercase().find(&key.to_ascii_lowercase())?;
        let tail = b
            .get(at + key.len()..)?
            .trim_matches(|c: char| c == '-' || c == '_' || c == '/');
        (!tail.is_empty()).then(|| tail.replace(['-', '_', '/'], " "))
    });
    match rest {
        Some(r) => format!("{key}: {r}"),
        None => key.to_string(),
    }
}
