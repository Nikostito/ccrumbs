//! Sessions that are open right now: running `claude` processes, matched to
//! sessions, and switching to the terminal they run in.
//!
//! A process started with `--resume <id>` is matched exactly. Otherwise it is
//! matched by its working directory to the session there whose transcript it has
//! been writing since it started. Unix only (`ps`, `/proc` or `lsof`).

use crate::model::SessionRow;
use anyhow::{Result, bail};
use std::collections::HashSet;
use std::process::Command;

/// A running `claude` process with a session open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub pid: u32,
    /// Controlling terminal as `ps` prints it (`pts/3`, `ttys003`).
    pub tty: String,
    /// Matched by `--resume <id>`, not guessed from the directory.
    pub exact: bool,
}

impl Live {
    /// `/dev/pts/3`, as terminals and tmux name it.
    pub fn dev(&self) -> String {
        format!("/dev/{}", self.tty)
    }
}

#[derive(Debug, Clone)]
struct Proc {
    pid: u32,
    ppid: u32,
    tty: String,
    /// Seconds since it started.
    age: i64,
    args: Vec<String>,
}

/// `[[dd-]hh:]mm:ss` (ps `etime`) in seconds.
fn parse_etime(s: &str) -> Option<i64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, s),
    };
    let mut secs = 0i64;
    for part in rest.split(':') {
        secs = secs * 60 + part.parse::<i64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

fn is_claude(args: &[String]) -> bool {
    let base = |s: &str| {
        s.rsplit(['/', '\\'])
            .next()
            .unwrap_or(s)
            .trim_end_matches(".exe")
            .to_string()
    };
    let Some(first) = args.first() else {
        return false;
    };
    let via_runtime = matches!(base(first).as_str(), "node" | "bun");
    let named = base(first) == "claude"
        || (via_runtime
            && args
                .get(1)
                .is_some_and(|a| base(a) == "claude" || a.contains("@anthropic-ai/claude-code")));
    if !named {
        return false;
    }
    // Headless runs and helper subcommands are not open sessions.
    let rest = &args[if via_runtime { 2 } else { 1 }..];
    let helper = rest.first().is_some_and(|a| {
        matches!(
            a.as_str(),
            "mcp" | "--ripgrep" | "update" | "doctor" | "config" | "install" | "setup-token"
        )
    });
    !helper && !rest.iter().any(|a| a == "-p" || a == "--print")
}

/// Interactive `claude` processes, top-most only (not ones a claude started).
fn claude_procs() -> Vec<Proc> {
    let Ok(out) = Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,etime=,tty=,args="])
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let all: Vec<Proc> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let age = parse_etime(it.next()?)?;
            let tty = it.next()?.to_string();
            let args: Vec<String> = it.map(str::to_string).collect();
            Some(Proc {
                pid,
                ppid,
                tty,
                age,
                args,
            })
        })
        .collect();
    let claude: HashSet<u32> = all
        .iter()
        .filter(|p| is_claude(&p.args))
        .map(|p| p.pid)
        .collect();
    let me = std::process::id();
    all.into_iter()
        .filter(|p| {
            claude.contains(&p.pid)
                && !claude.contains(&p.ppid)
                && p.pid != me
                && !matches!(p.tty.as_str(), "?" | "??" | "-" | "")
        })
        .collect()
}

/// Session id given on the command line (`--resume ID`, `-r ID`, `--session-id ID`).
fn resumed_id(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for flag in ["--resume", "--session-id"] {
            if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
                return Some(v.to_string());
            }
        }
        if matches!(a.as_str(), "--resume" | "-r" | "--session-id") {
            return it.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

/// Working directories of `pids`.
fn cwds(pids: &[u32]) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    if pids.is_empty() {
        return out;
    }
    if cfg!(target_os = "linux") {
        for &pid in pids {
            if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                out.insert(pid, p.to_string_lossy().into_owned());
            }
        }
        return out;
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(o) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &list])
        .output()
    else {
        return out;
    };
    let mut pid = 0;
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some(p) = l.strip_prefix('p') {
            pid = p.parse().unwrap_or(0);
        } else if let Some(n) = l.strip_prefix('n') {
            out.insert(pid, n.to_string());
        }
    }
    out
}

