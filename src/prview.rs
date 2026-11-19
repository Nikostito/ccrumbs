//! Aggregate session↔PR links into one entry per PR.

use crate::model::{PrMeta, Role, SessionRow};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone)]
pub struct PrSession {
    /// Index into the sessions slice, when there's a local transcript.
    pub session: Option<usize>,
    /// claude.ai cloud session id, when known.
    pub cloud_id: Option<String>,
    pub role: Role,
    pub ts: Option<String>,
    pub evidence: String,
}

#[derive(Debug, Clone)]
pub struct PrEntry {
    pub repo: String,
    pub number: u32,
    pub meta: Option<PrMeta>,
    pub links: Vec<PrSession>,
}

/// Identity of whoever did something: a local session or a cloud session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Actor {
    Local(usize),
    Cloud(String),
}

impl PrEntry {
    pub fn key(&self) -> String {
        if self.repo.is_empty() {
            format!("#{}", self.number)
        } else {
            format!("{}#{}", self.repo, self.number)
        }
    }

    fn actors(&self, role: Role) -> Vec<Actor> {
        let mut v: Vec<Actor> = Vec::new();
        for l in self.links.iter().filter(|l| l.role == role) {
            let a = match (l.session, &l.cloud_id) {
                (Some(i), _) => Actor::Local(i),
                (None, Some(c)) => Actor::Cloud(c.clone()),
                _ => continue,
            };
            if !v.contains(&a) {
                v.push(a);
            }
        }
        v
    }

    pub fn creators(&self) -> Vec<Actor> {
        self.actors(Role::Created)
    }

    pub fn reviewers(&self) -> Vec<Actor> {
        self.actors(Role::Reviewed)
    }

    /// Opened by a Claude session (seen in a transcript, linked from the PR body, or with the Claude Code footer).
    pub fn born_from_claude(&self) -> bool {
        !self.creators().is_empty()
            || self
                .meta
                .as_ref()
                .is_some_and(|m| m.claude_footer || m.body_session.is_some())
    }

    pub fn title(&self) -> Option<&str> {
        self.meta.as_ref().and_then(|m| m.title.as_deref())
    }

    pub fn state(&self) -> Option<&str> {
        self.meta.as_ref().and_then(|m| m.state.as_deref())
    }

    pub fn last_ts(&self) -> Option<&str> {
        self.links
            .iter()
            .filter_map(|l| l.ts.as_deref())
            .max()
            .or_else(|| self.meta.as_ref().and_then(|m| m.created_at.as_deref()))
    }

    /// One row per session: its strongest role on this PR, plus the other roles it had.
    pub fn grouped(&self) -> Vec<(&PrSession, Vec<Role>)> {
        let mut out: Vec<(&PrSession, Vec<Role>)> = Vec::new();
        for l in &self.links {
            let same = |o: &PrSession| match (o.session, l.session) {
                (Some(a), Some(b)) => a == b,
                (None, None) => o.cloud_id == l.cloud_id,
                _ => false,
            };
            match out.iter_mut().find(|(o, _)| same(o)) {
                Some((o, also)) if o.role != l.role => {
                    if l.role != Role::Mentioned && !also.contains(&l.role) {
                        also.push(l.role);
                    }
                }
                Some(_) => {}
                None => out.push((l, vec![])),
            }
        }
        out
    }

    pub fn has_significant(&self) -> bool {
        self.links.iter().any(|l| l.role.is_significant()) || self.born_from_claude()
    }

    /// One-line story: who created it, who reviewed it, and whether those were different sessions.
    pub fn story(&self, sessions: &[SessionRow]) -> String {
        let name = |a: &Actor| match a {
            Actor::Local(i) => format!(
                "{} \"{}\"",
                sessions[*i].short_id(),
                crate::text::truncate(&sessions[*i].title(), 40)
            ),
            Actor::Cloud(c) => format!("cloud {}", &c[..c.len().min(16)]),
        };
        let c = self.creators();
        let r = self.reviewers();
        let list = |v: &[Actor]| v.iter().map(name).collect::<Vec<_>>().join(", ");
        match (c.is_empty(), r.is_empty()) {
            (false, false) => {
                let same: Vec<&Actor> = r.iter().filter(|x| c.contains(x)).collect();
                let other: Vec<Actor> = r.iter().filter(|x| !c.contains(x)).cloned().collect();
                if other.is_empty() {
                    format!("Created and reviewed in the SAME session: {}", list(&c))
                } else if same.is_empty() {
                    format!(
                        "Created by {} · reviewed by a DIFFERENT session: {}",
                        list(&c),
                        list(&other)
                    )
                } else {
                    format!(
                        "Created by {} (self-reviewed) · also reviewed by a DIFFERENT session: {}",
                        list(&c),
                        list(&other)
                    )
                }
            }
            (false, true) => format!("Created by {} · no reviewing session found", list(&c)),
            (true, false) => format!(
                "Reviewed by {} · created outside your Claude sessions",
                list(&r)
            ),
            (true, true) => {
                if self.born_from_claude() {
                    "Opened by Claude (per GitHub) · no local transcript".to_string()
                } else {
                    let roles: Vec<&str> = {
                        let mut v: Vec<Role> = self.links.iter().map(|l| l.role).collect();
                        v.sort();
                        v.dedup();
                        v.into_iter().map(|r| r.as_str()).collect()
                    };
                    format!("Touched by your sessions ({})", roles.join(", "))
                }
            }
        }
    }
}

