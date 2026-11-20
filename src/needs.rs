//! "Needs you": sessions whose pull request has since got failing CI, a merge
//! conflict, a review or comment, or is ready to merge.
//!
//! The status comes from `gh pr view` (see [`crate::github::fetch_status`]), refreshed
//! by `ccrumbs needs` and in the background by the picker. Only open PRs that one of
//! your sessions created or worked on are checked.

use crate::db::Db;
use crate::model::{PrMeta, Role, SessionRow};
use anyhow::Result;
use chrono::{Duration, Utc};
use std::collections::HashMap;

/// Only PRs touched by a session in this many days are checked.
const RECENT_DAYS: i64 = 30;
/// At most this many PRs per refresh.
const MAX_PRS: usize = 40;

fn owns(role: Role) -> bool {
    matches!(role, Role::Created | Role::WorkedOn)
}

/// For every PR your sessions created or worked on: the latest such session.
fn owners(sessions: &[SessionRow]) -> HashMap<(String, u32), usize> {
    let mut out: HashMap<(String, u32), usize> = HashMap::new();
    // `sessions` is newest first, so the first one seen is the latest.
    for (i, s) in sessions.iter().enumerate() {
        for l in s.prs.iter().filter(|l| owns(l.role)) {
            let repo = if l.repo.is_empty() {
                s.repo.clone().unwrap_or_default().to_lowercase()
            } else {
                l.repo.clone()
            };
            if repo.is_empty() {
                continue;
            }
            out.entry((repo, l.number)).or_insert(i);
        }
    }
    out
}

/// PRs whose status is older than `max_age` (or unknown) and worth checking: open or
/// not yet synced, and touched by one of your sessions recently. Newest first.
pub fn stale(sessions: &[SessionRow], prs: &[PrMeta], max_age: Duration) -> Vec<(String, u32)> {
    let meta: HashMap<(&str, u32), &PrMeta> = prs
        .iter()
        .map(|p| ((p.repo.as_str(), p.number), p))
        .collect();
    let cutoff = Utc::now() - Duration::days(RECENT_DAYS);
    let fresh_after = Utc::now() - max_age;
    let mut list: Vec<((String, u32), usize)> = owners(sessions)
        .into_iter()
        .filter(|((repo, n), i)| {
            let s = &sessions[*i];
            let recent = s
                .ended
                .as_deref()
                .and_then(crate::text::parse_ts)
                .is_some_and(|t| t >= cutoff);
            let m = meta.get(&(repo.as_str(), *n));
            let open = m
                .and_then(|m| m.state.as_deref())
                .is_none_or(|st| st == "open");
            let fresh = m
                .and_then(|m| m.status.as_ref())
                .and_then(|st| crate::text::parse_ts(&st.synced))
                .is_some_and(|t| t >= fresh_after);
            recent && open && !fresh
        })
        .collect();
    list.sort_by_key(|(_, i)| *i);
    list.into_iter().take(MAX_PRS).map(|(k, _)| k).collect()
}

/// Fetch the status of `prs` from GitHub (a few at a time) and store it.
/// Returns how many were updated and the first error, if any.
pub fn refresh(db: &Db, prs: &[(String, u32)]) -> (usize, Option<String>) {
    use rayon::prelude::*;
    let fetch = || -> Vec<Result<PrMeta>> {
        prs.par_iter()
            .map(|(r, n)| crate::github::fetch_status(r, *n))
            .collect()
    };
    let results = match rayon::ThreadPoolBuilder::new().num_threads(6).build() {
        Ok(pool) => pool.install(fetch),
        Err(_) => fetch(),
    };
    let mut ok = 0;
    let mut err = None;
    for r in results {
        match r.and_then(|m| db.upsert_pr(&m)) {
            Ok(()) => ok += 1,
            Err(e) => {
                err.get_or_insert_with(|| format!("{e:#}"));
            }
        }
    }
    (ok, err)
}