/// What matching needs to know about a running `claude`.
struct Running {
    pid: u32,
    tty: String,
    /// Seconds since it started.
    age: i64,
    /// `--resume <id>` from its command line. Only a hint: `/clear` or `/resume`
    /// inside Claude switches session without changing the command line.
    resumed: Option<String>,
    dir: Option<String>,
}

/// Which session each running `claude` has open: `(session index, process index,
/// matched by id)`. `sessions` is newest first; `last` is each one's last write.
///
/// A process is on the session it wrote to most recently. Per directory, the `n`
/// processes there own the `n` sessions written to most recently since the oldest
/// of them started. A `--resume` id counts only while that session is one of those
/// (or nothing was written since the process started, i.e. it was just resumed).
fn assign(
    sessions: &[(String, String, Option<chrono::DateTime<chrono::Utc>>)],
    procs: &[Running],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<(usize, usize, bool)> {
    let started = |p: &Running| now - chrono::Duration::seconds(p.age + 5);
    let mut out: Vec<(usize, usize, bool)> = Vec::new();
    let mut taken: HashSet<usize> = HashSet::new();
    let mut done: HashSet<usize> = HashSet::new();
    let by_id = |id: &str| sessions.iter().position(|(sid, _, _)| sid == id);

    let mut dirs: Vec<&str> = procs.iter().filter_map(|p| p.dir.as_deref()).collect();
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        let mut here: Vec<usize> = (0..procs.len())
            .filter(|&k| procs[k].dir.as_deref() == Some(dir))
            .collect();
        let Some(earliest) = here.iter().map(|&k| started(&procs[k])).min() else {
            continue;
        };
        // The sessions being written to here right now.
        let current: Vec<usize> = sessions
            .iter()
            .enumerate()
            .filter(|(_, (_, d, last))| d == dir && last.is_some_and(|t| t >= earliest))
            .map(|(i, _)| i)
            .take(here.len())
            .collect();
        // 1. A --resume id that is still being written to.
        for &k in &here {
            if let Some(i) = procs[k].resumed.as_deref().and_then(by_id)
                && current.contains(&i)
                && taken.insert(i)
            {
                out.push((i, k, true));
                done.insert(k);
            }
        }
        // 2. The rest, youngest first: the newest session written since it started.
        here.retain(|k| !done.contains(k));
        here.sort_by_key(|&k| procs[k].age);
        for &k in &here {
            let since = started(&procs[k]);
            if let Some(&i) = current
                .iter()
                .find(|&&i| !taken.contains(&i) && sessions[i].2.is_some_and(|t| t >= since))
            {
                taken.insert(i);
                out.push((i, k, false));
                done.insert(k);
            }
        }
    }
    // 3. Just resumed and nothing typed yet: nothing written since it started, so
    //    trust the id (also for processes whose directory couldn't be read).
    for (k, p) in procs.iter().enumerate() {
        if done.contains(&k) {
            continue;
        }
        if let Some(i) = p.resumed.as_deref().and_then(by_id)
            && sessions[i].2.is_none_or(|t| t < started(p))
            && taken.insert(i)
        {
            out.push((i, k, true));
        }
    }
    out
}

