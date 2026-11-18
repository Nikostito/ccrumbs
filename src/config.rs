//! `~/.cctrail/config.json` and well-known paths.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where Claude Code keeps transcripts. Default: `~/.claude/projects`.
    pub claude_projects_dir: Option<String>,
    /// Project path substrings that are never indexed.
    pub ignored_projects: Vec<String>,
    /// Model used by `cctrail describe`.
    pub describe_model: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            claude_projects_dir: None,
            ignored_projects: vec![],
            describe_model: DEFAULT_MODEL.into(),
        }
    }
}

/// `$CCTRAIL_HOME` or `~/.cctrail`.
pub fn data_dir() -> PathBuf {
    if let Ok(h) = std::env::var("CCTRAIL_HOME") {
        return PathBuf::from(h);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cctrail")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

impl Config {
    pub fn load() -> Config {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(config_path(), serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    pub fn projects_dir(&self) -> PathBuf {
        if let Some(p) = &self.claude_projects_dir {
            return PathBuf::from(shellexpand(p));
        }
        if let Ok(c) = std::env::var("CLAUDE_CONFIG_DIR") {
            return PathBuf::from(c).join("projects");
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
            .join("projects")
    }

    pub fn is_ignored(&self, project: &str) -> bool {
        self.ignored_projects
            .iter()
            .any(|i| !i.is_empty() && project.contains(i.as_str()))
    }
}

fn shellexpand(p: &str) -> String {
    match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(h)) => h.join(rest).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}
