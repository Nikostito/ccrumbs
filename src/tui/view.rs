//! Screen layout ("Split" design): a short one-line list grouped by day on the
//! left, and a sectioned detail card on the right.

use super::*;

const HEAD: Style = Style::new().add_modifier(Modifier::BOLD);

fn head_style() -> Style {
    HEAD.fg(th::dim())
}

/// One line in a grouped list: a group heading or an item (index into the shown list).
enum Row {
    Head(String, String),
    Item(usize),
    Gap,
}

fn day_group(ts: Option<&str>) -> &'static str {
    let Some(t) = ts.and_then(crate::text::parse_ts) else {
        return "Older";
    };
    let local = t.with_timezone(&chrono::Local).date_naive();
    let today = chrono::Local::now().date_naive();
    match (today - local).num_days() {
        d if d <= 0 => "Today",
        1 => "Yesterday",
        2..=6 => "This week",
        7..=30 => "This month",
        _ => "Older",
    }
}

fn ago(ts: Option<&str>) -> String {
    let a = age(ts);
    match a.as_str() {
        "now" => "just now".into(),
        "?" => "some time ago".into(),
        _ => {
            let (n, u) = a.split_at(a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len()));
            let unit = match u {
                "m" => "minute",
                "h" => "hour",
                "d" => "day",
                "w" => "week",
                "mo" => "month",
                _ => "year",
            };
            format!("{n} {unit}{} ago", if n == "1" { "" } else { "s" })
        }
    }
}

/// Right-hand time label that matches the day groups: a clock time for today and
/// yesterday, a weekday this week, a date before that.
fn when_label(ts: Option<&str>) -> String {
    let Some(t) = ts.and_then(crate::text::parse_ts) else {
        return String::new();
    };
    let local = t.with_timezone(&chrono::Local);
    match day_group(ts) {
        "Today" | "Yesterday" => local.format("%H:%M").to_string(),
        "This week" => local.format("%a").to_string(),
        _ => local.format("%b %d").to_string(),
    }
}

/// Lines of `text` cut to `w` columns.
fn fit(s: &str, w: usize) -> String {
    truncate(s, w.max(2))
}

