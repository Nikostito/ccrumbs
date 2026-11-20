//! `ccrumbs log`: what you worked on, day by day (for stand-ups and timesheets).

use crate::model::{Role, SessionRow};
use chrono::{Datelike, Duration, Local, NaiveDate, Weekday};

const WEEKDAYS: [(&str, Weekday); 7] = [
    ("mon", Weekday::Mon),
    ("tue", Weekday::Tue),
    ("wed", Weekday::Wed),
    ("thu", Weekday::Thu),
    ("fri", Weekday::Fri),
    ("sat", Weekday::Sat),
    ("sun", Weekday::Sun),
];

/// A day as people say it, relative to `today`:
/// `today`, `yesterday`, `tuesday`/`tue` (the latest one, today included),
/// `last tuesday` (the one before that), `3d` (three days ago), `2026-11-17`, `11-17`.
pub fn parse_day(spec: &str, today: NaiveDate) -> Option<NaiveDate> {
    let s = spec.trim().to_lowercase();
    let s = s.as_str();
    match s {
        "today" => return Some(today),
        "yesterday" => return today.pred_opt(),
        _ => {}
    }
    let (last, word) = match s
        .strip_prefix("last")
        .map(|r| r.trim_start_matches([' ', '-']))
    {
        Some(w) if !w.is_empty() => (true, w),
        _ => (false, s),
    };
    if word.len() >= 3
        && let Some((_, wd)) = WEEKDAYS
            .iter()
            .find(|(k, _)| word.starts_with(k) && weekday_name(k).starts_with(word))
    {
        let back = (7 + today.weekday().num_days_from_monday() as i64
            - wd.num_days_from_monday() as i64)
            % 7;
        let back = if last { back + 7 } else { back };
        return today.checked_sub_signed(Duration::days(back));
    }
    if let Some(n) = s.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        return today.checked_sub_signed(Duration::try_days(n)?);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    // `11-17`: this year, or last year if that's still ahead.
    let d = NaiveDate::parse_from_str(&format!("{}-{s}", today.year()), "%Y-%m-%d").ok()?;
    Some(if d > today {
        d.with_year(today.year() - 1)?
    } else {
        d
    })
}

fn weekday_name(abbr: &str) -> &'static str {
    match abbr {
        "mon" => "monday",
        "tue" => "tuesday",
        "wed" => "wednesday",
        "thu" => "thursday",
        "fri" => "friday",
        "sat" => "saturday",
        _ => "sunday",
    }
}

/// Start of a range: a day (see [`parse_day`]), or `week` / `month` / `2w`.
pub fn parse_since(spec: &str, today: NaiveDate) -> Option<NaiveDate> {
    let s = spec.trim().to_lowercase();
    match s.as_str() {
        "week" => return today.checked_sub_signed(Duration::days(6)),
        "month" => return today.checked_sub_signed(Duration::days(29)),
        _ => {}
    }
    if let Some(n) = s.strip_suffix('w').and_then(|n| n.parse::<i64>().ok()) {
        return today.checked_sub_signed(Duration::try_weeks(n)?);
    }
    parse_day(&s, today)
}

fn local_day(ts: Option<&str>) -> Option<NaiveDate> {
    ts.and_then(crate::text::parse_ts)
        .map(|t| t.with_timezone(&Local).date_naive())
}

/// Days a session was active on (older index rows without day data: its first and
/// last day).
pub fn active_days(s: &SessionRow) -> Vec<NaiveDate> {
    let mut d: Vec<NaiveDate> = s
        .days
        .iter()
        .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .collect();
    if d.is_empty() {
        d.extend(local_day(s.started.as_deref()));
        d.extend(local_day(s.ended.as_deref()));
        d.dedup();
    }
    d
}

pub fn started_on(s: &SessionRow, day: NaiveDate) -> bool {
    local_day(s.started.as_deref()) == Some(day)
}

/// One day of the log: sessions started that day, then ones continued from before.
pub struct Day {
    pub date: NaiveDate,
    pub new: Vec<usize>,
    pub continued: Vec<usize>,
}

/// Days from `from` to `to` (newest first) with the sessions active on each.
/// `keep` decides which sessions count at all; `created_only` drops continued ones.
pub fn build(
    sessions: &[SessionRow],
    from: NaiveDate,
    to: NaiveDate,
    created_only: bool,
    keep: impl Fn(&SessionRow) -> bool,
) -> Vec<Day> {
    let mut days: Vec<Day> = Vec::new();
    let mut d = to;
    while d >= from {
        days.push(Day {
            date: d,
            new: vec![],
            continued: vec![],
        });
        let Some(p) = d.pred_opt() else { break };
        d = p;
    }
    // Oldest session first within a day, so the day reads in order.
    for (i, s) in sessions.iter().enumerate().rev() {
        if !keep(s) {
            continue;
        }
        for date in active_days(s) {
            let Some(day) = days.iter_mut().find(|x| x.date == date) else {
                continue;
            };
            if started_on(s, date) {
                day.new.push(i);
            } else if !created_only {
                day.continued.push(i);
            }
        }
    }
    days.retain(|d| !d.new.is_empty() || !d.continued.is_empty());
    days
}

