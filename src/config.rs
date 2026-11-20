//! `~/.ccrumbs/config.json` and well-known paths.

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
    /// Model used by `ccrumbs describe`.
    pub describe_model: String,
    /// Override context windows: model-id substring -> tokens, e.g. {"sonnet-4-5": 1000000}.
    pub context_windows: std::collections::BTreeMap<String, u64>,
    /// Ticket prefix -> URL with `{key}`, e.g. {"ABC": "https://acme.atlassian.net/browse/{key}",
    /// "ENG": "https://linear.app/acme/issue/{key}"}. `"*"` applies to any prefix.
    pub tickets: std::collections::BTreeMap<String, String>,
    /// TUI colors: "dark" (own gray-black theme, default) or "terminal" (your terminal's palette).
    pub theme: String,
    /// Command that opens a project, e.g. "cursor", "open -a \"PyCharm\"", "nvim".
    /// Unset: ccrumbs asks the first time (Ctrl-E) and remembers.
    pub editor: Option<String>,
    /// Check your open PRs' status (CI, reviews, conflicts) in the background when the
    /// picker opens, at most every 10 minutes. Needs `gh`. Default: on.
    pub refresh_prs: bool,
}

static TICKETS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Configured ticket templates (empty until `Config::load`).
pub fn ticket_templates() -> std::collections::BTreeMap<String, String> {
    TICKETS.read().map(|t| t.clone()).unwrap_or_default()
}

/// Browser URL for a ticket key, if a template is configured for its prefix.
pub fn ticket_url(key: &str) -> Option<String> {
    let t = TICKETS.read().ok()?;
    let tpl = t.get(crate::tickets::prefix(key)).or_else(|| t.get("*"))?;
    Some(tpl.replace("{key}", key))
}

/// Turn whatever the user pastes into a URL template with `{key}`:
/// - `acme.atlassian.net` or `https://jira.acme.com` → `…/browse/{key}`
/// - `https://linear.app/acme` → `…/issue/{key}`
/// - any ticket URL (`…/browse/ABC-123`, `…/issue/ENG-42/some-title`) → that URL with the key replaced
/// - anything that already contains `{key}` is kept as-is
pub fn ticket_template(input: &str) -> Result<String> {
    let mut u = input.trim().to_string();
    if u.is_empty() {
        anyhow::bail!("empty URL");
    }
    if !u.contains("://") {
        u = format!("https://{u}");
    }
    if u.contains("{key}") {
        return Ok(u);
    }
    // A pasted ticket link: swap its key for {key}, drop anything after it (Linear adds a slug).
    let key_re = regex::Regex::new(r"/([A-Za-z][A-Za-z0-9]{1,9}-[0-9]{1,7})(?:[/?#].*)?$").unwrap();
    if let Some(m) = key_re.captures(&u).and_then(|c| c.get(1)) {
        return Ok(format!("{}{{key}}", &u[..m.start()]));
    }
    let base = u.trim_end_matches('/');
    Ok(if base.contains("linear.app") {
        if base.ends_with("/issue") {
            format!("{base}/{{key}}")
        } else {
            format!("{base}/issue/{{key}}")
        }
    } else if base.ends_with("/browse") || base.ends_with("/issue") || base.ends_with("/issues") {
        format!("{base}/{{key}}")
    } else {
        format!("{base}/browse/{{key}}")
    })
}

/// Save a ticket link for a prefix (`*` = any prefix) to the config file.
pub fn set_ticket_url(prefix: &str, template: &str) -> Result<()> {
    let mut cfg = Config::load();
    cfg.tickets.insert(prefix.to_string(), template.to_string());
    cfg.save()?;
    if let Ok(mut t) = TICKETS.write() {
        *t = cfg.tickets;
    }
    Ok(())
}

/// Remember the editor to open projects with (`None` forgets it, so ccrumbs asks again).
pub fn set_editor(command: Option<&str>) -> Result<()> {
    let mut cfg = Config::load();
    cfg.editor = command.map(str::to_string);
    cfg.save()
}

pub fn remove_ticket_url(prefix: &str) -> Result<bool> {
    let mut cfg = Config::load();
    let had = cfg.tickets.remove(prefix).is_some();
    cfg.save()?;
    if let Ok(mut t) = TICKETS.write() {
        *t = cfg.tickets;
    }
    Ok(had)
}

static OVERRIDES: std::sync::OnceLock<std::collections::BTreeMap<String, u64>> =
    std::sync::OnceLock::new();

