//! The picker: type to find a session, Enter to jump back into it.

use crate::config::Config;
use crate::context::Context;
use crate::db::Db;
use crate::launch;
use crate::model::{Role, SessionRow};
use crate::prview::{self, Actor, PrEntry};
use crate::search::{self, Corpus, Opts, Ranker};
use crate::tasks::{self, Task};
use crate::text::{age, duration, home_short, local_time, truncate};
use crate::transcript::{self, Entry, Kind};
use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::{Frame, Terminal};

/// The TUI draws on stderr, so stdout stays free for `pick --print` / `--print-dir`
/// (shell helpers capture it with `$(...)`).
type Term = Terminal<CrosstermBackend<std::io::Stderr>>;

fn term_enter() -> Result<Term> {
    use ratatui::crossterm::{execute, terminal};
    terminal::enable_raw_mode()?;
    execute!(std::io::stderr(), terminal::EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(std::io::stderr()))?)
}

fn term_leave() {
    use ratatui::crossterm::{cursor, execute, terminal};
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stderr(),
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
}

/// What a key press asks the event loop to do.
enum Step {
    Stay,
    Exit(Outcome),
    /// Leave the screen, run this (a terminal editor), come back.
    Suspend(std::process::Command),
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    View,
    OpenPr,
    OpenTicket,
    OpenCloud,
    Editor,
    Export,
    Pin,
    Note,
    Hide,
    Describe,
    NewTab,
    CopyCmd,
    CopyId,
}

const MENU: &[(char, Action, &str)] = &[
    ('v', Action::View, "view the conversation"),
    ('b', Action::OpenPr, "open its PR in the browser"),
    ('k', Action::OpenTicket, "open its Jira / Linear ticket"),
    ('w', Action::OpenCloud, "open the claude.ai session"),
    ('e', Action::Editor, "open the project in your editor"),
    ('m', Action::Export, "export as Markdown"),
    ('p', Action::Pin, "pin / unpin"),
    ('n', Action::Note, "add / edit a note"),
    ('h', Action::Hide, "hide / unhide"),
    ('d', Action::Describe, "describe with Claude"),
    ('o', Action::NewTab, "resume in a new terminal tab"),
    ('y', Action::CopyCmd, "copy the resume command"),
    ('c', Action::CopyId, "copy the session id"),
];

/// Full-screen conversation reader.
struct Viewer {
    session: SessionRow,
    entries: Vec<Entry>,
    rows: Vec<(Style, String)>,
    width: u16,
    outputs: bool,
    scroll: usize,
    height: usize,
    input: Option<String>,
    find: String,
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for line in s.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut start = 0;
        while start < chars.len() {
            let mut end = (start + width).min(chars.len());
            if end < chars.len() {
                if let Some(sp) = chars[start..end].iter().rposition(|c| *c == ' ') {
                    if sp > width / 3 {
                        end = start + sp + 1;
                    }
                }
            }
            out.push(
                chars[start..end]
                    .iter()
                    .collect::<String>()
                    .trim_end()
                    .to_string(),
            );
            start = end;
        }
    }
    out
}

impl Viewer {
    fn new(session: SessionRow) -> Viewer {
        let entries = transcript::load(&session.path);
        Viewer {
            session,
            entries,
            rows: vec![],
            width: 0,
            outputs: false,
            scroll: 0,
            height: 20,
            input: None,
            find: String::new(),
        }
    }

    fn rebuild(&mut self, width: u16) {
        self.width = width;
        let w = width.saturating_sub(2) as usize;
        let mut rows: Vec<(Style, String)> = Vec::new();
        for e in &self.entries {
            let when = local_time(e.ts.as_deref());
            match e.kind {
                Kind::User => {
                    rows.push((Style::new(), String::new()));
                    rows.push((
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        format!("▶ You · {when}"),
                    ));
                    rows.extend(
                        wrap(&e.text, w)
                            .into_iter()
                            .map(|l| (Style::new().fg(Color::White), l)),
                    );
                }
                Kind::Assistant => {
                    rows.push((Style::new(), String::new()));
                    rows.push((
                        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                        "◀ Claude".to_string(),
                    ));
                    rows.extend(wrap(&e.text, w).into_iter().map(|l| (Style::new(), l)));
                }
                Kind::Tool => rows.push((
                    Style::new().fg(Color::Yellow),
                    format!("  ⚙ {}", truncate(&e.text, w.saturating_sub(4))),
                )),
                Kind::Output | Kind::Error if self.outputs => {
                    let st = if e.kind == Kind::Error {
                        Style::new().fg(Color::Red)
                    } else {
                        DIM
                    };
                    for l in e.text.lines().take(12) {
                        rows.push((st, format!("    │ {}", truncate(l, w.saturating_sub(6)))));
                    }
                }
                Kind::Meta => rows.push((Style::new().fg(Color::Magenta), e.text.clone())),
                _ => {}
            }
        }
        if rows.is_empty() {
            rows.push((DIM, "(this transcript has no readable messages)".into()));
        }
        self.rows = rows;
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
    }

    fn find_next(&mut self, forward: bool) -> bool {
        let q = self.find.to_lowercase();
        if q.is_empty() {
            return false;
        }
        let n = self.rows.len();
        for k in 1..=n {
            let i = if forward {
                (self.scroll + 2 + k) % n
            } else {
                (self.scroll + 2 + n * 2 - k) % n
            };
            if self.rows[i].1.to_lowercase().contains(&q) {
                self.scroll = i.saturating_sub(2);
                return true;
            }
        }
        false
    }
}
use std::collections::HashMap;

pub enum Outcome {
    Resume(Box<SessionRow>),
    Quit,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Sessions,
    Tasks,
    Prs,
}

struct App {
    sessions: Vec<SessionRow>,
    prs: Vec<PrEntry>,
    pr_by_key: HashMap<(String, u32), usize>,
    tasks: Vec<Task>,
    corpus: Corpus,
    ranker: Ranker,
    ctx: Option<Context>,
    here_first: bool,
    hide_quick: bool,
    query: String,
    tab: Tab,
    deep: bool,
    shown: Vec<usize>,
    pr_shown: Vec<usize>,
    task_shown: Vec<usize>,
    list: ListState,
    pr_list: ListState,
    task_list: ListState,
    preview_scroll: u16,
    status: String,
    viewer: Option<Viewer>,
    menu: bool,
    /// Note being edited for the target session.
    note_input: Option<(String, String)>,
}

pub fn role_color(r: Role) -> Color {
    match r {
        Role::Created => Color::Green,
        Role::Reviewed => Color::Magenta,
        Role::Merged => Color::Blue,
        Role::Commented => Color::Yellow,
        Role::WorkedOn => Color::Cyan,
        Role::Viewed | Role::Mentioned => Color::DarkGray,
    }
}

