//! Issue-tracker keys (Jira `ABC-123`, Linear `ENG-42`, ...) found in a session.
//!
//! A key in the git branch name is trusted. A key that only shows up in a
//! prompt or title counts when its prefix is configured in `tickets`, or was
//! seen in some session's branch name; that keeps `UTF-8` & co. out.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub key: String,
    /// Found in the branch name.
    pub strong: bool,
}

static IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Z][A-Z0-9]{1,9})-([1-9][0-9]{0,5})\b").unwrap());
static IN_BRANCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[/_.-])([a-z][a-z0-9]{1,9})-([1-9][0-9]{0,5})(?:$|[/_.-])").unwrap()
});

const STOP: &[&str] = &[
    "UTF", "SHA", "ISO", "HTTP", "HTTPS", "RFC", "CVE", "TLS", "SSL", "AES", "RSA", "MD", "GPT",
    "CLAUDE", "OPUS", "SONNET", "HAIKU", "FABLE", "MYTHOS", "RELEASE", "VERSION", "HOTFIX",
    "BUILD", "PR", "PATCH", "RC", "PY", "NODE", "STEP", "PHASE", "PART", "DAY", "WEEK", "TOP",
    "ES", "UUID", "BASE", "WIN", "COVID", "COMMIT", "SPRINT", "Q", "API", "V", "IPV", "PEP", "FIX",
    "FEAT", "CHORE", "ISSUE", "ITEM", "TEST", "TICKET",
];

fn ok_prefix(p: &str) -> bool {
    !STOP.contains(&p)
}

/// Keys in a branch name (case-insensitive, e.g. `nikos/eng-42-dark-mode`).
pub fn from_branch(branch: &str) -> Vec<String> {
    // Overlapping matches: scan with a sliding start so `a/eng-1-b` and `x-2` both work.
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(c) = IN_BRANCH.captures_at(branch, start) {
        let m = c.get(0).unwrap();
        let key = format!("{}-{}", c[1].to_uppercase(), &c[2]);
        if ok_prefix(&c[1].to_uppercase()) && !out.contains(&key) {
            out.push(key);
        }
        start = m.end().saturating_sub(1).max(m.start() + 1);
        if start >= branch.len() {
            break;
        }
    }
    out
}

/// Upper-case keys in free text.
pub fn from_text(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in IN_TEXT.captures_iter(text) {
        let key = format!("{}-{}", &c[1], &c[2]);
        if ok_prefix(&c[1]) && !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

pub fn prefix(key: &str) -> &str {
    key.split('-').next().unwrap_or(key)
}

/// Keep strong candidates, and weak ones whose prefix is trusted.
pub fn resolve(cands: &[Candidate], trusted: &HashSet<String>, any_prefix: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in cands {
        if (c.strong || any_prefix || trusted.contains(prefix(&c.key))) && !out.contains(&c.key) {
            out.push(c.key.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_keys() {
        assert_eq!(from_branch("feature/ABC-123-add-login"), vec!["ABC-123"]);
        assert_eq!(from_branch("nikos/eng-42-dark-mode"), vec!["ENG-42"]);
        assert!(from_branch("feat/rate-limit").is_empty());
        assert!(from_branch("release-2026").is_empty());
        assert!(from_branch("main").is_empty());
    }

    #[test]
    fn text_keys() {
        assert_eq!(
            from_text("Fix ABC-123 and see ENG-7; UTF-8 is fine"),
            vec!["ABC-123", "ENG-7"]
        );
        assert!(from_text("use sha-256 and ISO-8601").is_empty());
    }

    #[test]
    fn resolving() {
        let c = vec![
            Candidate {
                key: "ABC-1".into(),
                strong: true,
            },
            Candidate {
                key: "XYZ-2".into(),
                strong: false,
            },
            Candidate {
                key: "ABC-3".into(),
                strong: false,
            },
        ];
        let trusted: HashSet<String> = ["ABC".to_string()].into();
        assert_eq!(resolve(&c, &trusted, false), vec!["ABC-1", "ABC-3"]);
        assert_eq!(resolve(&c, &HashSet::new(), true).len(), 3);
    }
}