/// Mark the sessions that are open in a running `claude` (clearing old marks).
/// Cheap enough to run often: one `ps`, and `/proc` or one `lsof`.
pub fn annotate(sessions: &mut [SessionRow]) {
    for s in sessions.iter_mut() {
        s.live = None;
    }
    if !cfg!(unix) || crate::config::env("NO_LIVE").is_some() {
        return;
    }
    let procs = claude_procs();
    if procs.is_empty() {
        return;
    }
    let dirs = cwds(&procs.iter().map(|p| p.pid).collect::<Vec<_>>());
    let running: Vec<Running> = procs
        .iter()
        .map(|p| Running {
            pid: p.pid,
            tty: p.tty.clone(),
            age: p.age,
            resumed: resumed_id(&p.args),
            dir: dirs.get(&p.pid).map(|d| crate::text::norm_path(d)),
        })
        .collect();
    let view: Vec<(String, String, Option<chrono::DateTime<chrono::Utc>>)> = sessions
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                if s.missing {
                    String::new()
                } else {
                    crate::text::norm_path(&s.resume_dir())
                },
                s.ended.as_deref().and_then(crate::text::parse_ts),
            )
        })
        .collect();
    for (i, k, exact) in assign(&view, &running, chrono::Utc::now()) {
        sessions[i].live = Some(Live {
            pid: running[k].pid,
            tty: running[k].tty.clone(),
            exact,
        });
    }
}

fn run_ok(cmd: &mut Command) -> bool {
    cmd.output().is_ok_and(|o| o.status.success())
}