/// Draw a grouped one-line list. `item` renders (left text, left style, right text, marker).
fn grouped_list(
    f: &mut Frame,
    area: Rect,
    rows: &[Row],
    selected: Option<usize>,
    offset: &mut usize,
    mut item: impl FnMut(usize, bool) -> (Span<'static>, String, Style, String),
) {
    let h = area.height as usize;
    let w = area.width as usize;
    // Keep the selection visible, showing its heading when scrolling up.
    if let Some(sel) = selected
        && let Some(pos) = rows
            .iter()
            .position(|r| matches!(r, Row::Item(i) if *i == sel))
    {
        let want_top = if pos >= 1 && matches!(rows[pos - 1], Row::Head(..)) {
            pos - 1
        } else {
            pos
        };
        if want_top < *offset {
            *offset = want_top.saturating_sub(
                if want_top >= 1 && matches!(rows[want_top - 1], Row::Gap) {
                    1
                } else {
                    0
                },
            );
        } else if pos >= *offset + h {
            *offset = pos + 1 - h;
        }
    }
    *offset = (*offset).min(rows.len().saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for r in rows.iter().skip(*offset).take(h) {
        match r {
            Row::Gap => lines.push(Line::raw("")),
            Row::Head(t, suffix) => {
                // The subtitle gives way to the column edge.
                let room = w.saturating_sub(crate::text::width(t) + 6);
                let suffix = if suffix.is_empty() || room < 8 {
                    String::new()
                } else {
                    format!("  {}", fit(suffix, room))
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {}", t.to_uppercase()), head_style()),
                    Span::styled(suffix, Style::new().fg(th::faint())),
                ]))
            }
            Row::Item(i) => {
                let sel = selected == Some(*i);
                let (marker, text, style, right) = item(*i, sel);
                let bg = if sel {
                    Style::new().bg(th::sel())
                } else {
                    Style::new()
                };
                let room = w.saturating_sub(crate::text::width(&right) + 7);
                let text = fit(&text, room);
                let pad = w
                    .saturating_sub(4 + crate::text::width(&text) + crate::text::width(&right) + 2);
                lines.push(Line::from(vec![
                    Span::styled(if sel { "▌" } else { " " }, bg.fg(th::accent())),
                    Span::styled(" ", bg),
                    marker.patch_style(bg),
                    Span::styled(" ", bg),
                    Span::styled(
                        text,
                        bg.patch(if sel {
                            style.fg(th::bright()).add_modifier(Modifier::BOLD)
                        } else {
                            style
                        }),
                    ),
                    Span::styled(" ".repeat(pad), bg),
                    Span::styled(right, bg.fg(th::dim())),
                    Span::styled("  ", bg),
                ]));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

// ── Sessions ────────────────────────────────────────────────────────────────

pub(super) fn session_list(f: &mut Frame, app: &mut App, area: Rect) {
    let q = search::parse(&app.query);
    let chronological = q.text.is_empty() && q.prs.is_empty() && !app.deep;
    let mut rows: Vec<Row> = Vec::new();
    let mut cur = String::new();
    if !chronological {
        rows.push(Row::Head(
            format!(
                "{} result{}",
                app.shown.len(),
                if app.shown.len() == 1 { "" } else { "s" }
            ),
            String::new(),
        ));
    }
    for (k, &i) in app.shown.iter().enumerate() {
        let s = &app.sessions[i];
        if chronological {
            let tier = if app.here_first {
                app.corpus.tier[i]
            } else {
                0
            };
            let (g, suffix) = if s.pinned {
                ("Pinned".to_string(), String::new())
            } else if !s.needs.is_empty() {
                (
                    "Needs you".to_string(),
                    "its PR has news: CI, conflicts, reviews".to_string(),
                )
            } else if s.live.is_some() {
                (
                    "Open now".to_string(),
                    "running in another terminal".to_string(),
                )
            } else if tier > 0 {
                (
                    "Here".to_string(),
                    app.ctx.as_ref().map(|c| c.label()).unwrap_or_default(),
                )
            } else {
                (
                    day_group(s.ended.as_deref().or(s.started.as_deref())).to_string(),
                    String::new(),
                )
            };
            if g != cur {
                if !rows.is_empty() {
                    rows.push(Row::Gap);
                }
                rows.push(Row::Head(g.clone(), suffix));
                cur = g;
            }
        }
        rows.push(Row::Item(k));
    }
    if app.shown.is_empty() {
        let msg = if app.sessions.is_empty() {
            "  No sessions yet."
        } else {
            "  Nothing matches. Esc clears the search."
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, dim())),
            Rect {
                y: area.y + 1,
                ..area
            },
        );
        return;
    }
    let selected = app.list.selected();
    let mut offset = app.offsets[0];
    let (sessions, shown, corpus, here_first) =
        (&app.sessions, &app.shown, &app.corpus, app.here_first);
    grouped_list(
        f,
        Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        },
        &rows,
        selected,
        &mut offset,
        |k, _| {
            let i = shown[k];
            let s = &sessions[i];
            let tier = if here_first { corpus.tier[i] } else { 0 };
            let marker = if !s.needs.is_empty() {
                Span::styled("!", Style::new().fg(th::red()).add_modifier(Modifier::BOLD))
            } else if s.live.is_some() {
                Span::styled("►", Style::new().fg(th::green()))
            } else if s.pinned {
                Span::styled("↑", Style::new().fg(th::amber()))
            } else if tier == crate::context::TIER_BRANCH {
                Span::styled("●", Style::new().fg(th::green()))
            } else if tier == crate::context::TIER_PLACE {
                Span::styled("○", Style::new().fg(th::green()))
            } else {
                Span::raw(" ")
            };
            let style = if s.missing {
                dim()
            } else {
                Style::new().fg(th::fg())
            };
            let ts = s.ended.as_deref().or(s.started.as_deref());
            let grouped_apart = s.pinned || !s.needs.is_empty() || s.live.is_some();
            let right = if chronological && !grouped_apart && tier == 0 {
                when_label(ts)
            } else {
                age(ts)
            };
            (marker, s.title(), style, right)
        },
    );
    app.offsets[0] = offset;
}

fn bar(pct: u32) -> Vec<Span<'static>> {
    let filled = (pct.min(100) as usize + 5) / 10;
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(ctx_color(pct))),
        Span::styled("░".repeat(10 - filled), Style::new().fg(th::faint())),
        Span::styled(format!(" {pct}%"), Style::new().fg(ctx_color(pct))),
    ]
}

