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
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
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
    let mut term = ratatui::init();
    let res = event_loop(&mut term, &mut app, db, cfg);
    ratatui::restore();
    res
}

fn event_loop(
    term: &mut DefaultTerminal,
    app: &mut App,
    db: &mut Db,
    cfg: &Config,
) -> Result<Outcome> {
    loop {
        term.draw(|f| draw(f, app))?;
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(out) = on_key(app, k, db, cfg)? {
            return Ok(out);
        }
    }
}

fn on_key(app: &mut App, k: KeyEvent, db: &mut Db, cfg: &Config) -> Result<Option<Outcome>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    app.status.clear();
    match (k.code, ctrl) {
        (KeyCode::Char('c'), true) => return Ok(Some(Outcome::Quit)),
        (KeyCode::Esc, _) => {
            if app.query.is_empty() {
                return Ok(Some(Outcome::Quit));
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
                        return Ok(Some(Outcome::Resume(Box::new(s.clone()))));
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
        (KeyCode::Up, _) | (KeyCode::Char('p'), true) | (KeyCode::Char('k'), true) => {
            app.move_by(-1)
        }
        (KeyCode::Down, _) | (KeyCode::Char('n'), true) | (KeyCode::Char('j'), true) => {
            app.move_by(1)
        }
        (KeyCode::PageUp, _) => app.move_by(-10),
        (KeyCode::PageDown, _) => app.move_by(10),
        (KeyCode::Home, _) => app.move_by(-100_000),
        (KeyCode::End, _) => app.move_by(100_000),
        (KeyCode::Right, _) | (KeyCode::Char('l'), true) => {
            app.preview_scroll = app.preview_scroll.saturating_add(3)
        }
        (KeyCode::Left, _) | (KeyCode::Char('h'), true) => {
            app.preview_scroll = app.preview_scroll.saturating_sub(3)
        }
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
        (KeyCode::Char('y'), true) => {
            if let Some(s) = app.selected() {
                let cmd = launch::resume_command(s);
                launch::copy(&cmd);
                app.status = format!("Copied: {cmd}");
            }
        }
        (KeyCode::Char('o'), true) => {
            if let Some(s) = app.selected() {
                app.status = match launch::open_tab(s) {
                    Ok(m) => m,
                    Err(e) => format!("{e} — use ctrl-y to copy the command"),
                };
            }
        }
        (KeyCode::Char('b'), true) => {
            let url = match app.tab {
                Tab::Prs => app.selected_pr().and_then(pr_url),
                Tab::Tasks => app
                    .selected_task()
                    .and_then(|t| t.prs.first())
                    .and_then(|&pi| pr_url(&app.prs[pi])),
                Tab::Sessions => app.selected().and_then(|s| {
                    s.cloud_id
                        .as_ref()
                        .map(|c| format!("https://claude.ai/code/{c}"))
                }),
            };
            app.status = match url {
                Some(u) => match launch::open_url(&u) {
                    Ok(()) => format!("Opened {u}"),
                    Err(e) => format!("{e}"),
                },
                None => "Nothing to open in the browser".into(),
            };
        }
        (KeyCode::Char('d'), true) => {
            if let Some(s) = app.selected().cloned() {
                app.status = match crate::describe::describe_one(db, &s, &cfg.describe_model) {
                    Ok((t, _)) => {
                        let sel = app.list.selected();
                        app.reload(db)?;
                        app.list.select(sel);
                        format!("Described: {t}")
                    }
                    Err(e) => format!("describe failed: {e}"),
                };
            }
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
    Ok(None)
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

    let help = if !app.status.is_empty() {
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
            Tab::Sessions => {
                v.extend(k("enter", "resume"));
                v.extend(k("^o", "new tab"));
                v.extend(k("^y", "copy"));
                v.extend(k("^d", "describe"));
            }
            Tab::Tasks | Tab::Prs => v.extend(k("enter", "show its sessions")),
        }
        v.extend(k("tab", "view"));
        v.extend(k("^g", "here/all"));
        v.extend(k("^t", "hide quick"));
        v.extend(k("^b", "web"));
        v.extend(k("^f", "full-text"));
        v.extend(k("←→", "scroll"));
        v.extend(k("esc", "quit"));
        Line::from(v)
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
        ]),
    ];
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