fn entry<'m>(map: &'m mut BTreeMap<(String, u32), PrEntry>, repo: &str, n: u32) -> &'m mut PrEntry {
    map.entry((repo.to_string(), n)).or_insert_with(|| PrEntry {
        repo: repo.to_string(),
        number: n,
        meta: None,
        links: vec![],
    })
}

pub fn build(sessions: &[SessionRow], prs: &[PrMeta]) -> Vec<PrEntry> {
    let mut map: BTreeMap<(String, u32), PrEntry> = BTreeMap::new();
    for (i, s) in sessions.iter().enumerate() {
        for l in &s.prs {
            entry(&mut map, &l.repo, l.number).links.push(PrSession {
                session: Some(i),
                cloud_id: s.cloud_id.clone(),
                role: l.role,
                ts: l.ts.clone(),
                evidence: l.evidence.clone(),
            });
        }
    }
    let by_cloud: HashMap<&str, usize> = sessions
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.cloud_id.as_deref().map(|c| (c, i)))
        .collect();
    for m in prs {
        let known = map.contains_key(&(m.repo.clone(), m.number));
        let interesting = known || m.claude_footer || m.body_session.is_some();
        // Sessions that worked on this PR's branch in the same repo.
        let branch_sessions: Vec<usize> = match &m.head {
            Some(h) if !matches!(h.as_str(), "main" | "master" | "develop" | "dev") => sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.repo.as_deref() == Some(m.repo.as_str())
                        && s.branch.as_deref() == Some(h.as_str())
                })
                .map(|(i, _)| i)
                .collect(),
            _ => vec![],
        };
        if !interesting && branch_sessions.is_empty() {
            continue;
        }
        let e = entry(&mut map, &m.repo, m.number);
        e.meta = Some(m.clone());
        if let Some(cid) = &m.body_session {
            let local = by_cloud.get(cid.as_str()).copied();
            let already = e
                .links
                .iter()
                .any(|l| l.role == Role::Created && (l.session.is_some() && l.session == local));
            if !already {
                e.links.push(PrSession {
                    session: local,
                    cloud_id: Some(cid.clone()),
                    role: Role::Created,
                    ts: m.created_at.clone(),
                    evidence: "PR body links this claude.ai session".into(),
                });
            }
        }
        for i in branch_sessions {
            if !e.links.iter().any(|l| l.session == Some(i)) {
                e.links.push(PrSession {
                    session: Some(i),
                    cloud_id: sessions[i].cloud_id.clone(),
                    role: Role::WorkedOn,
                    ts: sessions[i].ended.clone(),
                    evidence: format!(
                        "worked on the PR branch {}",
                        m.head.as_deref().unwrap_or("")
                    ),
                });
            }
        }
    }
    let mut out: Vec<PrEntry> = map.into_values().collect();
    for e in &mut out {
        e.links
            .sort_by(|a, b| (a.role, &a.ts).cmp(&(b.role, &b.ts)));
    }
    out.sort_by(|a, b| b.last_ts().cmp(&a.last_ts()));
    out
}

/// Find PRs matching a user-typed ref: `12`, `#12`, `owner/repo#12`, `repo#12`, or a PR URL.
pub fn lookup<'a>(entries: &'a [PrEntry], q: &str) -> Vec<&'a PrEntry> {
    let q = q.trim();
    let (repo, num) = if let Some((r, n)) = q.rsplit_once('#').filter(|(r, _)| !r.is_empty()) {
        (Some(r.to_lowercase()), n.parse::<u32>().ok())
    } else {
        match crate::prdetect::parse_ref(q) {
            Some((r, n)) => (r.map(|r| r.to_lowercase()), Some(n)),
            None => (None, None),
        }
    };
    let Some(num) = num else { return vec![] };
    entries
        .iter()
        .filter(|e| e.number == num)
        .filter(|e| match &repo {
            None => true,
            Some(r) => {
                let er = e.repo.to_lowercase();
                er == *r || er.ends_with(&format!("/{r}")) || e.repo.is_empty()
            }
        })
        .collect()
}
