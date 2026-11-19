//! SQLite index: sessions, session↔PR links, AI descriptions, GitHub PR metadata.

use crate::model::{ParsedSession, PrLink, PrMeta, Role, SessionRow};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;

/// Bump when the derived tables change shape; they are rebuilt from transcripts.
/// `ai` and `prs` hold data that can't be re-derived locally and are kept.
const SCHEMA_VERSION: i64 = 6;

/// Every column of `sessions` beyond the key, so later versions can add columns to
/// an existing database with ALTER TABLE instead of rebuilding it.
const SESSION_COLUMNS: &[(&str, &str)] = &[
    ("path", "TEXT"),
    ("project_dir", "TEXT"),
    ("cwd", "TEXT"),
    ("branch", "TEXT"),
    ("repo", "TEXT"),
    ("started", "TEXT"),
    ("ended", "TEXT"),
    ("user_prompts", "INT"),
    ("assistant_msgs", "INT"),
    ("custom_title", "TEXT"),
    ("summary", "TEXT"),
    ("first_prompt", "TEXT"),
    ("last_prompt", "TEXT"),
    ("prompts_json", "TEXT"),
    ("files_json", "TEXT"),
    ("commands_run", "INT"),
    ("tests_ran", "INT"),
    ("commits", "INT"),
    ("tokens_in", "INT"),
    ("tokens_out", "INT"),
    ("model", "TEXT"),
    ("entrypoint", "TEXT"),
    ("cloud_id", "TEXT"),
    ("last_reply", "TEXT"),
    ("auto_desc", "TEXT"),
    ("mtime", "INT"),
    ("size", "INT"),
    ("missing", "INT DEFAULT 0"),
    ("context_tokens", "INT DEFAULT 0"),
    ("compactions", "INT DEFAULT 0"),
    ("cost_usd", "REAL DEFAULT 0"),
    ("tickets_json", "TEXT"),
];

pub struct Db {
    pub conn: Connection,
}