fn stdout(cmd: &mut Command) -> Option<String> {
    let o = cmd.output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Bring the terminal running `live` to the front. Returns where it is.
pub fn focus(live: &Live) -> Result<String> {
    let dev = live.dev();
    // tmux: find the pane by its terminal device.
    if let Some(panes) = stdout(Command::new("tmux").args([
        "list-panes",
        "-a",
        "-F",
        "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}",
    ])) && let Some(target) = panes
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{dev} ")))
    {
        if std::env::var_os("TMUX").is_none() {
            bail!("it's open in tmux pane {target}; attach there to continue it");
        }
        let window = target.rsplit_once('.').map(|(w, _)| w).unwrap_or(target);
        let session = window.rsplit_once(':').map(|(s, _)| s).unwrap_or(window);
        let mut ok = run_ok(Command::new("tmux").args(["select-window", "-t", window]))
            && run_ok(Command::new("tmux").args(["select-pane", "-t", target]));
        // Another tmux session: move this client there too.
        let here = stdout(Command::new("tmux").args(["display-message", "-p", "#{session_name}"]));
        if ok && here.as_deref().map(str::trim) != Some(session) {
            ok = run_ok(Command::new("tmux").args(["switch-client", "-t", target]));
        }
        if ok {
            return Ok(format!("switched to tmux pane {target}"));
        }
    }
    // kitty and WezTerm can focus a window by process / terminal.
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        && run_ok(Command::new("kitty").args([
            "@",
            "focus-window",
            "--match",
            &format!("pid:{}", live.pid),
        ]))
    {
        return Ok("switched to its kitty window".into());
    }
    if let Some(list) = stdout(Command::new("wezterm").args(["cli", "list", "--format", "json"]))
        && let Ok(serde_json::Value::Array(panes)) = serde_json::from_str(&list)
        && let Some(id) = panes.iter().find_map(|p| {
            (p.get("tty_name").and_then(|t| t.as_str()) == Some(dev.as_str()))
                .then(|| p.get("pane_id").and_then(|i| i.as_u64()))
                .flatten()
        })
        && run_ok(Command::new("wezterm").args([
            "cli",
            "activate-pane",
            "--pane-id",
            &id.to_string(),
        ]))
    {
        return Ok("switched to its WezTerm pane".into());
    }
    // macOS: iTerm2 and Terminal know each tab's terminal device.
    if cfg!(target_os = "macos") {
        let iterm = format!(
            r#"if application "iTerm2" is running then
tell application "iTerm2"
repeat with w in windows
repeat with t in tabs of w
repeat with s in sessions of t
if tty of s is "{dev}" then
select w
tell t to select
tell s to select
activate
return "iTerm2"
end if
end repeat
end repeat
end repeat
end tell
end if
if application "Terminal" is running then
tell application "Terminal"
repeat with w in windows
repeat with t in tabs of w
if tty of t is "{dev}" then
set selected of t to true
set index of w to 1
activate
return "Terminal"
end if
end repeat
end repeat
end tell
end if
return """#
        );
        if let Some(app) = stdout(Command::new("osascript").args(["-e", &iterm]))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Ok(format!("switched to its {app} tab"));
        }
    }
    bail!("it's already open in another terminal ({})", live.tty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn recognises_claude_processes() {
        assert!(is_claude(&v("claude")));
        assert!(is_claude(&v("/Users/u/.local/bin/claude --resume abc")));
        assert!(is_claude(&v(
            "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        )));
        assert!(!is_claude(&v("claude -p hello")));
        assert!(!is_claude(&v("claude mcp serve")));
        assert!(!is_claude(&v("ccrumbs")));
        assert!(!is_claude(&v("vim claude.md")));
        assert!(is_claude(&v("claude fix the config loader")));
    }

    fn at(min: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        Some(chrono::DateTime::from_timestamp(1_800_000_000 + min * 60, 0).unwrap())
    }

    fn proc(pid: u32, started_min: i64, resumed: Option<&str>, dir: &str) -> Running {
        // "now" is minute 100 in these tests.
        Running {
            pid,
            tty: format!("pts/{pid}"),
            age: (100 - started_min) * 60,
            resumed: resumed.map(str::to_string),
            dir: Some(dir.into()),
        }
    }

    fn sess(
        id: &str,
        dir: &str,
        last_min: i64,
    ) -> (String, String, Option<chrono::DateTime<chrono::Utc>>) {
        (id.into(), dir.into(), at(last_min))
    }

    fn matched(
        s: &[(String, String, Option<chrono::DateTime<chrono::Utc>>)],
        p: &[Running],
    ) -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> = assign(s, p, at(100).unwrap())
            .into_iter()
            .map(|(i, k, _)| (s[i].0.clone(), p[k].pid))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_cleared_session_is_not_open_any_more() {
        // Started as `claude --resume a`, then /clear: it now writes to b.
        let s = vec![sess("b", "/w", 99), sess("a", "/w", 60)];
        let p = vec![proc(1, 50, Some("a"), "/w")];
        assert_eq!(matched(&s, &p), vec![("b".into(), 1)]);
    }

    #[test]
    fn a_session_closed_before_the_process_started_is_not_open() {
        // a was closed at minute 70; a new claude started at 80 hasn't written yet.
        let s = vec![sess("a", "/w", 70)];
        let p = vec![proc(1, 80, None, "/w")];
        assert!(matched(&s, &p).is_empty());
    }

    #[test]
    fn two_processes_in_one_directory() {
        let s = vec![
            sess("y", "/w", 99),
            sess("x", "/w", 98),
            sess("old", "/w", 10),
        ];
        let p = vec![proc(1, 50, Some("x"), "/w"), proc(2, 90, None, "/w")];
        assert_eq!(matched(&s, &p), vec![("x".into(), 1), ("y".into(), 2)]);
    }

    #[test]
    fn just_resumed_and_untouched_counts_as_open() {
        let s = vec![sess("a", "/w", 10)];
        let p = vec![proc(1, 95, Some("a"), "/w")];
        assert_eq!(matched(&s, &p), vec![("a".into(), 1)]);
    }

    #[test]
    fn reads_resume_ids_and_elapsed_times() {
        assert_eq!(
            resumed_id(&v("claude --resume abc-1")).as_deref(),
            Some("abc-1")
        );
        assert_eq!(resumed_id(&v("claude -r abc")).as_deref(), Some("abc"));
        assert_eq!(
            resumed_id(&v("claude --resume=xyz")).as_deref(),
            Some("xyz")
        );
        assert_eq!(resumed_id(&v("claude --resume")), None);
        assert_eq!(resumed_id(&v("claude -c")), None);
        assert_eq!(parse_etime("05:03"), Some(303));
        assert_eq!(parse_etime("1:00:00"), Some(3600));
        assert_eq!(parse_etime("2-00:00:01"), Some(172_801));
    }
}