/// Two-column key/value grid row.
fn facts(
    lw: usize,
    l: (&str, Vec<Span<'static>>),
    r: Option<(&str, Vec<Span<'static>>)>,
) -> Line<'static> {
    let mut v = vec![Span::styled(format!("{:<9}", l.0), dim())];
    let lw_used: usize = 9 + l
        .1
        .iter()
        .map(|s| crate::text::width(&s.content))
        .sum::<usize>();
    v.extend(l.1);
    if let Some((k, vals)) = r {
        v.push(Span::raw(" ".repeat(lw.saturating_sub(lw_used).max(2))));
        v.push(Span::styled(format!("{k:<11}"), dim()));
        v.extend(vals);
    }
    Line::from(v)
}

fn heading(lines: &mut Vec<Line<'static>>, t: &str) {
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(t.to_uppercase(), head_style())));
}

fn txt(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(th::fg()))
}

fn card_area(area: Rect) -> Rect {
    Rect {
        x: area.x + 3,
        y: area.y + 1,
        width: area.width.saturating_sub(5),
        height: area.height.saturating_sub(1),
    }
}

/// Claude's open to-dos and the start of its last reply.
fn left_off(lines: &mut Vec<Line<'static>>, s: &SessionRow, w: usize) {
    let open = s.open_todos();
    let reply = s.reply_excerpt(3);
    if open.is_empty() && reply.is_empty() {
        return;
    }
    let head = if s.todos.is_empty() {
        "Where you left off".to_string()
    } else {
        format!(
            "Where you left off · {} of {} to-dos open",
            open.len(),
            s.todos.len()
        )
    };
    heading(lines, &head);
    for t in open.iter().take(6) {
        let (mark, color) = if t.status == "in_progress" {
            ("► ", th::amber())
        } else {
            ("○ ", th::dim())
        };
        lines.push(Line::from(vec![
            Span::styled(mark, Style::new().fg(color)),
            txt(fit(&t.text, w.saturating_sub(3))),
        ]));
    }
    if open.len() > 6 {
        lines.push(Line::from(Span::styled(
            format!("  +{} more", open.len() - 6),
            dim(),
        )));
    }
    if !reply.is_empty() {
        if !open.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled("Claude's last reply", dim())));
        for l in reply {
            lines.push(Line::from(Span::styled(
                fit(&l, w.saturating_sub(2)),
                Style::new().fg(th::fg()).add_modifier(Modifier::ITALIC),
            )));
        }
    }
}