pub fn ctx_color(pct: u32) -> Color {
    match pct {
        0..50 => Color::Green,
        50..80 => Color::Yellow,
        _ => Color::Red,
    }
}

const DIM: Style = Style::new().fg(Color::DarkGray);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

impl App {
    fn new(db: &Db, initial: &str, ctx: Option<Context>) -> Result<App> {
        let mut app = App {
            sessions: vec![],
            prs: vec![],
            pr_by_key: HashMap::new(),
            tasks: vec![],
            corpus: Corpus::new(&[], None, vec![]),
            ranker: Ranker::default(),
            here_first: ctx.is_some(),
            ctx,
            hide_quick: false,
            query: initial.to_string(),
            tab: Tab::Sessions,
            deep: false,
            shown: vec![],
            pr_shown: vec![],
            task_shown: vec![],
            list: ListState::default(),
            pr_list: ListState::default(),
            task_list: ListState::default(),
            preview_scroll: 0,
            status: String::new(),
            viewer: None,
            menu: false,
            note_input: None,
        };
        app.reload(db)?;
        Ok(app)
    }

    fn reload(&mut self, db: &Db) -> Result<()> {
        self.sessions = db.load_sessions()?;
        self.prs = prview::build(&self.sessions, &db.load_prs()?);
        self.pr_by_key = self
            .prs
            .iter()
            .enumerate()
            .map(|(i, p)| ((p.repo.clone(), p.number), i))
            .collect();
        let (tasks, task_of) = tasks::build(&self.sessions, &self.prs);
        self.tasks = tasks;
        self.corpus = Corpus::new(&self.sessions, self.ctx.as_ref(), task_of);
        self.refilter(db);
        Ok(())
    }

    fn refilter(&mut self, db: &Db) {
        let q = search::parse(&self.query);
        self.shown = if self.deep && !q.text.is_empty() {
            let ids = db.fts(&q.text, 300).unwrap_or_default();
            let pos: HashMap<&str, usize> = self
                .sessions
                .iter()
                .enumerate()
                .map(|(i, s)| (s.id.as_str(), i))
                .collect();
            ids.iter()
                .filter_map(|id| pos.get(id.as_str()).copied())
                .filter(|&i| search::passes(&self.sessions[i], self.corpus.task_of[i], &q))
                .collect()
        } else {
            let opts = Opts {
                here_first: self.here_first,
                hide_quick: self.hide_quick,
            };
            self.ranker.rank(&self.sessions, &self.corpus, &q, opts)
        };
        // Tasks follow the session ranking: a task shows up where its best session is.
        let mut seen = vec![false; self.tasks.len()];
        self.task_shown = self
            .shown
            .iter()
            .filter_map(|&i| self.corpus.task_of[i])
            .filter(|&t| !std::mem::replace(&mut seen[t], true))
            .collect();
        let text = q.text.to_lowercase();
        self.pr_shown = (0..self.prs.len())
            .filter(|&i| {
                let p = &self.prs[i];
                if !q.prs.is_empty() {
                    return !prview::lookup(std::slice::from_ref(p), &format_ref(&q.prs[0]))
                        .is_empty();
                }
                if text.is_empty() {
                    return p.has_significant();
                }
                let hay = format!(
                    "{} {} {}",
                    p.key(),
                    p.title().unwrap_or(""),
                    p.story(&self.sessions)
                )
                .to_lowercase();
                text.split_whitespace().all(|w| hay.contains(w))
            })
            .collect();
        self.list
            .select(if self.shown.is_empty() { None } else { Some(0) });
        self.task_list.select(if self.task_shown.is_empty() {
            None
        } else {
            Some(0)
        });
        self.pr_list.select(if self.pr_shown.is_empty() {
            None
        } else {
            Some(0)
        });
        self.preview_scroll = 0;
    }

    fn selected(&self) -> Option<&SessionRow> {
        self.list
            .selected()
            .and_then(|i| self.shown.get(i))
            .map(|&i| &self.sessions[i])
    }

    fn selected_task(&self) -> Option<&Task> {
        self.task_list
            .selected()
            .and_then(|i| self.task_shown.get(i))
            .map(|&i| &self.tasks[i])
    }

    fn selected_pr(&self) -> Option<&PrEntry> {
        self.pr_list
            .selected()
            .and_then(|i| self.pr_shown.get(i))
            .map(|&i| &self.prs[i])
    }

    fn move_by(&mut self, d: i32) {
        let (state, len) = match self.tab {
            Tab::Sessions => (&mut self.list, self.shown.len()),
            Tab::Tasks => (&mut self.task_list, self.task_shown.len()),
            Tab::Prs => (&mut self.pr_list, self.pr_shown.len()),
        };
        if len == 0 {
            return;
        }
        let cur = state.selected().unwrap_or(0) as i32;
        state.select(Some((cur + d).clamp(0, len as i32 - 1) as usize));
        self.preview_scroll = 0;
    }
}

fn format_ref((repo, n): &(Option<String>, u32)) -> String {
    match repo {
        Some(r) => format!("{r}#{n}"),
        None => n.to_string(),
    }
}

pub fn run(db: &mut Db, cfg: &Config, initial: &str) -> Result<Outcome> {
    let mut app = App::new(db, initial, Context::detect())?;
    if app.sessions.is_empty() {
        app.status = format!(
            "No sessions found under {} — run Claude Code first, or set claude_projects_dir",
            cfg.projects_dir().display()
        );
    }
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term_leave();
        hook(info);
    }));
    let mut term = term_enter()?;
    let res = event_loop(&mut term, &mut app, db, cfg);
    term_leave();
    res
}

fn event_loop(term: &mut Term, app: &mut App, db: &mut Db, cfg: &Config) -> Result<Outcome> {
    loop {
        term.draw(|f| draw(f, app))?;
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match on_key(app, k, db, cfg)? {
            Step::Stay => {}
            Step::Exit(out) => return Ok(out),
            Step::Suspend(mut cmd) => {
                term_leave();
                let res = cmd.status();
                *term = term_enter()?;
                term.clear()?;
                app.status = match res {
                    Ok(st) if st.success() => "Back from the editor".into(),
                    Ok(st) => format!("Editor exited with {st}"),
                    Err(e) => format!("Could not start the editor: {e}"),
                };
            }
        }
    }
}

