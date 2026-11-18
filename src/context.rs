//! Where you are right now: the directory, git repo and branch cctrail was started in.
//! Sessions from here are ranked first.

use crate::model::SessionRow;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Git top-level directory, or the plain cwd outside a repo.
    pub root: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
}

/// How close a session is to the current context.
pub const TIER_NONE: u8 = 0;
pub const TIER_PLACE: u8 = 1; // same repo or directory tree
pub const TIER_BRANCH: u8 = 2; // ... and the same branch

impl Context {
    /// None when started somewhere too generic to mean anything (home dir, `/`).
    pub fn detect() -> Option<Context> {
        let cwd = std::env::current_dir().ok()?;
        Context::at(&cwd)
    }

    pub fn at(dir: &Path) -> Option<Context> {
        let cwd = dir.to_string_lossy().trim_end_matches('/').to_string();
        let home = dirs::home_dir().map(|h| h.to_string_lossy().trim_end_matches('/').to_string());
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--show-toplevel", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success());
        let (root, branch) = match out {
            Some(o) => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                let mut lines = text.lines();
                let root = lines
                    .next()
                    .unwrap_or(&cwd)
                    .trim_end_matches('/')
                    .to_string();
                let branch = lines
                    .next()
                    .map(str::to_string)
                    .filter(|b| !b.is_empty() && b != "HEAD");
                (root, branch)
            }
            None => (cwd.clone(), None),
        };
        if root.is_empty() || root == "/" || Some(&root) == home.as_ref() {
            return None;
        }
        let repo = crate::index::git_repo(&root);
        Some(Context { root, repo, branch })
    }

    pub fn tier(&self, s: &SessionRow) -> u8 {
        let same_repo = match (&self.repo, &s.repo) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        let dir = s.resume_dir();
        let under_root = dir == self.root || dir.starts_with(&format!("{}/", self.root));
        if !(same_repo || under_root) {
            return TIER_NONE;
        }
        match (&self.branch, &s.branch) {
            (Some(a), Some(b)) if a == b && !crate::tasks::is_default_branch(a) => TIER_BRANCH,
            _ => TIER_PLACE,
        }
    }

    /// Short label for the header, e.g. `api ⎇ feat/rate-limit`.
    pub fn label(&self) -> String {
        let name = self.root.rsplit('/').next().unwrap_or(&self.root);
        match &self.branch {
            Some(b) => format!("{name} ⎇ {b}"),
            None => name.to_string(),
        }
    }
}