pub(super) fn session_card(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.selected() else { return };
    let me = app.shown[app.list.selected().unwrap_or(0)];
    let a = card_area(area);
    let w = a.width as usize;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(s.title(), HEAD.fg(th::bright())))];
    let mut sub = s.project_name();
    if let Some(b) = &s.branch {
        sub.push_str(&format!(" · {b}"));
    }
    sub.push_str(&format!(
        " · {}",
        ago(s.ended.as_deref().or(s.started.as_deref()))
    ));
    let state = if s.missing {
        Span::styled(" · transcript deleted", Style::new().fg(th::red()))
    } else if let Some(l) = &s.live {
        Span::styled(
            format!(" · open now ({}) — Enter switches to it", l.tty),
            Style::new().fg(th::green()),
        )
    } else if s.cloud_id.is_some() || s.entrypoint.as_deref() == Some("remote") {
        Span::styled(" · claude.ai session", dim())
    } else {
        Span::styled(" · resumable", dim())
    };
    let (proj, rest) = sub.split_at(s.project_name().len());
    lines.push(Line::from(vec![
        Span::styled(proj.to_string(), Style::new().fg(th::project())),
        Span::styled(rest.to_string(), dim()),
        state,
    ]));
    lines.push(Line::raw(""));

    let half = (w / 2).clamp(30, 44);
    let prompts = format!(
        "{}{}",
        s.user_prompts,
        duration(s.started.as_deref(), s.ended.as_deref())
            .map(|d| format!(" · {d}"))
            .unwrap_or_default()
    );
    let ctx = s
        .context_pct()
        .map(bar)
        .unwrap_or_else(|| vec![Span::styled("—", dim())]);
    let cost = if s.cost_usd > 0.0 {
        format!("${:.2}", s.cost_usd)
    } else {
        "—".into()
    };
    let model = s
        .model
        .as_deref()
        .map(|m| m.trim_start_matches("claude-").to_string())
        .unwrap_or_else(|| "—".into());
    lines.push(facts(
        half,
        (
            "Project",
            vec![txt(fit(&home_short(&s.resume_dir()), half - 11))],
        ),
        Some(("Context", ctx)),
    ));
    lines.push(facts(
        half,
        (
            "Branch",
            vec![txt(fit(s.branch.as_deref().unwrap_or("—"), half - 11))],
        ),
        Some(("Cost", vec![txt(cost)])),
    ));
    lines.push(facts(
        half,
        ("Prompts", vec![txt(prompts)]),
        Some(("Model", vec![txt(model)])),
    ));
    if !s.tickets.is_empty() || s.compactions > 0 {
        let t = s.tickets.join(", ");
        let left = if t.is_empty() {
            ("", vec![])
        } else {
            (
                "Ticket",
                vec![Span::styled(
                    fit(&t, half - 11),
                    Style::new().fg(th::teal()),
                )],
            )
        };
        let right =
            (s.compactions > 0).then(|| ("Compacted", vec![txt(format!("{}×", s.compactions))]));
        lines.push(facts(half, left, right));
    }

    if !s.needs.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("NEEDS YOU", HEAD.fg(th::red()))));
        for n in &s.needs {
            lines.push(Line::from(vec![
                Span::styled("! ", Style::new().fg(th::red())),
                txt(n.clone()),
            ]));
        }
    }

    heading(&mut lines, "What happened");
    for l in s.description().lines() {
        lines.push(Line::from(txt(l.to_string())));
    }
    if let Some(n) = &s.note {
        lines.push(Line::from(vec![
            Span::styled("note  ", Style::new().fg(th::amber())),
            Span::styled(n.clone(), Style::new().fg(th::amber())),
        ]));
    }
    left_off(&mut lines, s, w);
    if let Some(pct) = s.context_pct().filter(|p| *p >= 80) {
        heading(&mut lines, "Next");
        lines.push(Line::from(txt(format!(
            "Context is {pct}% full: resume to wrap up, or start a fresh session in {} so you don't hit compaction.",
            home_short(&s.resume_dir())
        ))));
    }

    let prs = s.pr_summary();
    if !prs.is_empty() {
        heading(
            &mut lines,
            if prs.len() == 1 {
                "Pull request"
            } else {
                "Pull requests"
            },
        );
        for (l, _) in prs {
            let Some(e) = app
                .pr_by_key
                .get(&(l.repo.clone(), l.number))
                .map(|&i| &app.prs[i])
            else {
                continue;
            };
            let mut head = vec![Span::styled(e.key(), HEAD.fg(th::bright()))];
            if let Some(t) = e.title() {
                head.push(txt(format!(
                    "  {}",
                    fit(t, w.saturating_sub(e.key().len() + 12))
                )));
            }
            if let Some(st) = e.state() {
                let c = match st {
                    "open" => th::green(),
                    "merged" => th::purple(),
                    _ => th::red(),
                };
                head.push(Span::styled(format!("  {st}"), Style::new().fg(c)));
            }
            lines.push(Line::from(head));
            for (pl, _) in e
                .grouped()
                .into_iter()
                .filter(|(pl, _)| pl.role != Role::Mentioned)
            {
                let who: Vec<Span> = match pl.session {
                    Some(i) if i == me => vec![Span::styled("this session", dim())],
                    Some(i) => vec![
                        Span::styled(
                            format!("{}  ", app.sessions[i].short_id()),
                            Style::new().fg(th::accent()),
                        ),
                        txt(fit(&app.sessions[i].title(), w.saturating_sub(26))),
                    ],
                    None => vec![Span::styled("claude.ai session", dim())],
                };
                let mut row = vec![Span::styled(
                    format!("  {:<11}", pl.role.as_str()),
                    Style::new().fg(role_color(pl.role)),
                )];
                row.extend(who);
                lines.push(Line::from(row));
            }
        }
    }

    if let Some(t) = app.corpus.task_of[me].map(|t| &app.tasks[t]) {
        heading(&mut lines, "Same task");
        for &i in t.sessions.iter().filter(|&&i| i != me).take(5) {
            let o = &app.sessions[i];
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {}  ", o.short_id()),
                    Style::new().fg(th::accent()),
                ),
                txt(fit(&o.title(), w.saturating_sub(18))),
                Span::styled(format!("  {}", age(o.ended.as_deref())), dim()),
            ]));
        }
    }
    if !s.files_edited.is_empty() {
        heading(
            &mut lines,
            &format!("Files edited · {}", s.files_edited.len()),
        );
        let names: Vec<&str> = s.files_edited.iter().take(8).map(String::as_str).collect();
        let more = s.files_edited.len().saturating_sub(8);
        lines.push(Line::from(Span::styled(
            format!(
                "{}{}",
                names.join("  "),
                if more > 0 {
                    format!("  +{more} more")
                } else {
                    String::new()
                }
            ),
            dim(),
        )));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        a,
    );
}