/// Fill in `needs` on each session from the stored PR status (no network).
pub fn annotate(sessions: &mut [SessionRow], prs: &[PrMeta]) {
    let owners = owners(sessions);
    let mut found: Vec<(usize, String)> = Vec::new();
    for p in prs {
        let (Some(st), Some("open")) = (&p.status, p.state.as_deref()) else {
            continue;
        };
        let Some(&i) = owners.get(&(p.repo.clone(), p.number)) else {
            continue;
        };
        let pr = format!("#{}", p.number);
        let mut why = Vec::new();
        if st.checks == "fail" {
            why.push(format!("CI failing on {pr}"));
        }
        if st.mergeable == "CONFLICTING" {
            why.push(format!("merge conflict on {pr}"));
        }
        if st.review_decision == "CHANGES_REQUESTED" {
            why.push(format!("changes requested on {pr}"));
        }
        let ended = sessions[i].ended.as_deref().and_then(crate::text::parse_ts);
        let fb = st.feedback_at.as_deref().and_then(crate::text::parse_ts);
        if let (Some(fb), Some(ended)) = (fb, ended)
            && fb > ended
            && st.review_decision != "CHANGES_REQUESTED"
        {
            let who = st.feedback_by.as_deref().unwrap_or("someone");
            why.push(format!("new feedback from {who} on {pr}"));
        }
        if why.is_empty()
            && st.review_decision == "APPROVED"
            && st.checks != "fail"
            && st.checks != "pending"
            && st.mergeable == "MERGEABLE"
        {
            why.push(format!("{pr} approved, ready to merge"));
        }
        found.extend(why.into_iter().map(|w| (i, w)));
    }
    for (i, w) in found {
        sessions[i].needs.push(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PrLink, PrStatus};

    fn sess(id: &str, ended: &str, role: Role) -> SessionRow {
        SessionRow {
            id: id.into(),
            ended: Some(ended.into()),
            repo: Some("acme/web".into()),
            prs: vec![PrLink {
                repo: String::new(),
                number: 7,
                role,
                ts: None,
                evidence: String::new(),
            }],
            ..Default::default()
        }
    }

    fn pr(state: &str, st: PrStatus) -> PrMeta {
        PrMeta {
            repo: "acme/web".into(),
            number: 7,
            state: Some(state.into()),
            status: Some(st),
            ..Default::default()
        }
    }

    #[test]
    fn reasons_go_to_the_latest_owning_session() {
        let mut s = vec![
            sess("new", "2026-11-18T10:00:00Z", Role::WorkedOn),
            sess("old", "2026-11-17T10:00:00Z", Role::Created),
            sess("viewer", "2026-11-19T10:00:00Z", Role::Viewed),
        ];
        // newest first, as loaded
        s.swap(0, 2);
        let st = PrStatus {
            checks: "fail".into(),
            mergeable: "MERGEABLE".into(),
            feedback_at: Some("2026-11-18T12:00:00Z".into()),
            feedback_by: Some("bob".into()),
            ..Default::default()
        };
        annotate(&mut s, &[pr("open", st)]);
        assert!(s[0].needs.is_empty()); // only viewed it
        assert_eq!(
            s[1].needs,
            vec!["CI failing on #7", "new feedback from bob on #7"]
        );
        assert!(s[2].needs.is_empty());
    }

    #[test]
    fn closed_prs_and_old_feedback_are_quiet() {
        let mut s = vec![sess("a", "2026-11-18T10:00:00Z", Role::Created)];
        let st = PrStatus {
            feedback_at: Some("2026-11-18T09:00:00Z".into()),
            mergeable: "MERGEABLE".into(),
            ..Default::default()
        };
        annotate(&mut s, &[pr("open", st.clone())]);
        assert!(s[0].needs.is_empty());
        let failing = PrStatus {
            checks: "fail".into(),
            ..st
        };
        annotate(&mut s, &[pr("merged", failing)]);
        assert!(s[0].needs.is_empty());
    }

    #[test]
    fn approved_and_green_is_ready_to_merge() {
        let mut s = vec![sess("a", "2026-11-18T10:00:00Z", Role::Created)];
        let st = PrStatus {
            review_decision: "APPROVED".into(),
            checks: "pass".into(),
            mergeable: "MERGEABLE".into(),
            ..Default::default()
        };
        annotate(&mut s, &[pr("open", st)]);
        assert_eq!(s[0].needs, vec!["#7 approved, ready to merge"]);
    }
}
