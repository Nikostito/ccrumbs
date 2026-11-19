//! Incremental, parallel indexing of `~/.claude/projects/**.jsonl`.

use crate::config::Config;
use crate::db::Db;
use crate::model::ParsedSession;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, UNIX_EPOCH};

pub struct Found {
    pub path: PathBuf,
    pub project_dir: String,
    pub subagents: Vec<PathBuf>,
    pub mtime: i64,
    pub size: i64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub scanned: usize,
    pub updated: usize,
    pub missing: usize,
    pub millis: u128,
}

fn stamp(p: &Path) -> Option<(i64, i64)> {
    let m = fs::metadata(p).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((t, m.len() as i64))
}

/// Every session transcript on disk, with a combined mtime/size stamp that
/// also changes when one of its subagent transcripts changes.
/// Transcript files under the projects folder. None when the folder can't be read
/// (not there yet, or a wrong `CLAUDE_CONFIG_DIR`), which is different from "empty".
pub fn scan(cfg: &Config) -> Option<Vec<Found>> {
    let root = cfg.projects_dir();
    let projects = fs::read_dir(&root).ok()?;
    let dirs: Vec<PathBuf> = projects
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    Some(
        dirs.par_iter()
            .flat_map_iter(|dir| {
                let project_dir = dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let mut out = Vec::new();
                if cfg.is_ignored(&project_dir)
                    || cfg.is_ignored(&crate::text::decode_project_dir(&project_dir))
                {
                    return out;
                }
                let Ok(entries) = fs::read_dir(dir) else {
                    return out;
                };
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().is_none_or(|x| x != "jsonl") {
                        continue;
                    }
                    let Some((mut mtime, mut size)) = stamp(&p) else {
                        continue;
                    };
                    let mut subagents = Vec::new();
                    let sub_dir = p.with_extension("").join("subagents");
                    if let Ok(subs) = fs::read_dir(&sub_dir) {
                        for s in subs.flatten() {
                            let sp = s.path();
                            if sp.extension().is_some_and(|x| x == "jsonl")
                                && let Some((m, z)) = stamp(&sp)
                            {
                                mtime = mtime.max(m);
                                size += z;
                                subagents.push(sp);
                            }
                        }
                    }
                    out.push(Found {
                        path: p,
                        project_dir: project_dir.clone(),
                        subagents,
                        mtime,
                        size,
                    });
                }
                out
            })
            .collect(),
    )
}

/// `owner/repo` of the `origin` remote in `dir`, if it's a GitHub repo.
static ANY_REMOTE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^(?:[a-z+]+://)?(?:[^@/]+@)?([^:/]+)[:/](?:\d+/)?([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+?)(?:\.git)?/?$").unwrap()
});

pub fn git_repo(dir: &str) -> Option<String> {
    if !Path::new(dir).is_dir() {
        return None;
    }
    let out = Command::new("git")
        .args(["-C", dir, "config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    crate::prdetect::repo_hints(&format!("{url} "))
        .into_iter()
        .next()
        .or_else(|| {
            // SSH host aliases (`git@github-work:org/repo`) and GitHub Enterprise hosts.
            let c = ANY_REMOTE.captures(&url)?;
            c[1].contains("github")
                .then(|| format!("{}/{}", &c[2], &c[3]).to_lowercase())
        })
}

fn most_common(items: &[String]) -> Option<String> {
    let mut c: HashMap<&String, usize> = HashMap::new();
    for i in items {
        *c.entry(i).or_default() += 1;
    }
    // Ties go to the alphabetically first name, so the result doesn't depend on hash order.
    c.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| k.clone())
}

/// Decide each session's repo and fill it into PR links that only had a number.
fn resolve_repos(sessions: &mut [ParsedSession]) {
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    for s in sessions.iter_mut() {
        let from_git = s.cwd.as_ref().and_then(|cwd| {
            cache
                .entry(cwd.clone())
                .or_insert_with(|| git_repo(cwd))
                .clone()
        });
        let linked: Vec<String> = s
            .pr_links
            .iter()
            .filter(|l| !l.repo.is_empty())
            .map(|l| l.repo.clone())
            .collect();
        s.repo = from_git
            .or_else(|| most_common(&s.repo_hints))
            .or_else(|| most_common(&linked));
        // A bare `#12` belongs to the repo the session's own PR links point at when
        // there is exactly one (forks: origin is the fork, the PRs live upstream),
        // else to the session's repo.
        let mut distinct = linked.clone();
        distinct.sort();
        distinct.dedup();
        let bare_repo = if distinct.len() == 1 {
            Some(distinct[0].clone())
        } else {
            s.repo.clone()
        };
        if let Some(r) = &bare_repo {
            for l in &mut s.pr_links {
                if l.repo.is_empty() {
                    l.repo = r.clone();
                }
            }
        }
        // Resolving can create duplicates of already-qualified links.
        s.pr_links
            .sort_by(|a, b| (&a.repo, a.number, a.role).cmp(&(&b.repo, b.number, b.role)));
        s.pr_links
            .dedup_by(|b, a| a.repo == b.repo && a.number == b.number && a.role == b.role);
    }
}

/// Set `CCRUMBS_TIMING=1` to print how long each indexing phase takes.
fn phase(label: &str, t: &mut Instant) {
    if crate::config::env("TIMING").is_some() {
        eprintln!(
            "  {label:<14} {:>7.1} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    *t = Instant::now();
}

pub fn run(db: &mut Db, cfg: &Config, full: bool) -> Result<Report> {
    let t0 = Instant::now();
    let mut t = Instant::now();
    let scanned = scan(cfg);
    let readable = scanned.is_some();
    let found = scanned.unwrap_or_default();
    phase("scan", &mut t);
    let stamps = if full {
        HashMap::new()
    } else {
        db.file_stamps()?
    };
    let present: HashSet<String> = found
        .iter()
        .filter_map(|f| f.path.file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    let changed: Vec<&Found> = found
        .iter()
        .filter(|f| {
            let id = f.path.file_stem().unwrap_or_default().to_string_lossy();
            stamps.get(id.as_ref()) != Some(&(f.mtime, f.size))
        })
        .collect();
    phase("diff", &mut t);
    let mut parsed: Vec<ParsedSession> = changed
        .par_iter()
        .filter_map(|f| {
            let mut s = crate::parse::parse_session(&f.path, &f.project_dir, &f.subagents)?;
            s.mtime = f.mtime;
            s.size = f.size;
            Some(s)
        })
        .collect();
    phase("parse", &mut t);
    resolve_repos(&mut parsed);
    phase("repos", &mut t);
    if full {
        db.clear_derived()?;
    }
    // Files that hold no session: remember their stamp so they aren't re-read every run.
    let got: HashSet<&str> = parsed.iter().map(|s| s.id.as_str()).collect();
    let skipped: Vec<(String, i64, i64)> = changed
        .iter()
        .filter_map(|f| {
            let id = f.path.file_stem()?.to_string_lossy().to_string();
            (!got.contains(id.as_str())).then_some((id, f.mtime, f.size))
        })
        .collect();
    db.note_skipped(&skipped)?;
    db.write_sessions(&parsed)?;
    phase("write", &mut t);
    // Only trust "not on disk" when the folder could actually be read.
    db.drop_ignored(cfg)?;
    let missing = if readable {
        db.mark_missing(&present)?
    } else {
        0
    };
    phase("missing", &mut t);
    Ok(Report {
        scanned: found.len(),
        updated: parsed.len(),
        missing,
        millis: t0.elapsed().as_millis(),
    })
}