// ── Tasks ───────────────────────────────────────────────────────────────────

pub(super) fn task_list(f: &mut Frame, app: &mut App, area: Rect) {
    if app.task_shown.is_empty() {
        let msg = if app.tasks.is_empty() {
            "  No tasks yet: sessions are grouped when they share a branch, PR or ticket."
        } else {
            "  No matching tasks."
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, dim())).wrap(Wrap { trim: false }),
            Rect {
                y: area.y + 1,
                ..area
            },
        );
        return;
    }
    let mut rows = Vec::new();
    let mut cur = String::new();
    for (k, &ti) in app.task_shown.iter().enumerate() {
        let t = &app.tasks[ti];
        let here = app.here_first && t.sessions.iter().any(|&i| app.corpus.tier[i] > 0);
        let g = if here {
            "Here".to_string()
        } else {
            day_group(app.sessions[t.sessions[0]].ended.as_deref()).to_string()
        };
        if g != cur {
            if !rows.is_empty() {
                rows.push(Row::Gap);
            }
            let suffix = if here {
                app.ctx.as_ref().map(|c| c.label()).unwrap_or_default()
            } else {
                String::new()
            };
            rows.push(Row::Head(g.clone(), suffix));
            cur = g;
        }
        rows.push(Row::Item(k));
    }
    let selected = app.task_list.selected();
    let mut offset = app.offsets[1];
    let (tasks, shown, sessions) = (&app.tasks, &app.task_shown, &app.sessions);
    grouped_list(
        f,
        Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        },
        &rows,
        selected,
        &mut offset,
        |k, _| {
            let t = &tasks[shown[k]];
            let right = format!(
                "{} · {}",
                t.sessions.len(),
                age(sessions[t.sessions[0]].ended.as_deref())
            );
            (
                Span::raw(" "),
                t.title.clone(),
                Style::new().fg(th::fg()),
                right,
            )
        },
    );
    app.offsets[1] = offset;
}

pub(super) fn task_card(f: &mut Frame, app: &App, area: Rect) {
    let Some(t) = app.selected_task() else { return };
    let a = card_area(area);
    let w = a.width as usize;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        t.title.clone(),
        HEAD.fg(th::bright()),
    ))];
    let mut sub = t.project.clone();
    if let Some(b) = &t.branch {
        sub.push_str(&format!(" · {b}"));
    }
    sub.push_str(&format!(" · {} sessions", t.sessions.len()));
    if t.cost_usd > 0.0 {
        sub.push_str(&format!(" · ${:.2}", t.cost_usd));
    }
    lines.push(Line::from(Span::styled(sub, dim())));
    for &pi in &t.prs {
        let p = &app.prs[pi];
        lines.push(Line::raw(""));
        let mut head = vec![Span::styled(p.key(), HEAD.fg(th::bright()))];
        if let Some(st) = p.state() {
            head.push(Span::styled(
                format!("  {st}"),
                Style::new().fg(if st == "open" {
                    th::green()
                } else {
                    th::purple()
                }),
            ));
        }
        lines.push(Line::from(head));
        lines.push(Line::from(Span::styled(
            p.story(&app.sessions),
            Style::new().fg(th::amber()),
        )));
    }
    for k in &t.tickets {
        lines.push(Line::from(vec![
            Span::styled(format!("{k}  "), Style::new().fg(th::teal())),
            Span::styled(
                crate::config::ticket_url(k).unwrap_or_else(|| "^k to set its link".into()),
                dim(),
            ),
        ]));
    }
    heading(&mut lines, "The story");
    let ordered: Vec<usize> = t.sessions.iter().rev().copied().collect();
    for (n, &i) in ordered.iter().enumerate() {
        let s = &app.sessions[i];
        lines.push(Line::from(vec![
            Span::styled(format!("{}  ", n + 1), HEAD.fg(th::accent())),
            txt(fit(&s.title(), w.saturating_sub(12))),
            Span::styled(format!("  {}", age(s.ended.as_deref())), dim()),
        ]));
        let mut detail: Vec<Span> = vec![Span::raw("   ")];
        for &pi in &t.prs {
            let p = &app.prs[pi];
            if let Some(r) = p
                .links
                .iter()
                .filter(|l| l.session == Some(i))
                .map(|l| l.role)
                .min()
                .filter(|r| r.is_significant())
            {
                detail.push(Span::styled(
                    format!("{} #{}  ", r.as_str(), p.number),
                    Style::new().fg(role_color(r)),
                ));
            }
        }
        if let Some(pct) = s.context_pct() {
            detail.push(Span::styled(
                format!("{pct}% context"),
                Style::new().fg(if pct >= 80 { th::red() } else { th::dim() }),
            ));
        }
        lines.push(Line::from(detail));
    }
    if let Some(&last) = ordered.last() {
        let s = &app.sessions[last];
        heading(&mut lines, "Next");
        let full = s.context_pct().is_some_and(|p| p >= 80);
        lines.push(Line::from(txt(format!(
            "Session {} is the latest{}. Enter lists the sessions; pick one to resume.",
            ordered.len(),
            if full {
                ", but its context is nearly full: consider a fresh session"
            } else {
                ""
            }
        ))));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        a,
    );
}