impl App {
    /// The session an action applies to: the selected one, a task's newest session,
    /// or the session that created the selected PR.
    fn target(&self) -> Option<&SessionRow> {
        match self.tab {
            Tab::Sessions => self.selected(),
            Tab::Tasks => self.selected_task().map(|t| &self.sessions[t.sessions[0]]),
            Tab::Prs => self
                .selected_pr()
                .and_then(|p| p.links.iter().find_map(|l| l.session))
                .map(|i| &self.sessions[i]),
        }
    }

    /// URL of the PR most relevant to the current selection.
    fn target_pr_url(&self) -> Option<String> {
        match self.tab {
            Tab::Prs => self.selected_pr().and_then(pr_url),
            Tab::Tasks => self
                .selected_task()
                .and_then(|t| t.prs.first())
                .and_then(|&pi| pr_url(&self.prs[pi])),
            Tab::Sessions => {
                let s = self.selected()?;
                let (l, _) = s
                    .pr_summary()
                    .into_iter()
                    .find(|(l, _)| l.role.is_significant())?;
                match self.pr_by_key.get(&(l.repo.clone(), l.number)) {
                    Some(&pi) => pr_url(&self.prs[pi]),
                    None => (!l.repo.is_empty())
                        .then(|| format!("https://github.com/{}/pull/{}", l.repo, l.number)),
                }
            }
        }
    }

    fn target_ticket(&self) -> Option<String> {
        match self.tab {
            Tab::Tasks => self
                .selected_task()
                .and_then(|t| t.tickets.first().cloned()),
            _ => self.target().and_then(|s| s.tickets.first().cloned()),
        }
    }

    /// Reload after changing marks, keeping the selection where it was.
    fn reload_keep(&mut self, db: &Db) -> Result<()> {
        let (a, b, c) = (
            self.list.selected(),
            self.task_list.selected(),
            self.pr_list.selected(),
        );
        self.reload(db)?;
        let clamp = |sel: Option<usize>, len: usize| {
            sel.map(|i| i.min(len.saturating_sub(1)))
                .filter(|_| len > 0)
        };
        self.list.select(clamp(a, self.shown.len()));
        self.task_list.select(clamp(b, self.task_shown.len()));
        self.pr_list.select(clamp(c, self.pr_shown.len()));
        Ok(())
    }
}

fn open(app: &mut App, url: Option<String>, what: &str) {
    app.status = match url {
        Some(u) => match launch::open_url(&u) {
            Ok(()) => format!("Opened {u}"),
            Err(e) => format!("{e}"),
        },
        None => format!("No {what} for this selection"),
    };
}

fn do_action(app: &mut App, a: Action, db: &mut Db, cfg: &Config) -> Result<Step> {
    match a {
        Action::OpenPr => {
            let url = app.target_pr_url();
            open(app, url, "PR");
        }
        Action::OpenTicket => match app.target_ticket() {
            None => app.status = "No ticket key (like ABC-123) found for this selection".into(),
            Some(key) => match crate::config::ticket_url(&key) {
                Some(u) => open(app, Some(u), "ticket"),
                None => {
                    app.status = format!(
                        "{key}: add \"tickets\": {{\"{}\": \"https://…/browse/{{key}}\"}} to {}",
                        crate::tickets::prefix(&key),
                        crate::config::config_path().display()
                    )
                }
            },
        },
        Action::OpenCloud => {
            let url = app.target().and_then(|s| {
                s.cloud_id
                    .as_ref()
                    .map(|c| format!("https://claude.ai/code/{c}"))
            });
            open(app, url, "claude.ai session");
        }
        Action::View => {
            if let Some(s) = app.target().cloned() {
                if !std::path::Path::new(&s.path).exists() {
                    app.status = "The transcript file is gone (Claude Code retention).".into();
                } else {
                    app.viewer = Some(Viewer::new(s));
                }
            }
        }
        Action::Editor => {
            if let Some(s) = app.target() {
                match launch::editor_command(&s.resume_dir()) {
                    Err(e) => app.status = format!("{e}"),
                    Ok((mut cmd, true)) => {
                        app.status = match cmd
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                        {
                            Ok(_) => {
                                format!("Opened {} in your editor", home_short(&s.resume_dir()))
                            }
                            Err(e) => format!("Could not start the editor: {e}"),
                        };
                    }
                    Ok((cmd, false)) => return Ok(Step::Suspend(cmd)),
                }
            }
        }
        Action::Export => {
            if let Some(s) = app.target().cloned() {
                let dir = crate::config::data_dir().join("exports");
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(transcript::file_name(&s));
                let md = transcript::to_markdown(&s, &transcript::load(&s.path), false);
                std::fs::write(&path, md)?;
                launch::copy(&path.to_string_lossy());
                app.status = format!("Exported to {} (path copied)", path.display());
            }
        }
        Action::Pin => {
            if let Some((id, on)) = app.target().map(|s| (s.id.clone(), !s.pinned)) {
                db.set_pinned(&id, on)?;
                app.reload_keep(db)?;
                app.status = if on {
                    "Pinned: it stays on top of the list".into()
                } else {
                    "Unpinned".into()
                };
            }
        }
        Action::Hide => {
            if let Some((id, on)) = app.target().map(|s| (s.id.clone(), !s.hidden)) {
                db.set_hidden(&id, on)?;
                app.reload_keep(db)?;
                app.status = if on {
                    "Hidden. Type is:hidden to see hidden sessions.".into()
                } else {
                    "Unhidden".into()
                };
            }
        }
        Action::Note => {
            if let Some(s) = app.target() {
                app.note_input = Some((s.id.clone(), s.note.clone().unwrap_or_default()));
            }
        }
        Action::Describe => {
            if let Some(s) = app.target().cloned() {
                app.status = match crate::describe::describe_one(db, &s, &cfg.describe_model) {
                    Ok((t, _)) => {
                        app.reload_keep(db)?;
                        format!("Described: {t}")
                    }
                    Err(e) => format!("describe failed: {e}"),
                };
            }
        }
        Action::NewTab => {
            if let Some(s) = app.target() {
                app.status = match launch::open_tab(s) {
                    Ok(m) => m,
                    Err(e) => format!("{e} — use ctrl-y to copy the command"),
                };
            }
        }
        Action::CopyCmd => {
            if let Some(s) = app.target() {
                let cmd = launch::resume_command(s);
                launch::copy(&cmd);
                app.status = format!("Copied: {cmd}");
            }
        }
        Action::CopyId => {
            if let Some(s) = app.target() {
                launch::copy(&s.id);
                app.status = format!("Copied: {}", s.id);
            }
        }
    }
    Ok(Step::Stay)
}

