//! ccrumbs — find a Claude Code session fast, see what it did, and which PRs it
//! created or reviewed, then jump straight back into it.

mod config;
mod context;
mod db;
mod describe;
mod github;
mod index;
mod launch;
mod live;
mod mcp;
mod model;
mod needs;
mod parse;
mod prdetect;
mod prview;
mod search;
mod tasks;
mod text;
mod theme;
mod tickets;
mod transcript;
mod tui;
mod worklog;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use db::Db;
use model::{Role, SessionRow};
use prview::{Actor, PrEntry};
use std::io::IsTerminal;
use text::{age, duration, home_short, local_time, truncate};

#[derive(Parser)]
#[command(
    name = "ccrumbs",
    version,
    about = "Find, understand and resume your Claude Code sessions — and the PRs they created or reviewed."
)]
struct Cli {
    /// Skip the automatic incremental re-index before running.
    #[arg(long, global = true)]
    no_index: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the picker (default). Optional initial query, e.g. `ccrumbs pick #42`.
    Pick {
        query: Vec<String>,
        /// Print the resume command instead of running it.
        #[arg(long)]
        print: bool,
        /// Print the session's directory (for `cd "$(ccrumbs pick --print-dir)"`).
        #[arg(long, conflicts_with = "print")]
        print_dir: bool,
    },
    /// Jump straight into the best match; opens the picker if it's ambiguous.
    /// With no query, continues the latest session from the repo/branch you're in.
    Go {
        /// Session id prefix, PR ref (#42, owner/repo#42, URL) or search words.
        query: Vec<String>,
        /// Print the resume command instead of running it.
        #[arg(long)]
        print: bool,
        /// Open it even if it's already open in another terminal.
        #[arg(long)]
        again: bool,
    },
    /// List recent sessions with descriptions. Takes the same filters as the picker.
    List {
        /// Optional query: words, #PR, p:project, b:branch, f:file, @today, @3d, has:pr ...
        query: Vec<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Only sessions whose project path contains this.
        #[arg(short, long)]
        project: Option<String>,
        /// One line per session (no description).
        #[arg(short, long)]
        short: bool,
    },
    /// Full-text search over every prompt, reply and command.
    Search {
        terms: Vec<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Everything about one session: description, PRs, files, how to resume.
    Show { id: String },
    /// Look up a PR by number/ref and show which sessions created, reviewed or touched it.
    Pr {
        /// 42, #42, owner/repo#42 or a PR URL.
        pr: String,
        /// Fetch fresh title/state from GitHub first (needs `gh`).
        #[arg(long)]
        sync: bool,
    },
    /// PRs your sessions created or reviewed.
    Prs {
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// Only PRs in repos whose name contains this.
        #[arg(short, long)]
        repo: Option<String>,
        /// Only PRs born from a Claude session.
        #[arg(long)]
        claude: bool,
    },
    /// Sessions grouped by task (same branch, PR or ticket).
    Tasks {
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,
    },
    /// Read a session's conversation.
    View {
        id: String,
        /// Include tool output (shell results, file reads...).
        #[arg(long)]
        outputs: bool,
    },
    /// Open a session's PR, ticket or claude.ai page in the browser.
    /// Target: session id, PR ref (#42, owner/repo#42) or ticket key (ABC-123).
    Open {
        target: String,
        #[arg(long)]
        pr: bool,
        #[arg(long)]
        ticket: bool,
        #[arg(long)]
        cloud: bool,
        /// Print the URL instead of opening it.
        #[arg(long)]
        print: bool,
    },
    /// Resume a session by id prefix (in place, or in a new tab).
    Resume {
        id: String,
        #[arg(long)]
        tab: bool,
        #[arg(long)]
        print: bool,
        /// Open it even if it's already open in another terminal.
        #[arg(long)]
        again: bool,
    },
    /// Sessions whose PR needs you: failing CI, a merge conflict, new reviews or
    /// comments, or approved and ready to merge (checks GitHub with `gh`).
    Needs {
        /// Use what was fetched last time; don't ask GitHub.
        #[arg(long)]
        offline: bool,
    },
    /// What you worked on, day by day: `ccrumbs log`, `log --on tuesday`, `log --since 2w`.
    Log {
        /// Optional filters, as in the picker: p:project, has:pr, t:ABC-1 ...
        query: Vec<String>,
        /// One day: today, yesterday, tuesday, "last friday", 2026-11-17, 3d.
        #[arg(long, conflicts_with = "since")]
        on: Option<String>,
        /// First day: a day as for --on, or week, month, 2w. Default: week.
        #[arg(long)]
        since: Option<String>,
        /// Only sessions started on those days, not ones continued from before.
        #[arg(long)]
        created: bool,
        /// Markdown, to paste into a stand-up note or timesheet.
        #[arg(long)]
        md: bool,
        /// Include quick one-question sessions.
        #[arg(long)]
        all: bool,
    },
    /// Export a session (summary, PRs, files, conversation) as Markdown.
    Export {
        id: String,
        /// Output file; `-` for stdout. Default: ./<id>-<title>.md
        #[arg(short, long)]
        output: Option<String>,
        /// Include tool output (shell results, file reads...).
        #[arg(long)]
        outputs: bool,
        /// Overwrite the output file if it exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Pin a session to the top of the picker.
    Pin { id: String },
    /// Remove a pin.
    Unpin { id: String },
    /// Set a note on a session (no text clears it). Notes are searchable.
    Note { id: String, text: Vec<String> },
    /// Hide a session from lists and search (`is:hidden` shows them).
    Hide { id: String },
    /// Show a hidden session again.
    Unhide { id: String },
    /// Open a session's project in your editor (see `ccrumbs editor`).
    Edit { id: String },
    /// Which editor projects open in. No arguments: show it and the editors found.
    Editor {
        #[command(subcommand)]
        cmd: Option<EditorCmd>,
    },
    /// Print a session's directory: `cd "$(ccrumbs dir a1b2)"`.
    Dir { id: String },
    /// Links for Jira / Linear tickets. With no arguments: what's set, and which
    /// ticket prefixes appear in your sessions.
    Tickets {
        #[command(subcommand)]
        cmd: Option<TicketsCmd>,
    },
    /// Pull PR titles/state from GitHub, and find PRs opened by claude.ai cloud sessions (needs `gh`).
    Sync {
        /// Repos to sync (owner/repo). Default: every repo seen in your sessions.
        #[arg(short, long)]
        repo: Vec<String>,
        /// Most recently updated PRs to fetch per repo.
        #[arg(short = 'n', long, default_value_t = 200)]
        limit: usize,
    },
    /// Write AI titles + descriptions with Claude (needs ANTHROPIC_API_KEY).
    Describe {
        /// One session id; default: recent sessions that lack a description.
        id: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Describe every session that needs it (ignores --limit).
        #[arg(long)]
        all: bool,
        /// Re-describe even if a description exists.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        model: Option<String>,
    },
    /// Run an MCP server so Claude Code can search your past sessions:
    /// `claude mcp add ccrumbs -- ccrumbs mcp`.
    Mcp,
    /// Print shell integration (key binding + `ccd`): eval "$(ccrumbs shell-init zsh)".
    ShellInit {
        #[arg(value_parser = ["zsh", "bash", "fish"])]
        shell: String,
    },
    /// Print shell tab-completion: `ccrumbs completions zsh > ~/.zfunc/_ccrumbs`.
    Completions { shell: clap_complete::Shell },
    /// Re-index transcripts (runs automatically before every command).
    Index {
        /// Re-parse everything, not just changed files.
        #[arg(long)]
        full: bool,
    },
    /// Show configuration and paths.
    Config,
    /// Never index projects whose path contains this.
    Ignore { pattern: String },
    /// Undo `ignore`.
    Unignore { pattern: String },
}

#[derive(Subcommand)]
enum EditorCmd {
    /// `ccrumbs editor set cursor`, `set pycharm`, or any command: `set "code -n"`.
    Set { editor: Vec<String> },
    /// Forget the choice; ccrumbs asks again next time.
    Reset,
}

#[derive(Subcommand)]
enum TicketsCmd {
    /// `ccrumbs tickets set ABC https://acme.atlassian.net` (any ticket URL works too).
    /// With only a URL, it's used for every prefix.
    Set {
        /// Ticket prefix (ABC for ABC-123), or the URL to use for all prefixes.
        prefix_or_url: String,
        /// Your Jira / Linear address, or a link to any ticket.
        url: Option<String>,
    },
    /// Forget the link for a prefix (`*` for the catch-all).
    Remove { prefix: String },
}

struct Paint(bool);
impl Paint {
    fn c(&self, code: &str, s: impl AsRef<str>) -> String {
        if self.0 {
            format!("\x1b[{code}m{}\x1b[0m", s.as_ref())
        } else {
            s.as_ref().to_string()
        }
    }
    fn dim(&self, s: impl AsRef<str>) -> String {
        self.c("2", s)
    }
    fn bold(&self, s: impl AsRef<str>) -> String {
        self.c("1", s)
    }
    fn role(&self, r: Role, s: impl AsRef<str>) -> String {
        let code = match r {
            Role::Created => "1;32",
            Role::Reviewed => "1;35",
            Role::Merged => "1;34",
            Role::Commented => "33",
            Role::WorkedOn => "36",
            Role::Viewed | Role::Mentioned => "2",
        };
        self.c(code, s)
    }
}

fn main() {
    // Behave like a normal Unix CLI when piped into `head` & co.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = real_main() {
        eprintln!("ccrumbs: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load();
    let p = Paint(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());

    if let Some(Cmd::Config | Cmd::Ignore { .. } | Cmd::Unignore { .. }) = &cli.cmd {
        return config_cmd(cli.cmd.unwrap(), &mut cfg);
    }
    if let Some(Cmd::ShellInit { shell }) = &cli.cmd {
        print!("{}", shell_init(shell));
        return Ok(());
    }
    if let Some(Cmd::Completions { shell }) = &cli.cmd {
        use clap::CommandFactory;
        clap_complete::generate(
            *shell,
            &mut Cli::command(),
            "ccrumbs",
            &mut std::io::stdout(),
        );
        return Ok(());
    }
    if let Some(Cmd::Mcp) = &cli.cmd {
        return mcp::serve(cfg);
    }

    let mut db = Db::open()?;
    if let Some(Cmd::Index { full }) = &cli.cmd {
        let r = index::run(&mut db, &cfg, *full)?;
        println!(
            "Indexed {} session files from {} — {} updated, {} no longer on disk ({} ms)",
            r.scanned,
            cfg.projects_dir().display(),
            r.updated,
            r.missing,
            r.millis
        );
        return Ok(());
    }
    if !cli.no_index {
        index::run(&mut db, &cfg, false)?;
    }
    if let Some(Cmd::Pin { id } | Cmd::Unpin { id } | Cmd::Hide { id } | Cmd::Unhide { id }) =
        &cli.cmd
    {
        let sessions = db.load_sessions()?;
        let s = one(&sessions, id)?;
        let (what, done) = match cli.cmd.as_ref().unwrap() {
            Cmd::Pin { .. } => (db.set_pinned(&s.id, true), "Pinned"),
            Cmd::Unpin { .. } => (db.set_pinned(&s.id, false), "Unpinned"),
            Cmd::Hide { .. } => (db.set_hidden(&s.id, true), "Hid"),
            _ => (db.set_hidden(&s.id, false), "Unhid"),
        };
        what?;
        println!("{done} {} \"{}\"", s.short_id(), truncate(&s.title(), 60));
        return Ok(());
    }

    match cli.cmd.unwrap_or(Cmd::Pick {
        query: vec![],
        print: false,
        print_dir: false,
    }) {
        Cmd::Pick {
            query,
            print,
            print_dir,
        } => {
            let mode = if print_dir {
                PickMode::Dir
            } else if print {
                PickMode::Cmd
            } else {
                PickMode::Resume
            };
            pick_mode(&mut db, &cfg, &query.join(" "), mode)
        }
        Cmd::View { id, outputs } => {
            let sessions = db.load_sessions()?;
            let s = one(&sessions, &id)?;
            let entries = transcript::load(&s.path);
            if entries.is_empty() {
                bail!("no readable transcript at {}", s.path);
            }
            println!("{}", p.bold(s.title()));
            print!("{}", transcript::to_text(&entries, outputs));
            Ok(())
        }
        Cmd::Export {
            id,
            output,
            outputs,
            force,
        } => {
            let sessions = db.load_sessions()?;
            let s = one(&sessions, &id)?;
            let md = transcript::to_markdown(s, &transcript::load(&s.path), outputs);
            match output.as_deref() {
                Some("-") => print!("{md}"),
                o => {
                    let path = o
                        .map(str::to_string)
                        .unwrap_or_else(|| transcript::file_name(s));
                    if std::path::Path::new(&path).exists() && !force {
                        bail!("{path} already exists (use --force to overwrite)");
                    }
                    std::fs::write(&path, md)?;
                    eprintln!("Wrote {path}");
                }
            }
            Ok(())
        }
        Cmd::Open {
            target,
            pr,
            ticket,
            cloud,
            print,
        } => open_cmd(&db, &target, pr, ticket, cloud, print),
        Cmd::Pin { id } | Cmd::Unpin { id } | Cmd::Hide { id } | Cmd::Unhide { id } => {
            unreachable!("{id}")
        }
        Cmd::Note { id, text } => {
            let sessions = db.load_sessions()?;
            let s = one(&sessions, &id)?;
            let text = text.join(" ");
            db.set_note(&s.id, Some(&text))?;
            println!(
                "{} {}",
                if text.trim().is_empty() {
                    "Cleared the note on"
                } else {
                    "Noted"
                },
                s.short_id()
            );
            Ok(())
        }
        Cmd::Dir { id } => {
            let sessions = db.load_sessions()?;
            println!("{}", one(&sessions, &id)?.resume_dir());
            Ok(())
        }
        Cmd::Tickets { cmd } => tickets_cmd(&p, &db, cmd),
        Cmd::Editor { cmd } => editor_cmd(&p, cmd),
        Cmd::Edit { id } => {
            let sessions = db.load_sessions()?;
            let s = one(&sessions, &id)?;
            let Some(ed) = launch::chosen_editor(cfg.editor.as_deref()) else {
                bail!(
                    "no editor chosen yet: run `ccrumbs editor` to see what's installed, then `ccrumbs editor set <name>`"
                );
            };
            let (mut cmd, gui) = launch::editor_command(&ed, &s.resume_dir())?;
            if gui {
                cmd.spawn()?;
                eprintln!("Opened {} with {ed}", home_short(&s.resume_dir()));
            } else {
                cmd.status()?;
            }
            Ok(())
        }
        Cmd::ShellInit { .. } | Cmd::Mcp | Cmd::Completions { .. } => unreachable!(),
        Cmd::Go {
            query,
            print,
            again,
        } => go(&mut db, &cfg, &query.join(" "), print, again),
        Cmd::Needs { offline } => needs_cmd(&p, &db, offline),
        Cmd::Log {
            query,
            on,
            since,
            created,
            md,
            all,
        } => log_cmd(&p, &db, &query.join(" "), on, since, created, md, all),
        Cmd::List {
            query,
            limit,
            project,
            short,
        } => {
            let mut sessions = db.load_sessions()?;
            live::annotate(&mut sessions);
            let mut q = search::parse(&query.join(" "));
            q.project.extend(project.map(|p| p.to_lowercase()));
            let prs = prview::build(&sessions, &db.load_prs()?);
            let (_, task_of) = tasks::build(&sessions, &prs);
            let corpus = search::Corpus::new(&sessions, None, task_of);
            let hits =
                search::Ranker::default().rank(&sessions, &corpus, &q, search::Opts::default());
            if hits.is_empty() {
                println!("No matching sessions.");
            }
            for i in hits.into_iter().take(limit) {
                print_row(&p, &sessions[i], !short);
            }
            Ok(())
        }
        Cmd::Tasks { limit } => {
            let sessions = db.load_sessions()?;
            let prs = prview::build(&sessions, &db.load_prs()?);
            let (tasks, _) = tasks::build(&sessions, &prs);
            if tasks.is_empty() {
                println!(
                    "No tasks yet: sessions are grouped when they work on the same branch or the same PR."
                );
            }
            for t in tasks.iter().take(limit) {
                print_task(&p, t, &sessions, &prs);
            }
            Ok(())
        }
        Cmd::Search { terms, limit } => {
            let sessions = db.load_sessions()?;
            let ids = db.fts(&terms.join(" "), limit)?;
            if ids.is_empty() {
                println!("No matches.");
            }
            for id in ids {
                if let Some(s) = sessions.iter().find(|s| s.id == id) {
                    print_row(&p, s, true);
                }
            }
            Ok(())
        }
        Cmd::Show { id } => {
            let mut sessions = db.load_sessions()?;
            live::annotate(&mut sessions);
            let s = one(&sessions, &id)?;
            let prs = prview::build(&sessions, &db.load_prs()?);
            show(&p, s, &sessions, &prs);
            Ok(())
        }
        Cmd::Pr { pr, sync } => pr_cmd(&p, &db, &pr, sync),
        Cmd::Prs {
            limit,
            repo,
            claude,
        } => {
            let sessions = db.load_sessions()?;
            let prs = prview::build(&sessions, &db.load_prs()?);
            let mut n = 0;
            for e in prs.iter().filter(|e| e.has_significant()) {
                if claude && !e.born_from_claude() {
                    continue;
                }
                if repo.as_ref().is_some_and(|r| !e.repo.contains(r.as_str())) {
                    continue;
                }
                print_pr_row(&p, e, &sessions);
                n += 1;
                if n >= limit {
                    break;
                }
            }
            if n == 0 {
                println!(
                    "No PRs found. Sessions that run `gh pr create/review` or GitHub MCP tools show up here; `ccrumbs sync` adds PRs opened from claude.ai."
                );
            }
            Ok(())
        }
        Cmd::Resume {
            id,
            tab,
            print,
            again,
        } => {
            let mut sessions = db.load_sessions()?;
            live::annotate(&mut sessions);
            let s = one(&sessions, &id)?;
            if print {
                println!("{}", launch::resume_command(s));
                Ok(())
            } else if !again && let Some(msg) = launch::already_open(s)? {
                eprintln!("{msg}");
                Ok(())
            } else if tab {
                println!("{}", launch::open_tab(s)?);
                Ok(())
            } else {
                launch::resume_here(s)
            }
        }
        Cmd::Describe {
            id,
            limit,
            all,
            force,
            model,
        } => {
            let model = model.unwrap_or_else(|| cfg.describe_model.clone());
            describe_cmd(
                &p,
                &db,
                id,
                if all { usize::MAX } else { limit },
                force,
                &model,
            )
        }
        Cmd::Sync { repo, limit } => {
            let repos = if repo.is_empty() {
                db.known_repos()?
            } else {
                repo
            };
            if repos.is_empty() {
                println!(
                    "No GitHub repos seen in your sessions yet. Pass one with --repo owner/name."
                );
            }
            for r in repos {
                match github::sync_repo(&db, &r, limit) {
                    Ok(n) => println!("{r}: {n} PRs"),
                    Err(e) => eprintln!("{r}: {e:#}"),
                }
            }
            Ok(())
        }
        Cmd::Index { .. } | Cmd::Config | Cmd::Ignore { .. } | Cmd::Unignore { .. } => {
            unreachable!()
        }
    }
}

fn config_cmd(cmd: Cmd, cfg: &mut Config) -> Result<()> {
    match cmd {
        Cmd::Ignore { pattern } => {
            if !cfg.ignored_projects.contains(&pattern) {
                cfg.ignored_projects.push(pattern.clone());
            }
            cfg.save()?;
            println!(
                "Ignoring projects matching {pattern:?} (run `ccrumbs index --full` to drop already-indexed ones)"
            );
        }
        Cmd::Unignore { pattern } => {
            cfg.ignored_projects.retain(|p| p != &pattern);
            cfg.save()?;
            println!("No longer ignoring {pattern:?}");
        }
        _ => {
            println!("config file      {}", config::config_path().display());
            println!("index database   {}", config::db_path().display());
            println!("transcripts      {}", cfg.projects_dir().display());
            println!("describe model   {}", cfg.describe_model);
            println!("ignored projects {:?}", cfg.ignored_projects);
            println!(
                "refresh_prs      {} (check open PRs' CI and reviews when the picker opens)",
                cfg.refresh_prs
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum PickMode {
    Resume,
    Cmd,
    Dir,
}

fn pick(db: &mut Db, cfg: &Config, query: &str) -> Result<()> {
    pick_mode(db, cfg, query, PickMode::Resume)
}

fn pick_mode(db: &mut Db, cfg: &Config, query: &str, mode: PickMode) -> Result<()> {
    // The picker draws on stderr, so stdout may be captured by `$(...)`.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "the picker needs a terminal; `ccrumbs list <query>` shows matches, `ccrumbs resume <id> --print` prints a command"
        );
    }
    match (tui::run(db, cfg, query)?, mode) {
        (tui::Outcome::Resume(s), PickMode::Resume) => launch::resume_here(&s),
        (tui::Outcome::Resume(s), PickMode::Cmd) => {
            println!("{}", launch::resume_command(&s));
            Ok(())
        }
        (tui::Outcome::Resume(s), PickMode::Dir) => {
            println!("{}", s.resume_dir());
            Ok(())
        }
        (tui::Outcome::Switched(msg), _) => {
            eprintln!("{msg}");
            Ok(())
        }
        (tui::Outcome::Quit, _) => Ok(()),
    }
}

fn editor_cmd(p: &Paint, cmd: Option<EditorCmd>) -> Result<()> {
    match cmd {
        Some(EditorCmd::Set { editor }) => {
            let wanted = editor.join(" ");
            if wanted.trim().is_empty() {
                bail!("say which editor, e.g. `ccrumbs editor set cursor`");
            }
            let command = launch::resolve_editor(&wanted);
            config::set_editor(Some(&command))?;
            println!("Projects now open with: {command}");
        }
        Some(EditorCmd::Reset) => {
            config::set_editor(None)?;
            println!("Forgot the editor; Ctrl-E in the picker will ask again.");
        }
        None => {
            let current = launch::chosen_editor(Config::load().editor.as_deref());
            match &current {
                Some(c) => println!("Projects open with: {}", p.bold(c)),
                None => println!(
                    "No editor chosen yet: Ctrl-E in the picker asks, or use `ccrumbs editor set <name>`."
                ),
            }
            let found = launch::detect_editors();
            if !found.is_empty() {
                println!();
                println!("{}", p.bold("Editors found"));
                for e in found {
                    let kind = if e.gui { "app" } else { "in terminal" };
                    let mark = if current.as_deref() == Some(e.command.as_str()) {
                        p.c("32", " (current)")
                    } else {
                        String::new()
                    };
                    println!("  {:<18} {:<34} {}{mark}", e.label, e.command, p.dim(kind));
                }
            }
        }
    }
    Ok(())
}

fn tickets_cmd(p: &Paint, db: &Db, cmd: Option<TicketsCmd>) -> Result<()> {
    match cmd {
        Some(TicketsCmd::Set { prefix_or_url, url }) => {
            let (prefix, url) = match url {
                Some(u) => (prefix_or_url.trim().to_uppercase(), u),
                None => ("*".to_string(), prefix_or_url),
            };
            let prefix = if prefix == "ANY" || prefix == "ALL" {
                "*".to_string()
            } else {
                prefix
            };
            let tpl = config::ticket_template(&url)?;
            config::set_ticket_url(&prefix, &tpl)?;
            let example = if prefix == "*" {
                "ABC-123".to_string()
            } else {
                format!("{prefix}-123")
            };
            println!(
                "Saved: {} → {}",
                if prefix == "*" {
                    "all tickets"
                } else {
                    prefix.as_str()
                },
                tpl
            );
            println!(
                "{}",
                p.dim(format!(
                    "e.g. {example} opens {}",
                    tpl.replace("{key}", &example)
                ))
            );
        }
        Some(TicketsCmd::Remove { prefix }) => {
            let prefix = prefix.trim().to_uppercase();
            if config::remove_ticket_url(&prefix)? {
                println!("Removed the link for {prefix}");
            } else {
                println!("No link was set for {prefix}");
            }
        }
        None => {
            let links = config::ticket_templates();
            if links.is_empty() {
                println!("No ticket links yet. Set one with:");
                println!("  ccrumbs tickets set ABC https://yourcompany.atlassian.net");
                println!("  ccrumbs tickets set ENG https://linear.app/yourteam");
            } else {
                println!("{}", p.bold("Ticket links"));
                for (k, v) in &links {
                    println!("  {:<6} {v}", if k == "*" { "(all)" } else { k.as_str() });
                }
            }
            let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
            for s in db.load_sessions()? {
                for t in &s.tickets {
                    *seen.entry(tickets::prefix(t).to_string()).or_default() += 1;
                }
            }
            if !seen.is_empty() {
                println!();
                println!("{}", p.bold("Ticket prefixes in your sessions"));
                for (k, n) in seen {
                    let linked = links.contains_key(&k) || links.contains_key("*");
                    let status = if linked {
                        p.c("32", "linked")
                    } else {
                        p.c("33", format!("no link: ccrumbs tickets set {k} <url>"))
                    };
                    println!(
                        "  {k:<6} {n:>3} session{}  {status}",
                        if n == 1 { " " } else { "s" }
                    );
                }
            }
        }
    }
    Ok(())
}

fn shell_init(shell: &str) -> String {
    match shell {
        "fish" => {
            r#"# ccrumbs: add to ~/.config/fish/config.fish:  ccrumbs shell-init fish | source
# Alt-S opens the session picker from any prompt.
bind \es 'ccrumbs </dev/tty; commandline -f repaint'
# ccd [query]: pick a session and cd into its directory.
function ccd
    set -l d (ccrumbs pick --print-dir $argv)
    and test -n "$d"
    and cd $d
end
"#
            .into()
        }
        "bash" => r#"# ccrumbs: add to ~/.bashrc:  eval "$(ccrumbs shell-init bash)"
# Alt-S opens the session picker from any prompt.
bind -x '"\es": ccrumbs </dev/tty'
# ccd [query]: pick a session and cd into its directory.
ccd() { local d; d="$(ccrumbs pick --print-dir "$@")" && [ -n "$d" ] && cd "$d"; }
"#
        .into(),
        _ => r#"# ccrumbs: add to ~/.zshrc:  eval "$(ccrumbs shell-init zsh)"
# Alt-S opens the session picker from any prompt.
_ccrumbs_pick() { ccrumbs </dev/tty; zle reset-prompt; }
zle -N _ccrumbs_pick
bindkey '\es' _ccrumbs_pick
# ccd [query]: pick a session and cd into its directory.
ccd() { local d; d="$(ccrumbs pick --print-dir "$@")" && [ -n "$d" ] && cd "$d"; }
"#
        .into(),
    }
}

/// URL for a session's PR / ticket / claude.ai page, by preference or by flag.
fn open_cmd(db: &Db, target: &str, pr: bool, ticket: bool, cloud: bool, print: bool) -> Result<()> {
    let sessions = db.load_sessions()?;
    let prs = prview::build(&sessions, &db.load_prs()?);
    let pr_url = |e: &PrEntry| {
        e.meta.as_ref().and_then(|m| m.url.clone()).or_else(|| {
            (!e.repo.is_empty()).then(|| format!("https://github.com/{}/pull/{}", e.repo, e.number))
        })
    };
    let ticket_url = |k: &str| {
        config::ticket_url(k).ok_or_else(|| {
            anyhow::anyhow!(
                "no link set for {k} tickets yet: ccrumbs tickets set {} <your Jira or Linear URL>",
                tickets::prefix(k)
            )
        })
    };
    let url = if let Some(k) = tickets::from_text(target).into_iter().find(|k| k == target) {
        ticket_url(&k)?
    } else if target.starts_with('#') || target.contains("/pull/") || target.contains('#') {
        let hits = prview::lookup(&prs, target);
        match hits.as_slice() {
            [e] => {
                pr_url(e).ok_or_else(|| anyhow::anyhow!("don't know which repo {target} is in"))?
            }
            [] => bail!("no session touched PR {target}"),
            _ => bail!("{target} matches several repos; use owner/repo#N"),
        }
    } else {
        let s = one(&sessions, target)?;
        let session_pr = || {
            s.pr_summary()
                .into_iter()
                .find(|(l, _)| l.role.is_significant())
                .and_then(|(l, _)| {
                    prs.iter()
                        .find(|e| e.repo == l.repo && e.number == l.number)
                        .and_then(pr_url)
                })
        };
        let cloud_url = || {
            s.cloud_id
                .as_ref()
                .map(|c| format!("https://claude.ai/code/{c}"))
        };
        let ticket_first = || s.tickets.first().map(|k| ticket_url(k)).transpose();
        if pr {
            session_pr().ok_or_else(|| anyhow::anyhow!("{} has no PR", s.short_id()))?
        } else if ticket {
            ticket_first()?.ok_or_else(|| anyhow::anyhow!("{} has no ticket key", s.short_id()))?
        } else if cloud {
            cloud_url()
                .ok_or_else(|| anyhow::anyhow!("{} isn't a claude.ai session", s.short_id()))?
        } else if let Some(u) = session_pr() {
            u
        } else if let Some(u) = s.tickets.first().and_then(|k| config::ticket_url(k)) {
            u
        } else {
            cloud_url().ok_or_else(|| {
                anyhow::anyhow!("{} has no PR, ticket or claude.ai page", s.short_id())
            })?
        }
    };
    if print {
        println!("{url}");
    } else {
        launch::open_url(&url)?;
        eprintln!("Opened {url}");
    }
    Ok(())
}

fn go(db: &mut Db, cfg: &Config, query: &str, print: bool, again: bool) -> Result<()> {
    let mut sessions = db.load_sessions()?;
    if !print {
        live::annotate(&mut sessions);
    }
    let run = |s: &SessionRow| -> Result<()> {
        if print {
            println!("{}", launch::resume_command(s));
            Ok(())
        } else if !again && let Some(msg) = launch::already_open(s)? {
            eprintln!("{msg}");
            Ok(())
        } else {
            launch::resume_here(s)
        }
    };
    if query.trim().is_empty() {
        // No query: continue the latest session from here (this branch first, then this repo).
        let ctx = context::Context::detect();
        let best = ctx.as_ref().and_then(|c| {
            let tier = |s: &SessionRow| c.tier(s);
            sessions
                .iter()
                .filter(|s| !s.missing && tier(s) > 0)
                .max_by_key(|s| (tier(s), s.ended.clone()))
        });
        return match best {
            Some(s) => {
                eprintln!(
                    "Latest session in {}: {}",
                    ctx.map(|c| c.label()).unwrap_or_default(),
                    s.title()
                );
                run(s)
            }
            None if print => pick_mode(db, cfg, "", PickMode::Cmd),
            None => pick(db, cfg, ""),
        };
    }
    // 1. Session id prefix.
    let by_id = search::by_id(&sessions, query);
    if by_id.len() == 1 {
        return run(by_id[0]);
    }
    // 2. A lone PR ref: the session that created it, else the one that reviewed it.
    let q = search::parse(query);
    if q.text.is_empty() && q.prs.len() == 1 {
        let prs = prview::build(&sessions, &db.load_prs()?);
        let (repo, n) = &q.prs[0];
        let r = match repo {
            Some(r) => format!("{r}#{n}"),
            None => n.to_string(),
        };
        let hits = prview::lookup(&prs, &r);
        if hits.len() == 1 {
            let e = hits[0];
            for actors in [e.creators(), e.reviewers()] {
                let local: Vec<usize> = actors
                    .iter()
                    .filter_map(|a| {
                        if let Actor::Local(i) = a {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                    .collect();
                if local.len() == 1 && !sessions[local[0]].missing {
                    return run(&sessions[local[0]]);
                }
            }
        }
    }
    // 3. Search; resume only when there's exactly one hit.
    let prs = prview::build(&sessions, &db.load_prs()?);
    let (_, task_of) = tasks::build(&sessions, &prs);
    let ctx = context::Context::detect();
    let corpus = search::Corpus::new(&sessions, ctx.as_ref(), task_of);
    let opts = search::Opts {
        here_first: true,
        hide_quick: false,
    };
    let hits = search::Ranker::default().rank(&sessions, &corpus, &q, opts);
    if hits.len() == 1 {
        return run(&sessions[hits[0]]);
    }
    if hits.is_empty() {
        bail!("no session matches {query:?}");
    }
    if print && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        // Let the user choose; print the choice (stdout stays a single command).
        return pick_mode(db, cfg, query, PickMode::Cmd);
    }
    if print || !std::io::stdout().is_terminal() {
        // Ambiguous: list the candidates on stderr, so `$(ccrumbs go … --print)`
        // never captures anything but a command.
        for &i in hits.iter().take(10) {
            let s = &sessions[i];
            eprintln!(
                "  {}  {:>4}  {}",
                s.short_id(),
                text::age(s.ended.as_deref()),
                text::truncate(&s.title(), 80)
            );
        }
        bail!(
            "{} sessions match {query:?}; add words or use an id",
            hits.len()
        );
    }
    pick(db, cfg, query)
}

fn one<'a>(sessions: &'a [SessionRow], id: &str) -> Result<&'a SessionRow> {
    let m = search::by_id(sessions, id);
    match m.len() {
        0 if id.trim().len() < 4 => bail!("give at least 4 characters of the session id"),
        0 => bail!("no session with id starting {id:?}"),
        1 => Ok(m[0]),
        n => bail!("{n} sessions start with {id:?} — use more characters"),
    }
}

fn badges(p: &Paint, s: &SessionRow) -> String {
    let mut seen = Vec::new();
    let mut out = String::new();
    for l in s.prs.iter().filter(|l| l.role.is_significant()) {
        if !seen.contains(&l.number) {
            seen.push(l.number);
            out.push(' ');
            out.push_str(&p.role(
                l.role,
                format!("{}#{} {}", l.role.icon(), l.number, l.role.as_str()),
            ));
        }
    }
    out
}

fn print_row(p: &Paint, s: &SessionRow, long: bool) {
    let ctx = match s.context_pct() {
        Some(pct) => {
            let code = match pct {
                0..50 => "32",
                50..80 => "33",
                _ => "31",
            };
            p.c(code, format!("{pct:>3}%"))
        }
        None => "    ".to_string(),
    };
    let mark = match (s.pinned, s.note.is_some()) {
        (true, _) => p.c("33", "↑ "),
        (false, true) => p.c("33", "≡ "),
        _ => String::new(),
    };
    let mut tickets: String = s
        .tickets
        .iter()
        .map(|t| format!(" {}", p.c("36", t)))
        .collect();
    if s.live.is_some() {
        tickets.push_str(&format!(" {}", p.c("32", "● open")));
    }
    if !s.needs.is_empty() {
        tickets.push_str(&format!(" {}", p.c("31", "! needs you")));
    }
    println!(
        "{}  {:>4}  {}  {:<18}  {mark}{}{}{tickets}",
        p.c("34", s.short_id()),
        age(s.ended.as_deref().or(s.started.as_deref())),
        ctx,
        truncate(&s.project_name(), 18),
        p.bold(truncate(&s.title(), 70)),
        badges(p, s)
    );
    if long {
        for l in s.description().lines().take(3) {
            println!("                {}", p.dim(truncate(l, 110)));
        }
    }
}

fn actor(p: &Paint, sessions: &[SessionRow], a: &Actor) -> String {
    match a {
        Actor::Local(i) => format!(
            "{} \"{}\"",
            p.c("34", sessions[*i].short_id()),
            truncate(&sessions[*i].title(), 40)
        ),
        Actor::Cloud(c) => format!("cloud https://claude.ai/code/{c}"),
    }
}

/// "Where you left off": Claude's open to-dos and the start of its last reply.
fn left_off(p: &Paint, s: &SessionRow) {
    let open = s.open_todos();
    let reply = s.reply_excerpt(3);
    if open.is_empty() && reply.is_empty() {
        return;
    }
    println!();
    println!("{}", p.c("1;36", "Where you left off"));
    if !open.is_empty() {
        println!(
            "  {}",
            p.dim(format!("{} of {} to-dos open", open.len(), s.todos.len()))
        );
        for t in open.iter().take(8) {
            if t.status == "in_progress" {
                println!("  {} {}", p.c("33", "►"), t.text);
            } else {
                println!("  {} {}", p.dim("○"), t.text);
            }
        }
        if open.len() > 8 {
            println!("  {}", p.dim(format!("+{} more", open.len() - 8)));
        }
    }
    if !reply.is_empty() {
        println!("  {}", p.dim("Claude's last reply:"));
        for l in reply {
            println!("  {}", p.dim(truncate(&l, 110)));
        }
    }
}

/// `ccrumbs needs`: refresh PR status (unless offline) and list what needs you.
fn needs_cmd(p: &Paint, db: &Db, offline: bool) -> Result<()> {
    let mut sessions = db.load_sessions()?;
    if !offline {
        if !github::available() {
            bail!(
                "`gh` (the GitHub CLI) is needed to check your PRs; --offline shows what was fetched last time"
            );
        }
        let stale = needs::stale(&sessions, &db.load_prs()?, chrono::Duration::zero());
        if !stale.is_empty() {
            eprintln!(
                "{}",
                p.dim(format!(
                    "Checking {} open PR{}…",
                    stale.len(),
                    if stale.len() == 1 { "" } else { "s" }
                ))
            );
            let (_, err) = needs::refresh(db, &stale);
            if let Some(e) = err {
                eprintln!(
                    "{}",
                    p.c("33", format!("some PRs could not be checked: {e}"))
                );
            }
            sessions = db.load_sessions()?;
        }
    }
    live::annotate(&mut sessions);
    let hits: Vec<&SessionRow> = sessions
        .iter()
        .filter(|s| !s.needs.is_empty() && !s.hidden)
        .collect();
    if hits.is_empty() {
        println!(
            "Nothing needs you: no open PR from your sessions has failing CI, conflicts, new feedback or an approval."
        );
        return Ok(());
    }
    for s in hits {
        println!(
            "{}  {:>4}  {:<18}  {}{}",
            p.c("34", s.short_id()),
            age(s.ended.as_deref().or(s.started.as_deref())),
            truncate(&s.project_name(), 18),
            p.bold(truncate(&s.title(), 72)),
            if s.live.is_some() {
                format!("  {}", p.c("32", "● open"))
            } else {
                String::new()
            }
        );
        for n in &s.needs {
            println!("                {}", p.c("31", format!("! {n}")));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn log_cmd(
    p: &Paint,
    db: &Db,
    query: &str,
    on: Option<String>,
    since: Option<String>,
    created: bool,
    md: bool,
    all: bool,
) -> Result<()> {
    let sessions = db.load_sessions()?;
    let today = chrono::Local::now().date_naive();
    let (from, to) = match (&on, &since) {
        (Some(d), _) => {
            let d = worklog::parse_day(d, today).with_context(|| {
                format!("can't read the day {d:?}; try today, yesterday, tuesday, \"last friday\", 2026-11-17 or 3d")
            })?;
            (d, d)
        }
        (None, Some(s)) => (
            worklog::parse_since(s, today).with_context(|| {
                format!("can't read {s:?}; try week, month, 2w, tuesday or 2026-11-01")
            })?,
            today,
        ),
        (None, None) => (worklog::parse_since("week", today).unwrap_or(today), today),
    };
    let q = search::parse(query);
    let keep = |s: &SessionRow| search::passes(s, None, &q) && (all || !search::is_quick(s));
    let days = worklog::build(&sessions, from, to, created, keep);
    if days.is_empty() {
        let span = if from == to {
            from.format("%a %d %b").to_string()
        } else {
            format!("{} – {}", from.format("%a %d %b"), to.format("%a %d %b"))
        };
        println!("No sessions on {span}.");
        return Ok(());
    }
    for (n, d) in days.iter().enumerate() {
        if n > 0 {
            println!();
        }
        let title = d.date.format("%A %d %B").to_string();
        if md {
            println!("## {title}");
        } else {
            println!(
                "{}  {}",
                p.bold(&title),
                p.dim(worklog::day_summary(&sessions, d))
            );
        }
        for (label, list) in [("", &d.new), ("continued", &d.continued)] {
            if list.is_empty() {
                continue;
            }
            if !label.is_empty() && !d.new.is_empty() {
                if md {
                    println!("\n*Continued from earlier:*");
                } else {
                    println!("  {}", p.dim("continued from earlier"));
                }
            }
            for &i in list.iter() {
                log_line(p, &sessions[i], md, label.is_empty());
            }
        }
    }
    Ok(())
}

fn log_line(p: &Paint, s: &SessionRow, md: bool, new: bool) {
    let work: Vec<String> = worklog::pr_work(s)
        .into_iter()
        .map(|(r, pr)| format!("{} {pr}", r.as_str()))
        .chain(s.tickets.iter().cloned())
        .collect();
    if md {
        let mut l = format!("- **{}** ({})", s.title(), s.project_name());
        if !work.is_empty() {
            l.push_str(&format!(" — {}", work.join(", ")));
        }
        println!("{l}");
        return;
    }
    let when = if new {
        s.started
            .as_deref()
            .and_then(text::parse_ts)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default()
    } else {
        "  ·  ".to_string()
    };
    let title = truncate(&s.title(), 64);
    let pad = " ".repeat(64usize.saturating_sub(text::width(&title)));
    let work = if work.is_empty() {
        String::new()
    } else {
        format!("{pad}  {}", p.c("36", work.join(" · ")))
    };
    println!(
        "  {}  {:<16}  {title}{work}",
        p.dim(format!("{when:>5}")),
        truncate(&s.project_name(), 16),
    );
}

fn show(p: &Paint, s: &SessionRow, sessions: &[SessionRow], prs: &[PrEntry]) {
    println!("{}", p.bold(s.title()));
    println!(
        "{}  {}",
        p.c("34", &s.id),
        p.dim(
            s.branch
                .as_ref()
                .map(|b| format!("branch {b}"))
                .unwrap_or_default()
        )
    );
    println!("{}", home_short(&s.resume_dir()));
    if let Some(l) = &s.live {
        println!(
            "{}",
            p.c("32", format!("● open now in another terminal ({})", l.tty))
        );
    }
    let mut meta = format!(
        "{} · {} prompt{}",
        local_time(s.started.as_deref()),
        s.user_prompts,
        if s.user_prompts == 1 { "" } else { "s" }
    );
    if let Some(d) = duration(s.started.as_deref(), s.ended.as_deref()) {
        meta.push_str(&format!(" · {d}"));
    }
    if s.tokens_in + s.tokens_out > 0 {
        meta.push_str(&format!(
            " · {} in / {} out tokens",
            text::tokens(s.tokens_in),
            text::tokens(s.tokens_out)
        ));
    }
    if s.cost_usd > 0.0 {
        meta.push_str(&format!(" · ${:.2}", s.cost_usd));
    }
    if s.tests_ran {
        meta.push_str(" · ran tests");
    }
    if s.commits > 0 {
        meta.push_str(&format!(
            " · {} commit{}",
            s.commits,
            if s.commits == 1 { "" } else { "s" }
        ));
    }
    if let Some(m) = &s.model {
        meta.push_str(&format!(" · {m}"));
    }
    println!("{}", p.dim(meta));
    if let Some(c) = s.context_summary() {
        println!("{}", p.dim(c));
    }
    if let Some(n) = &s.note {
        println!("{}", p.c("33", format!("note: {n}")));
    }
    if s.pinned {
        println!("{}", p.c("33", "pinned"));
    }
    for t in &s.tickets {
        println!(
            "{}",
            p.c(
                "36",
                format!("ticket {t}  {}", config::ticket_url(t).unwrap_or_default())
            )
        );
    }
    if let Some(c) = &s.cloud_id {
        println!(
            "{}",
            p.dim(format!("cloud session: https://claude.ai/code/{c}"))
        );
    }
    println!();
    println!(
        "{}",
        p.c(
            "1;36",
            if s.ai_desc.is_some() {
                "Description (AI)"
            } else {
                "Description"
            }
        )
    );
    for l in s.description().lines() {
        println!("  {l}");
    }
    if !s.needs.is_empty() {
        println!();
        println!("{}", p.c("1;31", "Needs you"));
        for n in &s.needs {
            println!("  {}", p.c("31", n));
        }
    }
    left_off(p, s);
    let me = sessions.iter().position(|x| x.id == s.id);
    let linked = s.pr_summary();
    if !linked.is_empty() {
        println!();
        println!("{}", p.c("1;36", "Pull requests"));
        for (l, also) in linked {
            let e = prs
                .iter()
                .find(|e| e.repo == l.repo && e.number == l.number);
            let title = e
                .and_then(|e| e.title())
                .map(|t| format!("  {}", truncate(t, 50)))
                .unwrap_or_default();
            println!(
                "  {}  {}#{}{}  {}",
                p.role(l.role, format!("{} {:<9}", l.role.icon(), l.role.verb())),
                l.repo,
                l.number,
                title,
                p.dim(if also.is_empty() {
                    l.evidence.clone()
                } else {
                    format!(
                        "{} (also {})",
                        l.evidence,
                        also.iter()
                            .map(|r| r.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            );
            if let Some(e) = e {
                for (role, actors) in [
                    (Role::Created, e.creators()),
                    (Role::Reviewed, e.reviewers()),
                ] {
                    for a in actors
                        .iter()
                        .filter(|a| me.is_none_or(|m| **a != Actor::Local(m)))
                    {
                        println!(
                            "      └ {} in another session: {}",
                            p.role(role, role.as_str()),
                            actor(p, sessions, a)
                        );
                    }
                }
            }
        }
    }
    if !s.files_edited.is_empty() {
        println!();
        println!(
            "{}",
            p.c("1;36", format!("Files edited ({})", s.files_edited.len()))
        );
        for f in s.files_edited.iter().take(30) {
            println!("  {}", p.dim(f));
        }
    }
    println!();
    if s.missing {
        println!(
            "{}",
            p.c("31", "Transcript no longer on disk — it can't be resumed.")
        );
    } else {
        println!("Resume: {}", p.bold(launch::resume_command(s)));
    }
    println!("{}", p.dim(format!("Transcript: {}", s.path)));
}

fn print_task(p: &Paint, t: &tasks::Task, sessions: &[SessionRow], prs: &[PrEntry]) {
    let branch = t
        .branch
        .as_ref()
        .map(|b| format!(" · {b}"))
        .unwrap_or_default();
    let extra = format!(
        "{}{}",
        if t.cost_usd > 0.0 {
            format!(" · ${:.2}", t.cost_usd)
        } else {
            String::new()
        },
        t.tickets
            .iter()
            .map(|k| format!(" · {k}"))
            .collect::<String>()
    );
    println!(
        "{}  {}{}  {}{}",
        p.bold(truncate(&t.title, 60)),
        p.c("34", &t.project),
        p.dim(branch),
        p.c("33", format!("{} sessions", t.sessions.len())),
        p.dim(extra)
    );
    for &pi in &t.prs {
        println!(
            "    {} {}",
            p.bold(prs[pi].key()),
            p.dim(prs[pi].story(sessions))
        );
    }
    for &i in t.sessions.iter().rev() {
        let s = &sessions[i];
        let roles: Vec<String> = t
            .prs
            .iter()
            .filter_map(|&pi| {
                let r = prs[pi]
                    .links
                    .iter()
                    .filter(|l| l.session == Some(i))
                    .map(|l| l.role)
                    .min()?;
                r.is_significant().then(|| {
                    p.role(
                        r,
                        format!("{} {} #{}", r.icon(), r.as_str(), prs[pi].number),
                    )
                })
            })
            .collect();
        println!(
            "    {}  {}  {}  {}",
            p.c("34", s.short_id()),
            p.dim(local_time(s.started.as_deref())),
            truncate(&s.title(), 50),
            roles.join(" ")
        );
    }
    println!();
}

fn print_pr_row(p: &Paint, e: &PrEntry, sessions: &[SessionRow]) {
    let mark = if e.born_from_claude() {
        p.c("32", "●")
    } else {
        p.dim("○")
    };
    let state = e.state().map(|s| format!(" [{s}]")).unwrap_or_default();
    let title = e
        .title()
        .map(|t| format!("  {}", truncate(t, 60)))
        .unwrap_or_default();
    println!("{mark} {}{}{}", p.bold(e.key()), p.dim(state), title);
    println!("    {}", p.dim(e.story(sessions)));
}

fn pr_cmd(p: &Paint, db: &Db, q: &str, sync: bool) -> Result<()> {
    let mut sessions = db.load_sessions()?;
    let mut prs = prview::build(&sessions, &db.load_prs()?);
    let mut hits: Vec<PrEntry> = prview::lookup(&prs, q).into_iter().cloned().collect();
    if sync {
        let mut targets: Vec<(String, u32)> = hits
            .iter()
            .filter(|e| !e.repo.is_empty())
            .map(|e| (e.repo.clone(), e.number))
            .collect();
        if let Some((Some(r), n)) = prdetect::parse_ref(q).or_else(|| {
            q.rsplit_once('#')
                .and_then(|(r, n)| Some((Some(r.to_string()), n.parse().ok()?)))
        }) && r.contains('/')
            && !targets.contains(&(r.clone(), n))
        {
            targets.push((r, n));
        }
        for (r, n) in targets {
            if let Err(e) = github::sync_one(db, &r, n) {
                eprintln!("{r}#{n}: {e:#}");
            }
        }
        sessions = db.load_sessions()?;
        prs = prview::build(&sessions, &db.load_prs()?);
        hits = prview::lookup(&prs, q).into_iter().cloned().collect();
    }
    if hits.is_empty() {
        bail!("no session touched PR {q:?} (try `ccrumbs pr {q} --sync` or `ccrumbs sync`)");
    }
    for (i, e) in hits.iter().enumerate() {
        if i > 0 {
            println!("\n{}", p.dim("─".repeat(60)));
        }
        let state = e.state().map(|s| format!("  [{s}]")).unwrap_or_default();
        println!("{}{}", p.bold(e.key()), p.dim(state));
        if let Some(t) = e.title() {
            println!("{}", p.bold(t));
        }
        if let Some(u) = e.meta.as_ref().and_then(|m| m.url.clone()) {
            println!("{}", p.dim(u));
        }
        if e.born_from_claude() {
            println!("{}", p.c("32", "Born from a Claude session"));
        }
        println!();
        println!("{}", p.c("33", e.story(&sessions)));
        println!();
        for (l, also) in e.grouped() {
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
            match l.session {
                Some(si) => {
                    let s = &sessions[si];
                    println!(
                        "  {}  {}  {}  {}",
                        p.role(l.role, format!("{} {:<9}", l.role.icon(), l.role.verb())),
                        p.c("34", s.short_id()),
                        p.dim(local_time(l.ts.as_deref().or(s.started.as_deref()))),
                        truncate(&s.title(), 60)
                    );
                    println!(
                        "               {}",
                        p.dim(format!(
                            "{} · {}{also}",
                            home_short(&s.resume_dir()),
                            l.evidence
                        ))
                    );
                }
                None => {
                    println!(
                        "  {}  {}  {}",
                        p.role(l.role, format!("{} {:<9}", l.role.icon(), l.role.verb())),
                        p.c("34", "cloud   "),
                        l.cloud_id
                            .as_ref()
                            .map(|c| format!("https://claude.ai/code/{c}"))
                            .unwrap_or_default()
                    );
                    println!("               {}", p.dim(&l.evidence));
                }
            }
        }
        let go_to = e.creators().into_iter().chain(e.reviewers()).find_map(|a| {
            if let Actor::Local(i) = a {
                Some(i)
            } else {
                None
            }
        });
        if let Some(i) = go_to {
            println!();
            println!(
                "Jump in: {}",
                p.bold(format!("ccrumbs go {}", sessions[i].short_id()))
            );
        }
    }
    Ok(())
}

fn describe_cmd(
    p: &Paint,
    db: &Db,
    id: Option<String>,
    limit: usize,
    force: bool,
    model: &str,
) -> Result<()> {
    let sessions = db.load_sessions()?;
    let targets: Vec<&SessionRow> = match &id {
        Some(id) => vec![one(&sessions, id)?],
        None => sessions
            .iter()
            .filter(|s| !s.missing && (force || describe::needs_description(db, s)))
            .take(limit)
            .collect(),
    };
    if targets.is_empty() {
        println!("Every session already has a description.");
        return Ok(());
    }
    describe::check_auth()?;
    eprintln!("Describing {} session(s) with {model}…", targets.len());
    // Network-bound: run a few requests at once, write results serially.
    let inputs: Vec<(&SessionRow, Vec<String>, Option<String>, i64)> = targets
        .iter()
        .filter_map(|s| {
            db.prompts_and_reply(&s.id)
                .ok()
                .map(|(pr, r, z)| (*s, pr, r, z))
        })
        .collect();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
    let results: Vec<_> = pool.install(|| {
        use rayon::prelude::*;
        inputs
            .par_iter()
            .map(|(s, pr, r, z)| (*s, *z, describe::ai_describe(s, pr, r.as_deref(), model)))
            .collect()
    });
    let mut failed = 0;
    for (s, size, r) in results {
        match r {
            Ok((t, d)) => {
                db.set_ai(&s.id, &t, &d, model, size)?;
                println!("{}  {}", p.c("34", s.short_id()), p.bold(&t));
                println!("          {}", p.dim(&d));
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}  {e:#}", s.short_id());
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} descriptions failed", inputs.len());
    }
    Ok(())
}
