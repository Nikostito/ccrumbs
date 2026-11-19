//! The picker: type to find a session, Enter to jump back into it.

use crate::config::Config;
use crate::context::Context;
use crate::db::Db;
use crate::launch;
use crate::model::{Role, SessionRow};
use crate::prview::{self, PrEntry};
use crate::search::{self, Corpus, Opts, Ranker};
use crate::tasks::{self, Task};
use crate::text::{age, duration, home_short, local_time, truncate};
use crate::theme as th;

mod view;
use crate::transcript::{self, Entry, Kind};
use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, ListState, Paragraph, Wrap};
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

/// Text being typed into the status bar.
#[derive(Clone)]
enum InputKind {
    Note {
        id: String,
    },
    /// Ask for the tracker address the first time a prefix's ticket is opened.
    TicketLink {
        prefix: String,
        key: String,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    View,
    OpenPr,
    OpenTicket,
    OpenCloud,
    Editor,
    PickEditor,
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
    ('i', Action::PickEditor, "choose which editor to use"),
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
                        Style::new().fg(th::accent()).add_modifier(Modifier::BOLD),
                        format!("► You · {when}"),
                    ));
                    rows.extend(
                        wrap(&e.text, w)
                            .into_iter()
                            .map(|l| (Style::new().fg(th::bright()), l)),
                    );
                }
                Kind::Assistant => {
                    rows.push((Style::new(), String::new()));
                    rows.push((
                        Style::new().fg(th::green()).add_modifier(Modifier::BOLD),
                        "◄ Claude".to_string(),
                    ));
                    rows.extend(wrap(&e.text, w).into_iter().map(|l| (Style::new(), l)));
                }
                Kind::Tool => rows.push((
                    Style::new().fg(th::amber()),
                    format!("  › {}", truncate(&e.text, w.saturating_sub(4))),
                )),
                Kind::Output | Kind::Error if self.outputs => {
                    let st = if e.kind == Kind::Error {
                        Style::new().fg(th::red())
                    } else {
                        dim()
                    };
                    for l in e.text.lines().take(12) {
                        rows.push((st, format!("    │ {}", truncate(l, w.saturating_sub(6)))));
                    }
                }
                Kind::Meta => rows.push((Style::new().fg(th::purple()), e.text.clone())),
                _ => {}
            }
        }
        if rows.is_empty() {
            rows.push((dim(), "(this transcript has no readable messages)".into()));
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
    input: Option<(InputKind, String)>,
    /// First visible row of each list (Sessions, Tasks, PRs).
    offsets: [usize; 3],
    /// The `?` overlay.
    help: bool,
    /// Editor chooser: the editors found, and which one is highlighted.
    editor_pick: Option<(Vec<launch::Editor>, usize)>,
}

pub fn role_color(r: Role) -> Color {
    match r {
        Role::Created => th::green(),
        Role::Reviewed => th::purple(),
        Role::Merged => th::blue(),
        Role::Commented => th::amber(),
        Role::WorkedOn => th::teal(),
        Role::Viewed | Role::Mentioned => th::dim(),
    }
}

pub fn ctx_color(pct: u32) -> Color {
    match pct {
        0..50 => th::green(),
        50..80 => th::amber(),
        _ => th::red(),
    }
}

fn dim() -> Style {
    Style::new().fg(th::dim())
}
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
            input: None,
            offsets: [0; 3],
            help: false,
            editor_pick: None,
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
    th::use_terminal_colors(cfg.theme == "terminal");
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
                    let prefix = crate::tickets::prefix(&key).to_string();
                    app.input = Some((InputKind::TicketLink { prefix, key }, String::new()));
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
            if app.target().is_some() {
                match launch::chosen_editor(crate::config::Config::load().editor.as_deref()) {
                    Some(ed) => return Ok(open_in_editor(app, &ed)),
                    None => open_editor_picker(app),
                }
            }
        }
        Action::PickEditor => open_editor_picker(app),
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
                app.input = Some((
                    InputKind::Note { id: s.id.clone() },
                    s.note.clone().unwrap_or_default(),
                ));
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