fn viewer_key(app: &mut App, k: KeyEvent) -> Step {
    let Some(v) = app.viewer.as_mut() else {
        return Step::Stay;
    };
    if let Some(input) = v.input.as_mut() {
        match k.code {
            KeyCode::Esc => v.input = None,
            KeyCode::Enter => {
                v.find = std::mem::take(input);
                v.input = None;
                if !v.find_next(true) {
                    app.status = format!("\"{}\" not found", v.find);
                }
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
        return Step::Stay;
    }
    let page = v.height.saturating_sub(2).max(1);
    let last = v.rows.len().saturating_sub(1);
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => app.viewer = None,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            return Step::Exit(Outcome::Quit);
        }
        KeyCode::Enter => {
            let s = v.session.clone();
            if !s.missing {
                return Step::Exit(Outcome::Resume(Box::new(s)));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => v.scroll = (v.scroll + 1).min(last),
        KeyCode::PageUp | KeyCode::Char('b') => v.scroll = v.scroll.saturating_sub(page),
        KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
            v.scroll = (v.scroll + page).min(last)
        }
        KeyCode::Home | KeyCode::Char('g') => v.scroll = 0,
        KeyCode::End | KeyCode::Char('G') => v.scroll = last.saturating_sub(page),
        KeyCode::Char('/') => v.input = Some(String::new()),
        KeyCode::Char('n') => {
            if !v.find_next(true) && !v.find.is_empty() {
                app.status = format!("\"{}\" not found", v.find);
            }
        }
        KeyCode::Char('N') => {
            v.find_next(false);
        }
        KeyCode::Char('o') => {
            v.outputs = !v.outputs;
            v.width = 0; // force rebuild
        }
        _ => {}
    }
    Step::Stay
}

fn note_key(app: &mut App, k: KeyEvent, db: &mut Db) -> Result<Step> {
    let Some((id, text)) = app.note_input.as_mut() else {
        return Ok(Step::Stay);
    };
    match k.code {
        KeyCode::Esc => app.note_input = None,
        KeyCode::Enter => {
            let (id, text) = (id.clone(), text.trim().to_string());
            db.set_note(&id, Some(&text))?;
            app.note_input = None;
            app.reload_keep(db)?;
            app.status = if text.is_empty() {
                "Note cleared".into()
            } else {
                "Note saved (it's searchable too)".into()
            };
        }
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char(c) => text.push(c),
        _ => {}
    }
    Ok(Step::Stay)
}

fn on_key(app: &mut App, k: KeyEvent, db: &mut Db, cfg: &Config) -> Result<Step> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    app.status.clear();
    if app.viewer.is_some() {
        return Ok(viewer_key(app, k));
    }
    if app.note_input.is_some() {
        return note_key(app, k, db);
    }
    if app.menu {
        app.menu = false;
        if let KeyCode::Char(c) = k.code {
            if let Some((_, a, _)) = MENU.iter().find(|(key, _, _)| *key == c) {
                return do_action(app, *a, db, cfg);
            }
        }
        return Ok(Step::Stay);
    }
    let action = match (k.code, ctrl) {
        (KeyCode::Char('a'), true) => {
            app.menu = true;
            return Ok(Step::Stay);
        }
        (KeyCode::Char('b'), true) => Some(Action::OpenPr),
        (KeyCode::Char('k'), true) => Some(Action::OpenTicket),
        (KeyCode::Char('v'), true) => Some(Action::View),
        (KeyCode::Char('e'), true) => Some(Action::Editor),
        (KeyCode::Char('s'), true) => Some(Action::Pin),
        (KeyCode::Char('d'), true) => Some(Action::Describe),
        (KeyCode::Char('o'), true) => Some(Action::NewTab),
        (KeyCode::Char('y'), true) => Some(Action::CopyCmd),
        _ => None,
    };
    if let Some(a) = action {
        return do_action(app, a, db, cfg);
    }
    match (k.code, ctrl) {
        (KeyCode::Char('c'), true) => return Ok(Step::Exit(Outcome::Quit)),
        (KeyCode::Esc, _) => {
            if app.query.is_empty() {
                return Ok(Step::Exit(Outcome::Quit));
            }
            app.query.clear();
            app.refilter(db);
        }
        (KeyCode::Enter, _) => match app.tab {
            Tab::Sessions => {
                if let Some(s) = app.selected() {
                    if s.missing {
                        app.status = "This transcript was deleted by Claude Code's retention; it can't be resumed.".into();
                    } else {
                        return Ok(Step::Exit(Outcome::Resume(Box::new(s.clone()))));
                    }
                }
            }
            Tab::Tasks => {
                if let Some(t) = app.selected_task() {
                    let (id, title) = (t.id, t.title.clone());
                    app.query = format!("task:{id}");
                    app.tab = Tab::Sessions;
                    app.refilter(db);
                    app.status = format!(
                        "Sessions of task \"{}\" — Enter to resume",
                        truncate(&title, 50)
                    );
                }
            }
            Tab::Prs => {
                if let Some(p) = app.selected_pr() {
                    app.query = if p.repo.is_empty() {
                        format!("#{}", p.number)
                    } else {
                        p.key()
                    };
                    app.tab = Tab::Sessions;
                    app.refilter(db);
                    app.status = format!("Sessions linked to {} — Enter to resume", app.query);
                }
            }
        },
        (KeyCode::Tab, _) => {
            app.tab = match app.tab {
                Tab::Sessions => Tab::Tasks,
                Tab::Tasks => Tab::Prs,
                Tab::Prs => Tab::Sessions,
            };
            app.preview_scroll = 0;
        }
        (KeyCode::BackTab, _) => {
            app.tab = match app.tab {
                Tab::Sessions => Tab::Prs,
                Tab::Tasks => Tab::Sessions,
                Tab::Prs => Tab::Tasks,
            };
            app.preview_scroll = 0;
        }
        (KeyCode::Char('g'), true) => match &app.ctx {
            None => {
                app.status =
                    "Not started inside a project, so there's no \"here\" to rank by.".into()
            }
            Some(c) => {
                app.here_first = !app.here_first;
                app.status = if app.here_first {
                    format!("Sessions from {} first", c.label())
                } else {
                    "Global ranking: newest first, wherever they ran".into()
                };
                app.refilter(db);
            }
        },
        (KeyCode::Char('t'), true) => {
            app.hide_quick = !app.hide_quick;
            app.status = if app.hide_quick {
                "Hiding one-off quick sessions (1 prompt, no edits, no PRs)".into()
            } else {
                "Showing all sessions".into()
            };
            app.refilter(db);
        }
        (KeyCode::Up, _) | (KeyCode::Char('p'), true) => app.move_by(-1),
        (KeyCode::Down, _) | (KeyCode::Char('n'), true) => app.move_by(1),
        (KeyCode::PageUp, _) => app.move_by(-10),
        (KeyCode::PageDown, _) => app.move_by(10),
        (KeyCode::Home, _) => app.move_by(-100_000),
        (KeyCode::End, _) => app.move_by(100_000),
        (KeyCode::Right, _) => app.preview_scroll = app.preview_scroll.saturating_add(3),
        (KeyCode::Left, _) => app.preview_scroll = app.preview_scroll.saturating_sub(3),
        (KeyCode::Char('u'), true) => {
            app.query.clear();
            app.refilter(db);
        }
        (KeyCode::Char('w'), true) => {
            let t = app.query.trim_end().len();
            app.query
                .truncate(app.query[..t].rfind(' ').map(|i| i + 1).unwrap_or(0));
            app.refilter(db);
        }
        (KeyCode::Char('f'), true) => {
            app.deep = !app.deep;
            app.status = if app.deep {
                "Full-text search ON (prompts, replies, commands)".into()
            } else {
                "Full-text search OFF".into()
            };
            app.refilter(db);
        }
        (KeyCode::Char('r'), true) => {
            let rep = crate::index::run(db, cfg, false)?;
            app.reload(db)?;
            app.status = format!(
                "Reindexed: {} updated of {} in {}ms",
                rep.updated, rep.scanned, rep.millis
            );
        }
        (KeyCode::Backspace, _) => {
            app.query.pop();
            app.refilter(db);
        }
        (KeyCode::Char(c), false) => {
            app.query.push(c);
            app.refilter(db);
        }
        _ => {}
    }
    Ok(Step::Stay)
}