/// Context window override for a model, if the config has one.
pub fn context_override(model: &str) -> Option<u64> {
    OVERRIDES
        .get()?
        .iter()
        .find(|(k, _)| model.contains(k.as_str()))
        .map(|(_, v)| *v)
}

impl Default for Config {
    fn default() -> Self {
        Config {
            claude_projects_dir: None,
            ignored_projects: vec![],
            describe_model: DEFAULT_MODEL.into(),
            context_windows: Default::default(),
            tickets: Default::default(),
            theme: "dark".into(),
            editor: None,
            refresh_prs: true,
        }
    }
}

/// Read `$CCRUMBS_<name>`, falling back to the pre-rename `$CCTRAIL_<name>`.
pub fn env(name: &str) -> Option<String> {
    std::env::var(format!("CCRUMBS_{name}"))
        .or_else(|_| std::env::var(format!("CCTRAIL_{name}")))
        .ok()
        .filter(|v| !v.is_empty())
}

/// `$CCRUMBS_HOME` or `~/.ccrumbs`.
pub fn data_dir() -> PathBuf {
    if let Some(h) = env("HOME") {
        return PathBuf::from(h);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = home.join(".ccrumbs");
    migrate_from_cctrail(&home, &dir);
    dir
}

/// This tool used to be called cctrail and kept its data in `~/.cctrail`. Copy (never
/// move) its index and config the first time, but only if they are ours: an unrelated
/// tool on crates.io is also called cctrail, so `index.db` must have our `marks` table.
fn migrate_from_cctrail(home: &std::path::Path, dir: &std::path::Path) {
    let old = home.join(".cctrail");
    let old_db = old.join("index.db");
    if dir.exists() || !old_db.is_file() {
        return;
    }
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(&old_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    let ours = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'marks'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if !ours || std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // VACUUM INTO reads through the write-ahead log, so nothing recent is lost.
    if conn
        .execute("VACUUM INTO ?1", [dir.join("index.db").to_string_lossy()])
        .is_err()
    {
        return;
    }
    let _ = std::fs::copy(old.join("config.json"), dir.join("config.json"));
    eprintln!(
        "ccrumbs: copied your index and settings from {} (cctrail is now ccrumbs)",
        old.display()
    );
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn db_path() -> PathBuf {
    data_dir().join("index.db")
}

impl Config {
    /// The config, or defaults if there is none. A file that doesn't parse is
    /// reported once and ignored (and `save` refuses to overwrite it).
    pub fn load() -> Config {
        let cfg = match Config::read() {
            Ok(c) => c,
            Err(e) => {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    eprintln!("ccrumbs: ignoring {}: {e:#}", config_path().display())
                });
                Config::default()
            }
        };
        let _ = OVERRIDES.set(cfg.context_windows.clone());
        if let Ok(mut t) = TICKETS.write() {
            *t = cfg.tickets.clone();
        }
        cfg
    }

    /// Parse the config file; a missing file is fine, a broken one is an error.
    fn read() -> Result<Config> {
        match std::fs::read_to_string(config_path()) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| anyhow::anyhow!("not valid JSON ({e})")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the config atomically. Refuses when the file on disk doesn't parse, so a
    /// typo never turns into "all your settings are gone".
    pub fn save(&self) -> Result<()> {
        if let Err(e) = Config::read() {
            anyhow::bail!(
                "{} is {e:#}; fix it first so your settings aren't overwritten",
                config_path().display()
            );
        }
        std::fs::create_dir_all(data_dir())?;
        let tmp = config_path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, config_path())?;
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

#[cfg(test)]
mod tests {
    use super::ticket_template as t;

    #[test]
    fn ticket_templates_from_what_people_paste() {
        assert_eq!(
            t("acme.atlassian.net").unwrap(),
            "https://acme.atlassian.net/browse/{key}"
        );
        assert_eq!(
            t("https://jira.acme.com/").unwrap(),
            "https://jira.acme.com/browse/{key}"
        );
        assert_eq!(
            t("https://acme.atlassian.net/browse/ABC-123").unwrap(),
            "https://acme.atlassian.net/browse/{key}"
        );
        assert_eq!(
            t("https://linear.app/acme").unwrap(),
            "https://linear.app/acme/issue/{key}"
        );
        assert_eq!(
            t("https://linear.app/acme/issue/ENG-42/fix-the-thing").unwrap(),
            "https://linear.app/acme/issue/{key}"
        );
        assert_eq!(t("https://x.y/t/{key}").unwrap(), "https://x.y/t/{key}");
        assert!(t("  ").is_err());
    }
}