fn open_editor_picker(app: &mut App) {
    let found = launch::detect_editors();
    if found.is_empty() {
        app.status = "No editors found. Set one with: cctrail editor set <command>".into();
    } else {
        let current = crate::config::Config::load().editor;
        let at = current
            .and_then(|c| found.iter().position(|e| e.command == c))
            .unwrap_or(0);
        app.editor_pick = Some((found, at));
    }
}

/// Open the target session's project with `editor`.
fn open_in_editor(app: &mut App, editor: &str) -> Step {
    let Some(s) = app.target() else {
        return Step::Stay;
    };
    let dir = s.resume_dir();
    match launch::editor_command(editor, &dir) {
        Err(e) => app.status = format!("{e}"),
        Ok((mut cmd, true)) => {
            app.status = match cmd
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(_) => format!(
                    "Opened {} with {editor}  (^a i to choose another editor)",
                    home_short(&dir)
                ),
                Err(e) => format!("Could not start {editor}: {e}"),
            };
        }
        Ok((cmd, false)) => return Step::Suspend(cmd),
    }
    Step::Stay
}

fn editor_pick_key(app: &mut App, k: KeyEvent) -> Result<Step> {
    let Some((list, at)) = app.editor_pick.as_mut() else {
        return Ok(Step::Stay);
    };
    let chosen = match k.code {
        KeyCode::Esc => {
            app.editor_pick = None;
            return Ok(Step::Stay);
        }
        KeyCode::Up => {
            *at = at.saturating_sub(1);
            None
        }
        KeyCode::Down => {
            *at = (*at + 1).min(list.len() - 1);
            None
        }
        KeyCode::Enter => Some(*at),
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let i = c as usize - '1' as usize;
            (i < list.len()).then_some(i)
        }
        _ => None,
    };
    if let Some(i) = chosen {
        let ed = list[i].clone();
        app.editor_pick = None;
        crate::config::set_editor(Some(&ed.command))?;
        let step = open_in_editor(app, &ed.command);
        if matches!(step, Step::Stay) && !app.status.starts_with("Could not") {
            app.status = format!(
                "{} is now your editor (^a i to change).  {}",
                ed.label, app.status
            );
        }
        return Ok(step);
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

fn input_key(app: &mut App, k: KeyEvent, db: &mut Db) -> Result<Step> {
    let Some((kind, text)) = app.input.as_mut() else {
        return Ok(Step::Stay);
    };
    match k.code {
        KeyCode::Esc => app.input = None,
        KeyCode::Enter => {
            let (kind, text) = (kind.clone(), text.trim().to_string());
            app.input = None;
            match kind {
                InputKind::Note { id } => {
                    db.set_note(&id, Some(&text))?;
                    app.reload_keep(db)?;
                    app.status = if text.is_empty() {
                        "Note cleared".into()
                    } else {
                        "Note saved (it's searchable too)".into()
                    };
                }
                InputKind::TicketLink { prefix, key } => {
                    if text.is_empty() {
                        app.status = "No link saved".into();
                        return Ok(Step::Stay);
                    }
                    match crate::config::ticket_template(&text).and_then(|tpl| {
                        crate::config::set_ticket_url(&prefix, &tpl)?;
                        Ok(tpl)
                    }) {
                        Err(e) => app.status = format!("Could not save the link: {e}"),
                        Ok(tpl) => {
                            app.reload_keep(db)?;
                            let url = tpl.replace("{key}", &key);
                            let _ = launch::open_url(&url);
                            app.status = format!(
                                "Saved {prefix} links as {tpl} and opened {key}. Change it with `cctrail tickets set`."
                            );
                        }
                    }
                }
            }
        }
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
        KeyCode::Char(c) => text.push(c),
        _ => {}
    }
    Ok(Step::Stay)
}

fn on_key(app: &mut App, k: KeyEvent, db: &mut Db, cfg: &Config) -> Result<Step> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    app.status.clear();
    if app.help {
        app.help = false;
        return Ok(Step::Stay);
    }
    if app.viewer.is_some() {
        return Ok(viewer_key(app, k));
    }
    if app.input.is_some() {
        return input_key(app, k, db);
    }
    if app.editor_pick.is_some() {
        return editor_pick_key(app, k);
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
        (KeyCode::Char('?'), false) if app.query.is_empty() => app.help = true,
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
    // Our own background, so the look doesn't depend on the terminal's palette.
    f.render_widget(
        Block::new().style(Style::new().bg(th::bg()).fg(th::fg())),
        f.area(),
    );
    f.render_widget(Block::new().style(Style::new().bg(th::bar())), top);
    f.render_widget(Block::new().style(Style::new().bg(th::bar())), bottom);

    let wide = main.width >= 100;
    let lw = if wide {
        (main.width * 40 / 100).clamp(38, 64)
    } else {
        main.width
    };
    view::header(f, app, top, if wide { lw } else { 0 });
    if app.viewer.is_some() {
        draw_viewer(f, app, main, bottom);
        return;
    }
    let (left, right) = if wide {
        let [l, d, r] = Layout::horizontal([
            Constraint::Length(lw),
            Constraint::Length(1),
            Constraint::Min(10),
        ])
        .areas(main);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "│",
                    Style::new().fg(th::faint())
                ));
                d.height as usize
            ]),
            d,
        );
        (l, r)
    } else {
        let [l, r] =
            Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(main);
        (l, r)
    };
    f.render_widget(Block::new().style(Style::new().bg(th::panel())), right);
    match app.tab {
        Tab::Sessions => {
            view::session_list(f, app, left);
            view::session_card(f, app, right);
        }
        Tab::Tasks => {
            view::task_list(f, app, left);
            view::task_card(f, app, right);
        }
        Tab::Prs => {
            view::pr_list(f, app, left);
            view::pr_card(f, app, right);
        }
    }
    if app.menu {
        draw_menu(f, app, main);
    }
    if app.editor_pick.is_some() {
        draw_editor_pick(f, app, main);
    }
    if app.help {
        view::help(f, main);
    }
    view::footer(f, app, bottom);
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
                    .fg(th::on_accent())
                    .bg(th::accent())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {what}")),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("any other key closes", dim())));
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
                .border_style(Style::new().fg(th::accent())),
        ),
        r,
    );
}