fn pr_url(p: &PrEntry) -> Option<String> {
    p.meta.as_ref().and_then(|m| m.url.clone()).or_else(|| {
        (!p.repo.is_empty()).then(|| format!("https://github.com/{}/pull/{}", p.repo, p.number))
    })
}

fn draw(f: &mut Frame, app: &mut App) {
    let [top, main, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let tab = |t: Tab, label: String| {
        if app.tab == t {
            Span::styled(
                format!(" {label} "),
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), DIM)
        }
    };
    let mut head = vec![
        tab(
            Tab::Sessions,
            format!("Sessions {}/{}", app.shown.len(), app.sessions.len()),
        ),
        tab(Tab::Tasks, format!("Tasks {}", app.task_shown.len())),
        tab(Tab::Prs, format!("PRs {}", app.pr_shown.len())),
        Span::raw("  "),
    ];
    if let (Some(c), true) = (&app.ctx, app.here_first) {
        head.push(Span::styled(
            format!("◉ {} ", c.label()),
            Style::new().fg(Color::Green),
        ));
    }
    if app.hide_quick {
        head.push(Span::styled(
            "[quick hidden] ",
            Style::new().fg(Color::Yellow),
        ));
    }
    if app.deep {
        head.push(Span::styled("[full-text] ", Style::new().fg(Color::Yellow)));
    }
    head.push(Span::styled(
        "❯ ",
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ));
    head.push(Span::raw(app.query.clone()));
    head.push(Span::styled("█", Style::new().fg(Color::Cyan)));
    if app.query.is_empty() {
        head.push(Span::styled(" search · #123 · p: b: f: @today has:pr", DIM));
    }
    f.render_widget(Paragraph::new(Line::from(head)), top);

    let wide = main.width >= 110;
    let [left, right] = if wide {
        Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)]).areas(main)
    } else {
        let [a, b] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);
        [a, b]
    };

    if app.viewer.is_some() {
        draw_viewer(f, app, main, bottom);
        return;
    }
    match app.tab {
        Tab::Sessions => {
            draw_session_list(f, app, left);
            draw_session_preview(f, app, right);
        }
        Tab::Tasks => {
            draw_task_list(f, app, left);
            draw_task_preview(f, app, right);
        }
        Tab::Prs => {
            draw_pr_list(f, app, left);
            draw_pr_preview(f, app, right);
        }
    }

    if app.menu {
        draw_menu(f, app, main);
    }
    let help = if let Some((_, text)) = &app.note_input {
        Line::from(vec![
            Span::styled(
                "Note: ",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.clone()),
            Span::styled("█", Style::new().fg(Color::Yellow)),
            Span::styled("   enter save · esc cancel · empty clears", DIM),
        ])
    } else if !app.status.is_empty() {
        Line::from(Span::styled(
            app.status.clone(),
            Style::new().fg(Color::Yellow),
        ))
    } else {
        let k = |key: &'static str, what: &'static str| {
            vec![
                Span::styled(key, Style::new().fg(Color::Cyan)),
                Span::styled(format!(" {what}  "), DIM),
            ]
        };
        let mut v = Vec::new();
        match app.tab {
            Tab::Sessions => v.extend(k("enter", "resume")),
            Tab::Tasks | Tab::Prs => v.extend(k("enter", "sessions")),
        }
        v.extend(k("^a", "actions"));
        v.extend(k("^v", "read"));
        v.extend(k("^b", "PR"));
        v.extend(k("^k", "ticket"));
        v.extend(k("^s", "pin"));
        v.extend(k("tab", "view"));
        v.extend(k("^g", "here/all"));
        v.extend(k("^t", "quick"));
        v.extend(k("^f", "full-text"));
        v.extend(k("esc", "quit"));
        Line::from(v)
    };
    f.render_widget(Paragraph::new(help), bottom);
}

fn draw_menu(f: &mut Frame, app: &App, area: Rect) {
    let target = app
        .target()
        .map(|s| truncate(&s.title(), 44))
        .unwrap_or_else(|| "nothing selected".into());
    let mut lines = vec![Line::from(Span::styled(target, BOLD)), Line::raw("")];
    for (key, _, what) in MENU {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {key} "),
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {what}")),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("any other key closes", DIM)));
    let h = (lines.len() as u16 + 2).min(area.height);
    let w = 52.min(area.width);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Actions ")
                .border_style(Style::new().fg(Color::Cyan)),
        ),
        r,
    );
}