// ── PRs ─────────────────────────────────────────────────────────────────────

pub(super) fn pr_list(f: &mut Frame, app: &mut App, area: Rect) {
    if app.pr_shown.is_empty() {
        let msg = if app.prs.is_empty() {
            "  No PRs in your sessions yet. `ccrumbs sync` adds titles and PRs opened from claude.ai."
        } else {
            "  No matching PRs."
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, dim())).wrap(Wrap { trim: false }),
            Rect {
                y: area.y + 1,
                ..area
            },
        );
        return;
    }
    // `pr_shown` is already in group order (see `App::refilter`), so rows are drawn
    // in the same order the cursor moves through them.
    let mut rows = Vec::new();
    let mut cur = "";
    for k in 0..app.pr_shown.len() {
        let g = pr_group(&app.prs[app.pr_shown[k]]);
        if g != cur {
            if !rows.is_empty() {
                rows.push(Row::Gap);
            }
            rows.push(Row::Head(g.to_string(), String::new()));
            cur = g;
        }
        rows.push(Row::Item(k));
    }
    let selected = app.pr_list.selected();
    let mut offset = app.offsets[2];
    let (prs, shown) = (&app.prs, &app.pr_shown);
    grouped_list(
        f,
        Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        },
        &rows,
        selected,
        &mut offset,
        |k, _| {
            let p = &prs[shown[k]];
            let marker = if p.born_from_claude() {
                Span::styled("●", Style::new().fg(th::green()))
            } else {
                Span::raw(" ")
            };
            let text = match p.title() {
                Some(t) => format!("#{}  {t}", p.number),
                None => p.key(),
            };
            (
                marker,
                text,
                Style::new().fg(th::fg()),
                p.repo.rsplit('/').next().unwrap_or("").to_string(),
            )
        },
    );
    app.offsets[2] = offset;
}

pub(super) fn pr_card(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = app.selected_pr() else { return };
    let a = card_area(area);
    let w = a.width as usize;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        p.title().unwrap_or(&p.key()).to_string(),
        HEAD.fg(th::bright()),
    ))];
    let mut sub = p.key();
    if let Some(st) = p.state() {
        sub.push_str(&format!(" · {st}"));
    }
    if p.born_from_claude() {
        sub.push_str(" · born from a Claude session");
    }
    lines.push(Line::from(Span::styled(sub, dim())));
    if let Some(u) = super::pr_url(p) {
        lines.push(Line::from(Span::styled(u, Style::new().fg(th::faint()))));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        p.story(&app.sessions),
        Style::new().fg(th::amber()),
    )));
    heading(&mut lines, "Who did what");
    for (l, also) in p.grouped() {
        let (who, title) = match l.session {
            Some(i) => (
                app.sessions[i].short_id().to_string(),
                app.sessions[i].title(),
            ),
            None => (
                "cloud".to_string(),
                l.cloud_id
                    .as_ref()
                    .map(|c| format!("https://claude.ai/code/{c}"))
                    .unwrap_or_default(),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<11}", l.role.as_str()),
                Style::new().fg(role_color(l.role)),
            ),
            Span::styled(format!("{who}  "), Style::new().fg(th::accent())),
            txt(fit(&title, w.saturating_sub(24))),
        ]));
        let also = if also.is_empty() {
            String::new()
        } else {
            format!(
                "  (also {})",
                also.iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        lines.push(Line::from(Span::styled(
            format!(
                "           {}  {}{also}",
                local_time(l.ts.as_deref()),
                l.evidence
            ),
            dim(),
        )));
    }
    if p.meta.is_none() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "No title or state yet: `ccrumbs sync` fetches them from GitHub.",
            dim(),
        )));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.preview_scroll, 0)),
        a,
    );
}