fn draw_editor_pick(f: &mut Frame, app: &App, area: Rect) {
    let Some((list, at)) = &app.editor_pick else {
        return;
    };
    let mut lines = vec![
        Line::from(Span::styled("Open projects with…", BOLD)),
        Line::from(Span::styled("cctrail remembers your choice", dim())),
        Line::raw(""),
    ];
    for (i, e) in list.iter().enumerate() {
        let sel = i == *at;
        let st = if sel {
            Style::new().bg(th::sel()).fg(th::bright())
        } else {
            Style::new()
        };
        let num = if i < 9 {
            format!(" {} ", i + 1)
        } else {
            "   ".into()
        };
        lines.push(Line::from(vec![
            Span::styled(num, Style::new().fg(th::accent())),
            Span::styled(
                format!(" {:<16}", e.label),
                st.add_modifier(if sel {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                format!(
                    " {}{}",
                    truncate(&e.command, 24),
                    if e.gui { "" } else { " (terminal)" }
                ),
                if sel { st.fg(th::dim()) } else { dim() },
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "↑↓ / 1-9 choose · enter open · esc cancel",
        dim(),
    )));
    let h = (lines.len() as u16 + 2).min(area.height);
    let w = 56.min(area.width);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .style(Style::new().bg(th::panel()))
            .block(
                Block::bordered()
                    .title(" Editor ")
                    .border_style(Style::new().fg(th::accent())),
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
                Line::from(Span::styled(t.clone(), st.bg(th::find())))
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
                .border_style(Style::new().fg(th::accent())),
        ),
        main,
    );
    let help = if let Some(input) = &v.input {
        Line::from(vec![
            Span::styled("/", Style::new().fg(th::amber())),
            Span::raw(input.clone()),
            Span::styled("█", Style::new().fg(th::amber())),
        ])
    } else if !app.status.is_empty() {
        Line::from(Span::styled(
            app.status.clone(),
            Style::new().fg(th::amber()),
        ))
    } else {
        let k = |key: &'static str, what: &'static str| {
            vec![
                Span::styled(key, Style::new().fg(th::accent())),
                Span::styled(format!(" {what}  "), dim()),
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