fn draw_viewer(f: &mut Frame, app: &mut App, main: Rect, bottom: Rect) {
    let Some(v) = app.viewer.as_mut() else { return };
    let inner_w = main.width.saturating_sub(2);
    if v.width != inner_w {
        v.rebuild(inner_w);
    }
    v.height = main.height.saturating_sub(2) as usize;
    let q = v.find.to_lowercase();
    let lines: Vec<Line> = v
        .rows
        .iter()
        .skip(v.scroll)
        .take(v.height)
        .map(|(st, t)| {
            if !q.is_empty() && t.to_lowercase().contains(&q) {
                Line::from(Span::styled(t.clone(), st.bg(Color::Indexed(58))))
            } else {
                Line::from(Span::styled(t.clone(), *st))
            }
        })
        .collect();
    let pct = if v.rows.len() <= v.height {
        100
    } else {
        (v.scroll + v.height).min(v.rows.len()) * 100 / v.rows.len()
    };
    let title = format!(
        " {} · {} · {pct}% ",
        truncate(&v.session.title(), 60),
        v.session.short_id()
    );
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(Color::Cyan)),
        ),
        main,
    );
    let help = if let Some(input) = &v.input {
        Line::from(vec![
            Span::styled("/", Style::new().fg(Color::Yellow)),
            Span::raw(input.clone()),
            Span::styled("█", Style::new().fg(Color::Yellow)),
        ])
    } else if !app.status.is_empty() {
        Line::from(Span::styled(
            app.status.clone(),
            Style::new().fg(Color::Yellow),
        ))
    } else {
        let k = |key: &'static str, what: &'static str| {
            vec![
                Span::styled(key, Style::new().fg(Color::Cyan)),
                Span::styled(format!(" {what}  "), DIM),
            ]
        };
        let mut x = Vec::new();
        x.extend(k("enter", "resume this session"));
        x.extend(k("↑↓ space b", "scroll"));
        x.extend(k("g G", "top/end"));
        x.extend(k("/", "find"));
        x.extend(k("n N", "next/prev"));
        x.extend(k(
            "o",
            if v.outputs {
                "hide tool output"
            } else {
                "show tool output"
            },
        ));
        x.extend(k("esc", "back"));
        Line::from(x)
    };
    f.render_widget(Paragraph::new(help), bottom);
}

fn pr_badges(s: &SessionRow, max: usize) -> Vec<Span<'static>> {
    let mut v = Vec::new();
    let mut seen = Vec::new();
    for l in s.prs.iter().filter(|l| l.role.is_significant()) {
        if seen.contains(&l.number) {
            continue;
        }
        seen.push(l.number);
        if seen.len() > max {
            v.push(Span::styled("…", DIM));
            break;
        }
        v.push(Span::styled(
            format!(" {}#{}", l.role.icon(), l.number),
            Style::new().fg(role_color(l.role)),
        ));
    }
    v
}

fn draw_session_list(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .shown
        .iter()
        .map(|&i| {
            let s = &app.sessions[i];
            let tier = if app.here_first {
                app.corpus.tier[i]
            } else {
                0
            };
            let task = app.corpus.task_of[i].map(|t| app.tasks[t].sessions.len());
            let a = format!("{:>4} ", age(s.ended.as_deref().or(s.started.as_deref())));
            let mut l1 = vec![Span::styled(a, DIM)];
            let title_style = if s.missing { DIM } else { BOLD };
            if s.pinned {
                l1.push(Span::styled("★ ", Style::new().fg(Color::Yellow)));
            }
            l1.push(Span::styled(
                truncate(&s.title(), width.saturating_sub(6)),
                title_style,
            ));
            let here = match tier {
                crate::context::TIER_BRANCH => Span::styled("   ◉ ", Style::new().fg(Color::Green)),
                crate::context::TIER_PLACE => Span::styled("   ○ ", Style::new().fg(Color::Green)),
                _ => Span::raw("     "),
            };
            let mut l2 = vec![
                here,
                Span::styled(s.project_name(), Style::new().fg(Color::Blue)),
            ];
            if let Some(b) = &s.branch {
                l2.push(Span::styled(format!(" ⎇ {}", truncate(b, 28)), DIM));
            }
            if let Some(pct) = s.context_pct() {
                l2.push(Span::styled(
                    format!(" ◔{pct}%"),
                    Style::new().fg(ctx_color(pct)),
                ));
            }
            if s.cloud_id.is_some() || s.entrypoint.as_deref() == Some("remote") {
                l2.push(Span::styled(" ☁", DIM));
            }
            if s.missing {
                l2.push(Span::styled(
                    " (transcript deleted)",
                    Style::new().fg(Color::Red),
                ));
            }
            l2.extend(pr_badges(s, 4));
            if let Some(t) = s.tickets.first() {
                l2.push(Span::styled(format!(" ⚑{t}"), Style::new().fg(Color::Cyan)));
            }
            if s.note.is_some() {
                l2.push(Span::styled(" ✎", Style::new().fg(Color::Yellow)));
            }
            if s.hidden {
                l2.push(Span::styled(" (hidden)", DIM));
            }
            if let Some(n) = task {
                l2.push(Span::styled(
                    format!("  ⋯ task of {n}"),
                    Style::new().fg(Color::Yellow),
                ));
            }
            ListItem::new(Text::from(vec![Line::from(l1), Line::from(l2)]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::new().borders(Borders::RIGHT).border_style(DIM))
        .highlight_style(Style::new().bg(Color::Indexed(236)))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, area, &mut app.list);
}

fn wrap_lines(label: &str, body: &str, style: Style) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        label.to_string(),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))];
    for l in body.lines() {
        out.push(Line::from(Span::styled(l.to_string(), style)));
    }
    out
}

fn actor_name(app: &App, a: &Actor) -> String {
    match a {
        Actor::Local(i) => {
            let s = &app.sessions[*i];
            format!("{} \"{}\"", s.short_id(), truncate(&s.title(), 36))
        }
        Actor::Cloud(c) => format!("cloud session {}", truncate(c, 20)),
    }
}