// ── Chrome ──────────────────────────────────────────────────────────────────

pub(super) fn header(f: &mut Frame, app: &App, top: Rect, split_x: u16) {
    let tab = |t: Tab, label: &str| {
        if app.tab == t {
            Span::styled(
                format!(" {label} "),
                Style::new()
                    .fg(th::bright())
                    .bg(th::sel())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), dim())
        }
    };
    let left = vec![
        Span::raw(" "),
        tab(Tab::Sessions, "Sessions"),
        Span::raw(" "),
        tab(Tab::Tasks, "Tasks"),
        Span::raw(" "),
        tab(Tab::Prs, "PRs"),
    ];
    f.render_widget(Paragraph::new(Line::from(left)), top);
    let mut q = vec![
        Span::styled("› ", HEAD.fg(th::accent())),
        Span::styled(app.query.clone(), Style::new().fg(th::bright())),
    ];
    q.push(Span::styled("█", Style::new().fg(th::accent())));
    if app.query.is_empty() {
        q.push(Span::styled(
            " search   ? for filters & keys",
            Style::new().fg(th::faint()),
        ));
    }
    // Search sits over the detail pane when there's room, else right after the tabs.
    let qx = if top.width >= 80 {
        split_x.max(30) + 2
    } else {
        26.min(top.width)
    };
    let q_used = qx as usize
        + 3
        + crate::text::width(&app.query)
        + if app.query.is_empty() { 31 } else { 0 };
    f.render_widget(
        Paragraph::new(Line::from(q)),
        Rect {
            x: top.x + qx,
            width: top.width.saturating_sub(qx),
            ..top
        },
    );
    let mut right: Vec<Span> = Vec::new();
    if app.deep {
        right.push(Span::styled("full-text  ", Style::new().fg(th::amber())));
    }
    if app.hide_quick {
        right.push(Span::styled("quick hidden  ", Style::new().fg(th::amber())));
    }
    if let (Some(c), true) = (&app.ctx, app.here_first) {
        right.push(Span::styled(
            format!("● {} ", c.label()),
            Style::new().fg(th::green()),
        ));
    }
    // Only when it doesn't cover the query.
    let right_w: usize = right.iter().map(|s| crate::text::width(&s.content)).sum();
    if q_used + right_w < top.width as usize {
        f.render_widget(
            Paragraph::new(Line::from(right)).alignment(ratatui::layout::Alignment::Right),
            top,
        );
    }
}

/// A bordered overlay. When the items don't fit the height, they're laid out in
/// two columns instead of being cut off.
pub(super) fn popup(
    f: &mut Frame,
    area: Rect,
    title: &str,
    head: Vec<Line<'static>>,
    items: Vec<Line<'static>>,
    foot: Vec<Line<'static>>,
    col_w: u16,
) {
    let mut head = head;
    let mut foot = foot;
    let need = (head.len() + items.len() + foot.len()) as u16 + 2;
    // Two columns when it's too tall; narrower columns on narrow screens.
    let col_w = if area.width >= col_w * 2 + 4 {
        col_w
    } else {
        col_w.min(area.width.saturating_sub(4) / 2).max(24)
    };
    let two = need > area.height && area.width >= col_w * 2 + 4 && items.len() > 1;
    let rows_needed = if two {
        items.len().div_ceil(2)
    } else {
        items.len()
    };
    // Still too tall: drop the header and footer lines rather than items.
    if (head.len() + rows_needed + foot.len()) as u16 + 2 > area.height {
        head.retain(|l| !l.spans.is_empty() && l.width() > 0);
        head.truncate(1);
        foot.clear();
        if (head.len() + rows_needed) as u16 + 2 > area.height {
            head.clear();
        }
    }
    let rows = if two {
        items.len().div_ceil(2)
    } else {
        items.len()
    };
    let h = ((head.len() + rows + foot.len()) as u16 + 2).min(area.height);
    let w = (if two { col_w * 2 + 3 } else { col_w + 2 }).min(area.width);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(th::accent()))
        .style(Style::new().bg(th::panel()));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let hh = head.len() as u16;
    f.render_widget(
        Paragraph::new(head),
        Rect {
            height: hh.min(inner.height),
            ..inner
        },
    );
    let body = Rect {
        y: inner.y + hh,
        height: inner.height.saturating_sub(hh + foot.len() as u16),
        ..inner
    };
    if two {
        let (a, b) = items.split_at(rows);
        let half = body.width / 2;
        f.render_widget(
            Paragraph::new(a.to_vec()),
            Rect {
                width: half,
                ..body
            },
        );
        f.render_widget(
            Paragraph::new(b.to_vec()),
            Rect {
                x: body.x + half + 1,
                width: body.width.saturating_sub(half + 1),
                ..body
            },
        );
    } else {
        f.render_widget(Paragraph::new(items), body);
    }
    let fy = body.y + body.height;
    if fy < inner.y + inner.height {
        f.render_widget(
            Paragraph::new(foot),
            Rect {
                y: fy,
                height: inner.y + inner.height - fy,
                ..inner
            },
        );
    }
}