impl Db {
    pub fn open() -> Result<Db> {
        let dir = crate::config::data_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let conn = Connection::open(crate::config::db_path())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=OFF;",
        )?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // Pre-release layouts (before 6) are rebuilt from the transcripts. From 6 on,
        // new columns are added in place (see `ensure_columns`), so sessions whose
        // transcripts Claude Code has since deleted are never lost to an upgrade.
        if v != 0 && v < 6 {
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS session_prs; DROP TABLE IF EXISTS fts;",
            )?;
        }
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, path TEXT, project_dir TEXT, cwd TEXT, branch TEXT, repo TEXT,
                started TEXT, ended TEXT, user_prompts INT, assistant_msgs INT,
                custom_title TEXT, summary TEXT, first_prompt TEXT, last_prompt TEXT, prompts_json TEXT,
                files_json TEXT, commands_run INT, tests_ran INT, commits INT,
                tokens_in INT, tokens_out INT, model TEXT, entrypoint TEXT, cloud_id TEXT,
                last_reply TEXT, auto_desc TEXT, mtime INT, size INT, missing INT DEFAULT 0,
                context_tokens INT DEFAULT 0, compactions INT DEFAULT 0, cost_usd REAL DEFAULT 0,
                tickets_json TEXT
            );
            -- Transcript files that hold no session (e.g. only a summary), so they
            -- aren't parsed again on every run.
            CREATE TABLE IF NOT EXISTS skipped (id TEXT PRIMARY KEY, mtime INT, size INT);
            -- Your own marks on sessions. Not derived from transcripts, so never dropped.
            CREATE TABLE IF NOT EXISTS marks (
                id TEXT PRIMARY KEY, pinned INT DEFAULT 0, hidden INT DEFAULT 0, note TEXT
            );
            CREATE INDEX IF NOT EXISTS sessions_started ON sessions(started);
            CREATE INDEX IF NOT EXISTS sessions_cloud ON sessions(cloud_id);
            CREATE TABLE IF NOT EXISTS session_prs (
                session_id TEXT, repo TEXT, number INT, role TEXT, ts TEXT, evidence TEXT,
                PRIMARY KEY (session_id, repo, number, role)
            );
            CREATE INDEX IF NOT EXISTS session_prs_pr ON session_prs(number, repo);
            CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(id UNINDEXED, title, body, tokenize='unicode61');
            CREATE TABLE IF NOT EXISTS ai (
                id TEXT PRIMARY KEY, title TEXT, description TEXT, model TEXT, created TEXT, src_size INT
            );
            CREATE TABLE IF NOT EXISTS prs (
                repo TEXT, number INT, title TEXT, state TEXT, author TEXT, url TEXT, head TEXT,
                created_at TEXT, body_session TEXT, claude_footer INT, synced TEXT,
                PRIMARY KEY (repo, number)
            );
            CREATE INDEX IF NOT EXISTS prs_body_session ON prs(body_session);
            "#,
        )?;
        self.ensure_columns()?;
        self.conn
            .pragma_update(None, "user_version", v.max(SCHEMA_VERSION))?;
        Ok(())
    }

    /// `id -> (mtime, size)` for change detection.
    pub fn file_stamps(&self) -> Result<HashMap<String, (i64, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT id, mtime, size FROM sessions WHERE missing = 0 UNION ALL SELECT id, mtime, size FROM skipped",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Empty the tables derived from transcripts in one go (used by `index --full`).
    /// Deleting full-text rows one by one is what made re-indexes slow.
    pub fn clear_derived(&mut self) -> Result<()> {
        // Keep sessions whose transcript is gone: they can't be re-read, and keeping
        // history past Claude Code's retention is the point. Their search rows are
        // copied aside because the full-text table is rebuilt from scratch.
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE keep_fts AS SELECT f.rowid AS rid, f.id, f.title, f.body FROM fts f
                 JOIN sessions s ON s.rowid = f.rowid WHERE s.missing = 1;
             DELETE FROM session_prs WHERE session_id IN (SELECT id FROM sessions WHERE missing = 0);
             DELETE FROM sessions WHERE missing = 0;
             DELETE FROM skipped;
             DROP TABLE IF EXISTS fts;
             CREATE VIRTUAL TABLE fts USING fts5(id UNINDEXED, title, body, tokenize='unicode61');
             INSERT INTO fts (rowid, id, title, body) SELECT rid, id, title, body FROM keep_fts;
             DROP TABLE keep_fts;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Add columns a newer version expects to an existing table, keeping its rows.
    fn ensure_columns(&self) -> Result<()> {
        let have: std::collections::HashSet<String> = {
            let mut st = self
                .conn
                .prepare("SELECT name FROM pragma_table_info('sessions')")?;
            st.query_map([], |r| r.get(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for (col, ty) in SESSION_COLUMNS {
            if !have.contains(*col) {
                self.conn
                    .execute(&format!("ALTER TABLE sessions ADD COLUMN {col} {ty}"), [])?;
            }
        }
        Ok(())
    }

    /// Forget sessions from projects that are now ignored.
    pub fn drop_ignored(&mut self, cfg: &crate::config::Config) -> Result<usize> {
        if cfg.ignored_projects.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = {
            let mut st = self
                .conn
                .prepare("SELECT id, project_dir, COALESCE(cwd, '') FROM sessions")?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.filter_map(|r| r.ok())
                .filter(|(_, p, cwd)| {
                    cfg.is_ignored(p)
                        || cfg.is_ignored(&crate::text::decode_project_dir(p))
                        || cfg.is_ignored(cwd)
                })
                .map(|(id, _, _)| id)
                .collect()
        };
        let tx = self.conn.transaction()?;
        for id in &ids {
            tx.execute(
                "DELETE FROM fts WHERE rowid = (SELECT rowid FROM sessions WHERE id = ?1)",
                [id],
            )?;
            tx.execute("DELETE FROM session_prs WHERE session_id = ?1", [id])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    pub fn note_skipped(&mut self, stamps: &[(String, i64, i64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st =
                tx.prepare("INSERT OR REPLACE INTO skipped (id, mtime, size) VALUES (?1, ?2, ?3)")?;
            for (id, m, z) in stamps {
                st.execute(params![id, m, z])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn write_sessions(&mut self, sessions: &[ParsedSession]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            // fts rows share the session's rowid, so deleting one is a direct lookup, not a scan.
            let mut old_rowid = tx.prepare("SELECT rowid FROM sessions WHERE id = ?1")?;
            let mut del_s = tx.prepare("DELETE FROM sessions WHERE id = ?1")?;
            let mut del_p = tx.prepare("DELETE FROM session_prs WHERE session_id = ?1")?;
            let mut del_f = tx.prepare("DELETE FROM fts WHERE rowid = ?1")?;
            let mut ins = tx.prepare(
                "INSERT INTO sessions (id, path, project_dir, cwd, branch, repo, started, ended, user_prompts,
                 assistant_msgs, custom_title, summary, first_prompt, last_prompt, prompts_json, files_json,
                 commands_run, tests_ran, commits, tokens_in, tokens_out, model, entrypoint, cloud_id,
                 last_reply, auto_desc, mtime, size, missing, context_tokens, compactions, cost_usd, tickets_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,0,?29,?30,?31,?32)",
            )?;
            let mut ins_p = tx.prepare(
                "INSERT OR IGNORE INTO session_prs (session_id, repo, number, role, ts, evidence) VALUES (?1,?2,?3,?4,?5,?6)",
            )?;
            let mut unskip = tx.prepare("DELETE FROM skipped WHERE id = ?1")?;
            let mut ins_f =
                tx.prepare("INSERT INTO fts (rowid, id, title, body) VALUES (?1, ?2, ?3, ?4)")?;
            let mut tm = [std::time::Duration::ZERO; 4];
            for s in sessions {
                unskip.execute([&s.id])?;
                let t = std::time::Instant::now();
                if let Some(rid) = old_rowid
                    .query_row([&s.id], |r| r.get::<_, i64>(0))
                    .optional()?
                {
                    del_f.execute([rid])?;
                    del_s.execute([&s.id])?;
                    del_p.execute([&s.id])?;
                }
                tm[0] += t.elapsed();
                let t = std::time::Instant::now();
                let auto = crate::describe::auto_description(s);
                let rowid = ins.insert(params![
                    s.id,
                    s.path,
                    s.project_dir,
                    s.cwd,
                    s.branch,
                    s.repo,
                    s.started,
                    s.ended,
                    s.user_prompts,
                    s.assistant_msgs,
                    s.custom_title,
                    s.summary,
                    s.first_prompt,
                    s.last_prompt,
                    serde_json::to_string(&s.prompts)?,
                    serde_json::to_string(&s.files_edited)?,
                    s.commands_run,
                    s.tests_ran,
                    s.commits,
                    s.tokens_in as i64,
                    s.tokens_out as i64,
                    s.model,
                    s.entrypoint,
                    s.cloud_id,
                    s.last_reply,
                    auto,
                    s.mtime,
                    s.size,
                    s.context_tokens as i64,
                    s.compactions,
                    s.cost_usd,
                    serde_json::to_string(&s.tickets)?
                ])?;
                tm[1] += t.elapsed();
                let t = std::time::Instant::now();
                for l in &s.pr_links {
                    ins_p.execute(params![
                        s.id,
                        l.repo,
                        l.number,
                        l.role.as_str(),
                        l.ts,
                        l.evidence
                    ])?;
                }
                tm[2] += t.elapsed();
                let t = std::time::Instant::now();
                let title = [
                    s.custom_title.as_deref(),
                    s.summary.as_deref(),
                    s.first_prompt.as_deref(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
                ins_f.execute(params![rowid, s.id, title, s.fulltext])?;
                tm[3] += t.elapsed();
            }
            if crate::config::env("TIMING").is_some() {
                for (label, d) in ["delete old", "session rows", "PR links", "full-text"]
                    .iter()
                    .zip(tm)
                {
                    eprintln!("    {label:<12} {:>7.1} ms", d.as_secs_f64() * 1000.0);
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Flag sessions whose transcript is no longer on disk, and clear the flag for
    /// ones that are back (e.g. a project that was ignored and then un-ignored).
    /// Returns how many are missing now.
    pub fn mark_missing(&mut self, present: &std::collections::HashSet<String>) -> Result<usize> {
        let rows: Vec<(String, bool)> = {
            let mut st = self.conn.prepare("SELECT id, missing FROM sessions")?;
            st.query_map([], |r| {
                Ok((r.get(0)?, r.get::<_, Option<bool>>(1)?.unwrap_or(false)))
            })?
            .filter_map(|r| r.ok())
            .collect()
        };
        let tx = self.conn.transaction()?;
        let mut missing = 0;
        for (id, was) in rows {
            let now = !present.contains(&id);
            if now != was {
                tx.execute(
                    "UPDATE sessions SET missing = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
            }
            missing += now as usize;
        }
        tx.commit()?;
        Ok(missing)
    }

    pub fn all_links(&self) -> Result<HashMap<String, Vec<PrLink>>> {
        let mut st = self
            .conn
            .prepare("SELECT session_id, repo, number, role, ts, evidence FROM session_prs")?;
        let mut map: HashMap<String, Vec<PrLink>> = HashMap::new();
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PrLink {
                    repo: r.get(1)?,
                    number: r.get(2)?,
                    role: Role::parse(&r.get::<_, String>(3)?).unwrap_or(Role::Mentioned),
                    ts: r.get(4)?,
                    evidence: r.get(5)?,
                },
            ))
        })?;
        for (id, l) in rows.flatten() {
            map.entry(id).or_default().push(l);
        }
        for v in map.values_mut() {
            v.sort_by_key(|l| (l.role, std::cmp::Reverse(l.number)));
        }
        Ok(map)
    }

    /// All sessions, newest first, with PR links and AI descriptions attached.
    pub fn load_sessions(&self) -> Result<Vec<SessionRow>> {
        let mut links = self.all_links()?;
        let mut st = self.conn.prepare(
            "SELECT s.id, s.path, s.project_dir, s.cwd, s.branch, s.repo, s.started, s.ended, s.user_prompts,
                    s.custom_title, s.summary, s.first_prompt, s.last_prompt, s.files_json, s.tests_ran, s.commits,
                    s.tokens_in, s.tokens_out, s.model, s.entrypoint, s.cloud_id, s.auto_desc, a.title, a.description, s.missing,
                    s.context_tokens, s.compactions, s.cost_usd, s.tickets_json,
                    m.pinned, m.hidden, m.note
             FROM sessions s LEFT JOIN ai a ON a.id = s.id LEFT JOIN marks m ON m.id = s.id
             ORDER BY COALESCE(s.ended, s.started) DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                path: r.get(1)?,
                project_dir: r.get(2)?,
                cwd: r.get(3)?,
                branch: r.get(4)?,
                repo: r.get(5)?,
                started: r.get(6)?,
                ended: r.get(7)?,
                user_prompts: r.get(8)?,
                custom_title: r.get(9)?,
                summary: r.get(10)?,
                first_prompt: r.get(11)?,
                last_prompt: r.get(12)?,
                files_edited: r
                    .get::<_, Option<String>>(13)?
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default(),
                tests_ran: r.get(14)?,
                commits: r.get(15)?,
                tokens_in: r.get::<_, i64>(16)? as u64,
                tokens_out: r.get::<_, i64>(17)? as u64,
                model: r.get(18)?,
                entrypoint: r.get(19)?,
                cloud_id: r.get(20)?,
                auto_desc: r.get::<_, Option<String>>(21)?.unwrap_or_default(),
                ai_title: r.get(22)?,
                ai_desc: r.get(23)?,
                prs: vec![],
                missing: r.get::<_, Option<bool>>(24)?.unwrap_or(false),
                context_tokens: r.get::<_, Option<i64>>(25)?.unwrap_or(0) as u64,
                compactions: r.get::<_, Option<u32>>(26)?.unwrap_or(0),
                cost_usd: r.get::<_, Option<f64>>(27)?.unwrap_or(0.0),
                tickets: vec![],
                pinned: r.get::<_, Option<bool>>(29)?.unwrap_or(false),
                hidden: r.get::<_, Option<bool>>(30)?.unwrap_or(false),
                note: r.get(31)?,
            }
            .with_ticket_candidates(r.get::<_, Option<String>>(28)?))
        })?;
        let mut out: Vec<(SessionRow, Vec<crate::tickets::Candidate>)> =
            rows.collect::<Result<_, _>>()?;
        // Prefixes trusted for keys that only appear in prompts: configured ones, and
        // any prefix seen in some session's branch name.
        let templates = crate::config::ticket_templates();
        let templates = &templates;
        let any_prefix = templates.contains_key("*");
        let mut trusted: std::collections::HashSet<String> =
            templates.keys().filter(|k| *k != "*").cloned().collect();
        for (_, c) in &out {
            trusted.extend(
                c.iter()
                    .filter(|c| c.strong)
                    .map(|c| crate::tickets::prefix(&c.key).to_string()),
            );
        }
        for (s, c) in &mut out {
            s.prs = links.remove(&s.id).unwrap_or_default();
            s.tickets = crate::tickets::resolve(c, &trusted, any_prefix);
        }
        Ok(out.into_iter().map(|(s, _)| s).collect())
    }

    fn mark(&self, id: &str, col: &str, val: &dyn rusqlite::ToSql) -> Result<()> {
        self.conn
            .execute("INSERT OR IGNORE INTO marks (id) VALUES (?1)", [id])?;
        self.conn.execute(
            &format!("UPDATE marks SET {col} = ?2 WHERE id = ?1"),
            params![id, val],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, id: &str, on: bool) -> Result<()> {
        self.mark(id, "pinned", &on)
    }

    pub fn set_hidden(&self, id: &str, on: bool) -> Result<()> {
        self.mark(id, "hidden", &on)
    }

    pub fn set_note(&self, id: &str, note: Option<&str>) -> Result<()> {
        self.mark(id, "note", &note.filter(|n| !n.trim().is_empty()))
    }

    /// Full-text search over prompts, replies and commands. Returns ids, best first.
    pub fn fts(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        let q = fts_query(query);
        if q.is_empty() {
            return Ok(vec![]);
        }
        let mut st = self.conn.prepare(
            "SELECT id FROM fts WHERE fts MATCH ?1 ORDER BY bm25(fts, 0.0, 5.0, 1.0) LIMIT ?2",
        )?;
        let rows = st.query_map(params![q, limit as i64], |r| r.get(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Prompts kept for one session (for AI descriptions).
    pub fn prompts_and_reply(&self, id: &str) -> Result<(Vec<String>, Option<String>, i64)> {
        self.conn
            .query_row(
                "SELECT prompts_json, last_reply, size FROM sessions WHERE id = ?1",
                [id],
                |r| {
                    let p: Option<String> = r.get(0)?;
                    Ok((
                        p.and_then(|j| serde_json::from_str(&j).ok())
                            .unwrap_or_default(),
                        r.get(1)?,
                        r.get(2)?,
                    ))
                },
            )
            .optional()?
            .context("session not found")
    }

    pub fn ai_size(&self, id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT src_size FROM ai WHERE id = ?1", [id], |r| r.get(0))
            .optional()?)
    }

    pub fn set_ai(&self, id: &str, title: &str, desc: &str, model: &str, size: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ai (id, title, description, model, created, src_size) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, title, desc, model, chrono::Utc::now().to_rfc3339(), size],
        )?;
        Ok(())
    }

    pub fn upsert_pr(&self, p: &PrMeta) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO prs (repo, number, title, state, author, url, head, created_at, body_session, claude_footer, synced)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![p.repo, p.number, p.title, p.state, p.author, p.url, p.head, p.created_at, p.body_session,
                    p.claude_footer, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn load_prs(&self) -> Result<Vec<PrMeta>> {
        let mut st = self.conn.prepare(
            "SELECT repo, number, title, state, author, url, head, created_at, body_session, claude_footer FROM prs",
        )?;
        let rows = st.query_map([], |r| {
            Ok(PrMeta {
                repo: r.get(0)?,
                number: r.get(1)?,
                title: r.get(2)?,
                state: r.get(3)?,
                author: r.get(4)?,
                url: r.get(5)?,
                head: r.get(6)?,
                created_at: r.get(7)?,
                body_session: r.get(8)?,
                claude_footer: r.get::<_, Option<bool>>(9)?.unwrap_or(false),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Repos seen anywhere in the index (for `ccrumbs sync`).
    pub fn known_repos(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT repo FROM session_prs WHERE repo != '' UNION SELECT repo FROM sessions WHERE repo IS NOT NULL AND repo != ''",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

/// Turn free text into a safe FTS5 query: every word must appear (prefix match on the last).
fn fts_query(q: &str) -> String {
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect();
    match words.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} {}*", rest.join(" "), last),
        Some((last, _)) => format!("{last}*"),
        None => String::new(),
    }
}
