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
pub fn scan(cfg: &Config) -> Vec<Found> {
    let root = cfg.projects_dir();
    let Ok(projects) = fs::read_dir(&root) else {
        return vec![];
    };
    let dirs: Vec<PathBuf> = projects
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
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
        .collect()
}

/// `owner/repo` of the `origin` remote in `dir`, if it's a GitHub repo.
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
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string() + " ";
    crate::prdetect::repo_hints(&url).into_iter().next()
}

fn most_common(items: &[String]) -> Option<String> {
    let mut c: HashMap<&String, usize> = HashMap::new();
    for i in items {
        *c.entry(i).or_default() += 1;
    }
    c.into_iter()
        .max_by_key(|(_, n)| *n)
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
        if let Some(r) = &s.repo {
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

/// Set `CCTRAIL_TIMING=1` to print how long each indexing phase takes.
fn phase(label: &str, t: &mut Instant) {
    if std::env::var_os("CCTRAIL_TIMING").is_some() {
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
    let found = scan(cfg);
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
    db.write_sessions(&parsed)?;
    phase("write", &mut t);
    let missing = db.mark_missing(&present)?;
    phase("missing", &mut t);
    Ok(Report {
        scanned: found.len(),
        updated: parsed.len(),
        missing,
        millis: t0.elapsed().as_millis(),
    })
}