pub(super) fn footer(f: &mut Frame, app: &App, bottom: Rect) {
    let line = if let Some((kind, text)) = &app.input {
        let (label, hint) = match kind {
            InputKind::Note { .. } => (
                " Note: ".to_string(),
                "   enter save · esc cancel · empty clears",
            ),
            InputKind::TicketLink { prefix, .. } => (
                format!(" Link for {prefix} tickets (Jira/Linear address or any ticket URL): "),
                "   enter save & open · esc cancel",
            ),
        };
        Line::from(vec![
            Span::styled(label, HEAD.fg(th::amber())),
            Span::raw(text.clone()),
            Span::styled("█", Style::new().fg(th::amber())),
            Span::styled(hint, dim()),
        ])
    } else if !app.status.is_empty() {
        Line::from(Span::styled(
            format!(" {}", app.status),
            Style::new().fg(th::amber()),
        ))
    } else {
        let mut v = vec![Span::raw(" ")];
        let mut k = |key: &'static str, what: &'static str| {
            v.push(Span::styled(key, Style::new().fg(th::accent())));
            v.push(Span::styled(format!(" {what}    "), dim()));
        };
        match app.tab {
            Tab::Sessions => k("enter", "resume"),
            _ => k("enter", "its sessions"),
        }
        k("^v", "read");
        k("^b", "PR");
        k("^e", "editor");
        k("^a", "actions");
        k("tab", "views");
        k("?", "all keys");
        Line::from(v)
    };
    f.render_widget(Paragraph::new(line), bottom);
}

pub(super) fn help(f: &mut Frame, area: Rect) {
    let k = |key: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {key:<17}"), Style::new().fg(th::accent())),
            Span::styled(what.to_string(), Style::new().fg(th::fg())),
        ])
    };
    let h = |t: &str| {
        Line::from(Span::styled(
            format!("  {}", t.to_uppercase()),
            head_style(),
        ))
    };
    let items = vec![
        h("Keys  (Alt works like Ctrl)"),
        k("enter", "resume (in Tasks / PRs: list its sessions)"),
        k("tab  shift-tab", "Sessions · Tasks · PRs"),
        k("^v", "read the conversation"),
        k("^b  ^k", "open its PR · its ticket"),
        k("^e", "open in your editor (^a i: change editor)"),
        k("^o  ^y", "resume in a new tab · copy the command"),
        k("^s", "pin / unpin"),
        k("^a  F2", "all actions (note, hide, export, describe…)"),
        k("^g  ^t  ^f", "here first · hide quick · full-text"),
        k("← →", "scroll the details"),
        k("esc", "clear the search, or quit"),
        Line::raw(""),
        h("Search filters"),
        k("#88  repo#88", "sessions linked to a PR"),
        k("ABC-123", "sessions linked to a ticket"),
        k("p:  b:  f:", "project · branch · a file it edited"),
        k("@today  @3d", "@yesterday · @week · @month · @2w"),
        k("ctx:>80", "context fill"),
        k("has:pr has:note", "is:pinned · is:hidden"),
        k("is:open is:needs", "open right now · its PR needs you"),
        k(
            "on:tuesday",
            "active that day (on:yesterday, on:2026-11-17)",
        ),
    ];
    popup(
        f,
        area,
        "ccrumbs",
        vec![Line::raw("")],
        items,
        vec![
            Line::raw(""),
            Line::from(Span::styled("  any key closes", dim())),
        ],
        66,
    );
}