/// PRs a session did something concrete with: "created acme/web#12".
pub fn pr_work(s: &SessionRow) -> Vec<(Role, String)> {
    s.pr_summary()
        .into_iter()
        .filter(|(l, _)| l.role.is_significant())
        .map(|(l, _)| {
            let r = if l.repo.is_empty() {
                format!("#{}", l.number)
            } else {
                format!("{}#{}", l.repo, l.number)
            };
            (l.role, r)
        })
        .collect()
}

/// "4 sessions · created 2 PRs · reviewed 1 · $3.10".
pub fn day_summary(sessions: &[SessionRow], d: &Day) -> String {
    let all: Vec<&SessionRow> = d
        .new
        .iter()
        .chain(&d.continued)
        .map(|&i| &sessions[i])
        .collect();
    let mut parts = vec![format!(
        "{} session{}",
        all.len(),
        if all.len() == 1 { "" } else { "s" }
    )];
    for (role, label) in [
        (Role::Created, "created"),
        (Role::Reviewed, "reviewed"),
        (Role::Merged, "merged"),
    ] {
        let mut prs: Vec<String> = all
            .iter()
            .flat_map(|s| pr_work(s))
            .filter(|(r, _)| *r == role)
            .map(|(_, p)| p)
            .collect();
        prs.sort();
        prs.dedup();
        if !prs.is_empty() {
            parts.push(format!(
                "{label} {} PR{}",
                prs.len(),
                if prs.len() == 1 { "" } else { "s" }
            ));
        }
    }
    // Cost is per session, not per day: only count sessions that lived within this day.
    let cost: f64 = d
        .new
        .iter()
        .map(|&i| &sessions[i])
        .filter(|s| active_days(s).len() <= 1)
        .map(|s| s.cost_usd)
        .sum();
    if cost > 0.0 {
        parts.push(format!("${cost:.2}"));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_people_say() {
        // A Thursday.
        let t = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let d = |s| parse_day(s, t).map(|d| d.to_string());
        assert_eq!(d("today").as_deref(), Some("2026-11-19"));
        assert_eq!(d("yesterday").as_deref(), Some("2026-11-18"));
        assert_eq!(d("tuesday").as_deref(), Some("2026-11-17"));
        assert_eq!(d("Tue").as_deref(), Some("2026-11-17"));
        assert_eq!(d("thursday").as_deref(), Some("2026-11-19"));
        assert_eq!(d("last tuesday").as_deref(), Some("2026-11-10"));
        assert_eq!(d("friday").as_deref(), Some("2026-11-13"));
        assert_eq!(d("3d").as_deref(), Some("2026-11-16"));
        assert_eq!(d("2026-10-01").as_deref(), Some("2026-10-01"));
        assert_eq!(d("11-17").as_deref(), Some("2026-11-17"));
        assert_eq!(d("12-24").as_deref(), Some("2025-12-24"));
        assert_eq!(d("tues").as_deref(), Some("2026-11-17"));
        assert_eq!(d("tuxedo"), None);
        assert_eq!(d("99999999999d"), None);
        assert_eq!(
            parse_since("week", t).map(|d| d.to_string()).as_deref(),
            Some("2026-11-13")
        );
    }

    #[test]
    fn new_and_continued_sessions_per_day() {
        let s = |id: &str, start: &str, days: &[&str]| SessionRow {
            id: id.into(),
            started: Some(start.into()),
            days: days.iter().map(|d| d.to_string()).collect(),
            ..Default::default()
        };
        // Midday UTC, so the local day is the same in any time zone within ±11h.
        let sessions = vec![
            s("b", "2026-11-18T12:00:00Z", &["2026-11-18"]),
            s("a", "2026-11-17T12:00:00Z", &["2026-11-17", "2026-11-18"]),
        ];
        let from = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let days = build(&sessions, from, to, false, |_| true);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date.to_string(), "2026-11-18");
        assert_eq!(
            (days[0].new.clone(), days[0].continued.clone()),
            (vec![0], vec![1])
        );
        assert_eq!(
            (days[1].new.clone(), days[1].continued.clone()),
            (vec![1], vec![])
        );
        let created = build(&sessions, from, to, true, |_| true);
        assert_eq!(created[0].continued, Vec::<usize>::new());
    }
}
