//! Small text helpers: cleaning prompts, truncation, time formatting.

use chrono::{DateTime, Local, Utc};
use regex::Regex;
use std::sync::LazyLock;

static TAG_BLOCKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<(system-reminder|local-command-stdout|local-command-stderr|command-message|local-command-caveat|bash-stdout|bash-stderr|pasted_content[^>]*)>.*?</[a-z_-]+>",
    )
    .unwrap()
});
static CMD_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<command-name>\s*(.*?)\s*</command-name>").unwrap());
static CMD_ARGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<command-args>\s*(.*?)\s*</command-args>").unwrap());
static ANY_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</?[a-zA-Z][\w-]*[^>]*>").unwrap());
static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Turn a raw user message into what the human actually typed, or None if it's
/// harness noise (caveats, reminders, interrupted markers...).
pub fn clean_prompt(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty()
        || t.starts_with("Caveat: The messages below")
        || t.starts_with("[Request interrupted")
        || t.starts_with("<local-command-stdout>")
        || t.starts_with("This session is being continued from a previous conversation")
    {
        return None;
    }
    if let Some(c) = CMD_NAME.captures(t) {
        let name = c[1].trim().to_string();
        let args = CMD_ARGS
            .captures(t)
            .map(|a| a[1].trim().to_string())
            .unwrap_or_default();
        let s = if args.is_empty() {
            name
        } else {
            format!("{name} {args}")
        };
        return if s.is_empty() { None } else { Some(s) };
    }
    let s = TAG_BLOCKS.replace_all(t, "");
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// Strip leftover markup and collapse whitespace onto one line.
pub fn one_line(s: &str) -> String {
    let s = ANY_TAG.replace_all(s, " ");
    WS.replace_all(s.trim(), " ").into_owned()
}

/// One-line, char-safe truncation with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    let s = one_line(s);
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out = out.trim_end().to_string();
    out.push('…');
    out
}

/// Cap a string at `max` bytes without splitting a UTF-8 char.
pub fn cap_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Claude Code stores transcripts under a folder named after the cwd with `/`
/// (and `.`) replaced by `-`. Decoding is lossy; prefer the recorded cwd.
pub fn decode_project_dir(dir: &str) -> String {
    if dir.starts_with('-') {
        dir.replacen('-', "/", 1).replace('-', "/")
    } else {
        dir.to_string()
    }
}

pub fn parse_ts(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// "3m", "5h", "2d", "3w", "4mo" ago.
pub fn age(ts: Option<&str>) -> String {
    let Some(t) = ts.and_then(parse_ts) else {
        return "?".into();
    };
    let secs = (Utc::now() - t).num_seconds().max(0);
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 86_400 * 14 => format!("{}d", s / 86_400),
        s if s < 86_400 * 60 => format!("{}w", s / (86_400 * 7)),
        s if s < 86_400 * 365 => format!("{}mo", s / (86_400 * 30)),
        s => format!("{}y", s / (86_400 * 365)),
    }
}

pub fn local_time(ts: Option<&str>) -> String {
    ts.and_then(parse_ts)
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

pub fn duration(start: Option<&str>, end: Option<&str>) -> Option<String> {
    let (s, e) = (parse_ts(start?)?, parse_ts(end?)?);
    let mins = (e - s).num_minutes().max(0);
    Some(if mins < 60 {
        format!("{mins}m")
    } else if mins < 60 * 48 {
        format!("{}h{:02}m", mins / 60, mins % 60)
    } else {
        format!("{}d", mins / (60 * 24))
    })
}

pub fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}k", n / 1000),
        n => n.to_string(),
    }
}

pub fn home_short(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let h = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(h.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_slash_commands() {
        let raw = "<command-message>review is running…</command-message>\n<command-name>/review</command-name>\n<command-args>42</command-args>";
        assert_eq!(clean_prompt(raw).as_deref(), Some("/review 42"));
    }

    #[test]
    fn drops_noise() {
        assert!(clean_prompt("Caveat: The messages below were generated by the user").is_none());
        assert!(clean_prompt("<system-reminder>x</system-reminder>").is_none());
        assert_eq!(
            clean_prompt("fix it<system-reminder>x</system-reminder>").as_deref(),
            Some("fix it")
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }
}