fn draw_session_preview(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.selected() else {
        f.render_widget(
            Paragraph::new(Span::styled("  No matching sessions.", DIM)),
            area,
        );
        return;
    };
    let me = app.shown[app.list.selected().unwrap_or(0)];
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(s.title(), BOLD.fg(Color::White))),
        Line::from(vec![
            Span::styled(home_short(&s.resume_dir()), Style::new().fg(Color::Blue)),
            Span::styled(
                s.branch
                    .as_ref()
                    .map(|b| format!("  ⎇ {b}"))
                    .unwrap_or_default(),
                DIM,
            ),
        ]),
    ];
    let mut meta = format!(
        "{} · {} prompt{}",
        local_time(s.started.as_deref()),
        s.user_prompts,
        if s.user_prompts == 1 { "" } else { "s" }
    );
    if let Some(d) = duration(s.started.as_deref(), s.ended.as_deref()) {
        meta.push_str(&format!(" · {d}"));
    }
    if s.cost_usd > 0.0 {
        meta.push_str(&format!(" · ${:.2}", s.cost_usd));
    }
    if let Some(m) = &s.model {
        meta.push_str(&format!(" · {m}"));
    }
    lines.push(Line::from(Span::styled(meta, DIM)));
    if let (Some(c), Some(pct)) = (s.context_summary(), s.context_pct()) {
        let hint = if pct >= 80 {
            "  — nearly full: consider a fresh session"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(format!("◔ {c}"), Style::new().fg(ctx_color(pct))),
            Span::styled(hint, DIM),
        ]));
    }
    lines.push(Line::from(Span::styled(format!("id {}", s.id), DIM)));
    if let Some(n) = &s.note {
        lines.push(Line::from(vec![
            Span::styled("✎ ", Style::new().fg(Color::Yellow)),
            Span::styled(n.clone(), Style::new().fg(Color::Yellow)),
        ]));
    }
    for t in &s.tickets {
        let url = crate::config::ticket_url(t)
            .unwrap_or_else(|| "(^k needs a tickets URL in the config)".into());
        lines.push(Line::from(vec![
            Span::styled(format!("⚑ {t}  "), Style::new().fg(Color::Cyan)),
            Span::styled(url, DIM),
        ]));
    }
    if let Some(c) = &s.cloud_id {
        lines.push(Line::from(Span::styled(
            format!("☁ https://claude.ai/code/{c}"),
            DIM,
        )));
    }
    lines.push(Line::raw(""));
    let label = if s.ai_desc.is_some() {
        "Description (AI)"
    } else {
        "Description"
    };
    lines.extend(wrap_lines(label, s.description(), Style::new()));

    if let Some(t) = app.corpus.task_of[me].map(|t| &app.tasks[t]) {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                "Part of task ",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(truncate(&t.title, 50), BOLD),
            Span::styled(format!("  ({} sessions)", t.sessions.len()), DIM),
        ]));
        for &i in t.sessions.iter().filter(|&&i| i != me).take(4) {
            let o = &app.sessions[i];
            let when = if i < me { "later  " } else { "earlier" };
            lines.push(Line::from(vec![
                Span::styled(format!(" {when} "), DIM),
                Span::styled(format!("{} ", o.short_id()), Style::new().fg(Color::Blue)),
                Span::styled(format!("{:>4} ", age(o.ended.as_deref())), DIM),
                Span::raw(truncate(&o.title(), 48)),
            ]));
        }
    }
    let sig = s.pr_summary();
    if !sig.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Pull requests",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        for (l, also) in sig {
            let key = if l.repo.is_empty() {
                format!("#{}", l.number)
            } else {
                format!("{}#{}", l.repo, l.number)
            };
            let entry = app
                .pr_by_key
                .get(&(l.repo.clone(), l.number))
                .map(|&i| &app.prs[i]);
            let title = entry
                .and_then(|e| e.title())
                .map(|t| format!(" {}", truncate(t, 40)))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} {:<9} ", l.role.icon(), l.role.verb()),
                    Style::new()
                        .fg(role_color(l.role))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(key),
                Span::styled(title, DIM),
                Span::styled(
                    if also.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "  (also {})",
                            also.iter()
                                .map(|r| r.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                    DIM,
                ),
            ]));
            // Cross-session context: who else created / reviewed this PR.
            if let Some(e) = entry {
                for (role, actors) in [
                    (Role::Created, e.creators()),
                    (Role::Reviewed, e.reviewers()),
                ] {
                    let others: Vec<String> = actors
                        .iter()
                        .filter(|a| **a != Actor::Local(me))
                        .map(|a| actor_name(app, a))
                        .collect();
                    if !others.is_empty() {
                        lines.push(Line::from(vec![
                            Span::raw("     ↳ "),
                            Span::styled(
                                format!("{} in another session: ", role.as_str()),
                                Style::new().fg(role_color(role)),
                            ),
                            Span::raw(others.join(", ")),
                        ]));
                    }
                }
            }
        }
    }
    if !s.files_edited.is_empty() {
        lines.push(Line::raw(""));
        let files: Vec<&str> = s.files_edited.iter().take(12).map(String::as_str).collect();
        let more = s.files_edited.len().saturating_sub(12);
        let body = files.join("\n")
            + &if more > 0 {
                format!("\n… +{more} more")
            } else {
                String::new()
            };
        lines.extend(wrap_lines(
            &format!("Files edited ({})", s.files_edited.len()),
            &body,
            DIM,
        ));
    }
    if let Some(lp) = s
        .last_prompt
        .as_ref()
        .filter(|lp| s.user_prompts > 1 && Some(*lp) != s.first_prompt.as_ref())
    {
        lines.push(Line::raw(""));
        lines.extend(wrap_lines("Last prompt", &truncate(lp, 600), Style::new()));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0))
            .block(Block::new().borders(Borders::NONE)),
        pad(area),
    );
}

fn pad(a: Rect) -> Rect {
    Rect {
        x: a.x + 1,
        y: a.y,
        width: a.width.saturating_sub(2),
        height: a.height,
    }
}

fn draw_pr_list(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .pr_shown
        .iter()
        .map(|&i| {
            let p = &app.prs[i];
            let mut l1 = vec![];
            if p.born_from_claude() {
                l1.push(Span::styled("● ", Style::new().fg(Color::Green)));
            } else {
                l1.push(Span::styled("○ ", DIM));
            }
            l1.push(Span::styled(p.key(), BOLD));
            if let Some(st) = p.state() {
                let c = match st {
                    "open" => Color::Green,
                    "merged" => Color::Magenta,
                    _ => Color::Red,
                };
                l1.push(Span::styled(format!(" [{st}]"), Style::new().fg(c)));
            }
            if let Some(t) = p.title() {
                l1.push(Span::raw(format!(
                    " {}",
                    truncate(t, width.saturating_sub(p.key().len() + 12))
                )));
            }
            let story = truncate(&p.story(&app.sessions), width.saturating_sub(4));
            ListItem::new(Text::from(vec![
                Line::from(l1),
                Line::from(Span::styled(format!("  {story}"), DIM)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::new().borders(Borders::RIGHT).border_style(DIM))
        .highlight_style(Style::new().bg(Color::Indexed(236)))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, area, &mut app.pr_list);
}

fn draw_pr_preview(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = app.selected_pr() else {
        let msg = if app.prs.is_empty() {
            "  No PRs found in your sessions yet.\n  Run `cctrail sync` to pull PR titles/state from GitHub."
        } else {
            "  No matching PRs."
        };
        f.render_widget(Paragraph::new(Text::styled(msg, DIM)), area);
        return;
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(p.key(), BOLD.fg(Color::White)),
        Span::styled(
            p.state().map(|s| format!("  [{s}]")).unwrap_or_default(),
            DIM,
        ),
    ])];
    if let Some(t) = p.title() {
        lines.push(Line::from(Span::styled(t.to_string(), BOLD)));
    }
    if let Some(u) = pr_url(p) {
        lines.push(Line::from(Span::styled(u, DIM)));
    }
    if p.meta.is_none() {
        lines.push(Line::from(Span::styled(
            "(no GitHub metadata — `cctrail sync` fetches title/state)",
            DIM,
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        p.story(&app.sessions),
        Style::new().fg(Color::Yellow),
    )));
    lines.push(Line::raw(""));
    for (l, also) in p.grouped() {
        let also = if also.is_empty() {
            String::new()
        } else {
            format!(
                " (also {})",
                also.iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let (who, when, title) = match l.session {
            Some(i) => {
                let s = &app.sessions[i];
                (
                    s.short_id().to_string(),
                    local_time(l.ts.as_deref().or(s.started.as_deref())),
                    s.title(),
                )
            }
            None => (
                "cloud".into(),
                local_time(l.ts.as_deref()),
                l.cloud_id
                    .as_ref()
                    .map(|c| format!("https://claude.ai/code/{c}"))
                    .unwrap_or_default(),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} {:<9} ", l.role.icon(), l.role.verb()),
                Style::new()
                    .fg(role_color(l.role))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{who}  "), Style::new().fg(Color::Blue)),
            Span::styled(format!("{when}  "), DIM),
            Span::raw(truncate(&title, 50)),
        ]));
        lines.push(Line::from(Span::styled(
            format!("             {}{also}", l.evidence),
            DIM,
        )));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        pad(area),
    );
}

fn draw_task_list(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .task_shown
        .iter()
        .map(|&ti| {
            let t = &app.tasks[ti];
            let newest = &app.sessions[t.sessions[0]];
            let l1 = vec![
                Span::styled(format!("{:>4} ", age(newest.ended.as_deref())), DIM),
                Span::styled(truncate(&t.title, width.saturating_sub(6)), BOLD),
            ];
            let here = t
                .sessions
                .iter()
                .map(|&i| app.corpus.tier[i])
                .max()
                .unwrap_or(0);
            let mut l2 = vec![
                Span::styled(
                    if app.here_first && here > 0 {
                        "   ◉ "
                    } else {
                        "     "
                    },
                    Style::new().fg(Color::Green),
                ),
                Span::styled(t.project.clone(), Style::new().fg(Color::Blue)),
            ];
            if let Some(b) = &t.branch {
                l2.push(Span::styled(format!(" ⎇ {}", truncate(b, 26)), DIM));
            }
            l2.push(Span::styled(
                format!(" · {} sessions", t.sessions.len()),
                Style::new().fg(Color::Yellow),
            ));
            if t.cost_usd > 0.0 {
                l2.push(Span::styled(format!(" · ${:.2}", t.cost_usd), DIM));
            }
            if let Some(k) = t.tickets.first() {
                l2.push(Span::styled(format!(" ⚑{k}"), Style::new().fg(Color::Cyan)));
            }
            for &pi in t.prs.iter().take(3) {
                let p = &app.prs[pi];
                let role = p
                    .links
                    .iter()
                    .filter(|l| l.session.is_some_and(|i| t.sessions.contains(&i)))
                    .map(|l| l.role)
                    .min();
                if let Some(r) = role {
                    l2.push(Span::styled(
                        format!(" {}#{}", r.icon(), p.number),
                        Style::new().fg(role_color(r)),
                    ));
                }
            }
            ListItem::new(Text::from(vec![Line::from(l1), Line::from(l2)]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::new().borders(Borders::RIGHT).border_style(DIM))
        .highlight_style(Style::new().bg(Color::Indexed(236)))
        .highlight_symbol("▌")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, area, &mut app.task_list);
}

fn draw_task_preview(f: &mut Frame, app: &App, area: Rect) {
    let Some(t) = app.selected_task() else {
        let msg = if app.tasks.is_empty() {
            "  No tasks yet. Sessions are grouped into a task when they work on\n  the same branch or the same PR."
        } else {
            "  No matching tasks."
        };
        f.render_widget(Paragraph::new(Text::styled(msg, DIM)), area);
        return;
    };
    let mut lines = vec![
        Line::from(Span::styled(t.title.clone(), BOLD.fg(Color::White))),
        Line::from(vec![
            Span::styled(t.project.clone(), Style::new().fg(Color::Blue)),
            Span::styled(
                t.branch
                    .as_ref()
                    .map(|b| format!("  ⎇ {b}"))
                    .unwrap_or_default(),
                DIM,
            ),
            Span::styled(
                if t.cost_usd > 0.0 {
                    format!("  · ${:.2} total", t.cost_usd)
                } else {
                    String::new()
                },
                DIM,
            ),
        ]),
    ];
    for k in &t.tickets {
        let url = crate::config::ticket_url(k).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("⚑ {k}  "), Style::new().fg(Color::Cyan)),
            Span::styled(url, DIM),
        ]));
    }
    for &pi in &t.prs {
        let p = &app.prs[pi];
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", p.key()), BOLD),
            Span::styled(
                p.state().map(|s| format!("[{s}] ")).unwrap_or_default(),
                DIM,
            ),
            Span::styled(p.story(&app.sessions), Style::new().fg(Color::Yellow)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!("Sessions ({}), oldest first", t.sessions.len()),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    for &i in t.sessions.iter().rev() {
        let s = &app.sessions[i];
        let roles: Vec<Span> = t
            .prs
            .iter()
            .filter_map(|&pi| {
                let p = &app.prs[pi];
                let r = p
                    .links
                    .iter()
                    .filter(|l| l.session == Some(i))
                    .map(|l| l.role)
                    .min()?;
                r.is_significant().then(|| {
                    Span::styled(
                        format!(" {} {} #{}", r.icon(), r.as_str(), p.number),
                        Style::new().fg(role_color(r)),
                    )
                })
            })
            .collect();
        let mut l = vec![
            Span::styled(format!("{} ", s.short_id()), Style::new().fg(Color::Blue)),
            Span::styled(format!("{}  ", local_time(s.started.as_deref())), DIM),
            Span::raw(truncate(&s.title(), 50)),
        ];
        l.extend(roles);
        lines.push(Line::from(l));
        if let Some(d) = s.description().lines().find(|l| !l.starts_with("Asked:")) {
            lines.push(Line::from(Span::styled(
                format!("          {}", truncate(d, 80)),
                DIM,
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Enter lists these sessions; the newest is selected.",
        DIM,
    )));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        pad(area),
    );
}
