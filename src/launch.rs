//! Getting back into a session: resume in place, open a new tab, copy, open URLs.

use crate::model::SessionRow;
use anyhow::{Result, bail};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn resume_command(s: &SessionRow) -> String {
    format!(
        "cd {} && claude --resume {}",
        shell_quote(&s.resume_dir()),
        s.id
    )
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Replace this process with `claude --resume <id>` inside the session's directory.
pub fn resume_here(s: &SessionRow) -> Result<()> {
    let dir = s.resume_dir();
    if !Path::new(&dir).is_dir() {
        bail!(
            "the session's directory {dir} no longer exists (resume command: {})",
            resume_command(s)
        );
    }
    eprintln!("→ {}", resume_command(s));
    let mut cmd = Command::new("claude");
    cmd.arg("--resume").arg(&s.id).current_dir(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        bail!("could not start `claude`: {err} (is Claude Code on your PATH?)");
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Copy via OSC 52 (works over SSH in most terminals) and a native clipboard tool.
pub fn copy(text: &str) -> bool {
    use base64_lite::encode;
    let _ = write!(
        std::io::stderr(),
        "\x1b]52;c;{}\x07",
        encode(text.as_bytes())
    );
    let _ = std::io::stderr().flush();
    let tools: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if cfg!(windows) {
        &[&["clip"]]
    } else {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    for t in tools {
        if !which(t[0]) && !cfg!(windows) {
            continue;
        }
        if let Ok(mut child) = Command::new(t[0])
            .args(&t[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    true // OSC 52 was sent; we can't know whether the terminal honoured it.
}

/// Open a new tab in the terminal you're using, running the resume command.
/// Returns a status message saying what happened.
pub fn open_tab(s: &SessionRow) -> Result<String> {
    let dir = s.resume_dir();
    if !Path::new(&dir).is_dir() {
        bail!("directory {dir} no longer exists");
    }
    let run = format!("claude --resume {}", s.id);
    let script = resume_command(s);
    let env = |k: &str| std::env::var_os(k).is_some();
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let ok = |c: &mut Command| -> Result<()> {
        let out = c.output()?;
        if out.status.success() {
            Ok(())
        } else {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
        }
    };

    // Multiplexers first: a tab there is what you'd expect even inside any terminal app.
    if env("TMUX") {
        ok(Command::new("tmux").args(["new-window", "-c", &dir, &run]))?;
        return Ok("opened a new tmux window".into());
    }
    if env("ZELLIJ") {
        ok(Command::new("zellij").args(["action", "new-tab", "--cwd", &dir]))?;
        ok(Command::new("zellij").args(["action", "write-chars", &format!("{run}\n")]))?;
        return Ok("opened a new zellij tab".into());
    }
    let keep_shell = format!("{script}; exec \"${{SHELL:-sh}}\"");
    // Terminals with a CLI for tabs (any OS).
    if env("WEZTERM_PANE") || term == "WezTerm" {
        ok(Command::new("wezterm").args([
            "cli",
            "spawn",
            "--cwd",
            &dir,
            "--",
            "sh",
            "-c",
            &keep_shell,
        ]))?;
        return Ok("opened a new WezTerm tab".into());
    }
    if env("KITTY_WINDOW_ID") {
        return match ok(Command::new("kitty").args([
            "@",
            "launch",
            "--type=tab",
            "--cwd",
            &dir,
            "sh",
            "-c",
            &keep_shell,
        ])) {
            Ok(()) => Ok("opened a new kitty tab".into()),
            Err(e) => bail!("kitty refused ({e}); set `allow_remote_control yes` in kitty.conf"),
        };
    }

    if cfg!(target_os = "macos") {
        let esc = script.replace('\\', "\\\\").replace('"', "\\\"");
        let osa = match term.as_str() {
            "iTerm.app" => format!(
                "tell application \"iTerm2\" to tell current window\ncreate tab with default profile\ntell current session to write text \"{esc}\"\nend tell"
            ),
            // Terminal.app has no "new tab" command: press ⌘T, then run in that tab.
            "Apple_Terminal" => format!(
                "tell application \"Terminal\" to activate\ntell application \"System Events\" to keystroke \"t\" using command down\ndelay 0.3\ntell application \"Terminal\" to do script \"{esc}\" in selected tab of front window"
            ),
            // Ghostty, Warp, Hyper, Tabby, ...: ⌘T opens a tab, then type the command.
            _ => format!(
                "tell application \"System Events\"\nkeystroke \"t\" using command down\ndelay 0.4\nkeystroke \"{esc}\"\nkey code 36\nend tell"
            ),
        };
        let out = Command::new("osascript").args(["-e", &osa]).output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("not allowed")
                || err.contains("1002")
                || err.contains("-1743")
                || err.contains("assistive")
            {
                bail!(
                    "macOS blocked it: allow your terminal under System Settings → Privacy & Security → Accessibility (and Automation), or use ^y to copy the command"
                );
            }
            bail!("osascript failed: {}", err.trim());
        }
        let name = match term.as_str() {
            "iTerm.app" => "iTerm2",
            "Apple_Terminal" => "Terminal",
            "ghostty" => "Ghostty",
            "WarpTerminal" => "Warp",
            "" => "terminal",
            other => other,
        };
        return Ok(format!("opened a new {name} tab"));
    }

    if cfg!(windows) {
        if which("wt.exe") || which("wt") {
            // `-w 0` targets the current Windows Terminal window, so this is a tab, not a window.
            Command::new("wt")
                .args(["-w", "0", "nt", "-d", &dir, "cmd", "/k", &run])
                .spawn()?;
            return Ok("opened a new Windows Terminal tab".into());
        }
        Command::new("cmd")
            .args([
                "/c",
                "start",
                "cmd",
                "/k",
                &format!("cd /d \"{dir}\" && {run}"),
            ])
            .spawn()?;
        return Ok("opened a new cmd window (Windows Terminal not found)".into());
    }

    // Linux: use the terminal we're actually in when it says so.
    let tab_in = |bin: &str, args: Vec<&str>| -> Option<String> {
        (which(bin)
            && Command::new(bin)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok())
        .then(|| format!("opened a new {bin} tab"))
    };
    if env("GNOME_TERMINAL_SCREEN") || env("GNOME_TERMINAL_SERVICE") {
        if let Some(m) = tab_in(
            "gnome-terminal",
            vec![
                "--tab",
                "--working-directory",
                &dir,
                "--",
                "sh",
                "-c",
                &keep_shell,
            ],
        ) {
            return Ok(m);
        }
    }
    if env("KONSOLE_VERSION") {
        if let Some(m) = tab_in(
            "konsole",
            vec![
                "--new-tab",
                "--workdir",
                &dir,
                "-e",
                "sh",
                "-c",
                &keep_shell,
            ],
        ) {
            return Ok(m);
        }
    }
    if term == "xfce4-terminal" || env("XFCE_TERMINAL") {
        if let Some(m) = tab_in(
            "xfce4-terminal",
            vec![
                "--tab",
                "--working-directory",
                &dir,
                "-x",
                "sh",
                "-c",
                &keep_shell,
            ],
        ) {
            return Ok(m);
        }
    }
    bail!(
        "this terminal has no way to open a tab from outside; use ^y to copy the command (tabs work in tmux, zellij, WezTerm, kitty, iTerm2, Terminal, Ghostty, Warp, GNOME Terminal, Konsole, Windows Terminal)"
    )
}

/// An editor cctrail can open a project in.
#[derive(Debug, Clone)]
pub struct Editor {
    pub label: String,
    /// Shell command; the project directory is appended as its last argument.
    pub command: String,
    /// GUI editors start detached; terminal editors take over the screen.
    pub gui: bool,
}

const TERMINAL_EDITORS: &[&str] = &[
    "nvim", "vim", "vi", "hx", "helix", "emacs", "nano", "micro", "kak",
];

fn is_gui(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("");
    let name = Path::new(first)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // `emacs -nw` is the terminal flavour; plain `emacs` opens a window when it can, but treat as terminal.
    !TERMINAL_EDITORS.contains(&name.as_str())
}

/// Editors found on this machine, most common first.
pub fn detect_editors() -> Vec<Editor> {
    let mut out: Vec<Editor> = Vec::new();
    let mut add = |label: &str, command: String| {
        if !out.iter().any(|e| e.command == command || e.label == label) {
            let gui = is_gui(&command);
            out.push(Editor {
                label: label.to_string(),
                command,
                gui,
            });
        }
    };
    let cli = [
        ("VS Code", "code"),
        ("Cursor", "cursor"),
        ("Windsurf", "windsurf"),
        ("Zed", "zed"),
        ("Sublime Text", "subl"),
        ("IntelliJ IDEA", "idea"),
        ("PyCharm", "pycharm"),
        ("WebStorm", "webstorm"),
        ("GoLand", "goland"),
        ("RustRover", "rustrover"),
        ("Fleet", "fleet"),
        ("VSCodium", "codium"),
        ("Neovim", "nvim"),
        ("Vim", "vim"),
        ("Helix", "hx"),
        ("Emacs", "emacs"),
        ("Nano", "nano"),
        ("Micro", "micro"),
    ];
    for (label, bin) in cli {
        if which(bin) {
            add(label, bin.to_string());
        }
    }
    // macOS apps without their command-line launcher installed.
    if cfg!(target_os = "macos") {
        let apps = [
            ("VS Code", "Visual Studio Code"),
            ("Cursor", "Cursor"),
            ("Windsurf", "Windsurf"),
            ("Zed", "Zed"),
            ("Sublime Text", "Sublime Text"),
            ("IntelliJ IDEA", "IntelliJ IDEA"),
            ("IntelliJ IDEA", "IntelliJ IDEA CE"),
            ("PyCharm", "PyCharm"),
            ("PyCharm", "PyCharm CE"),
            ("WebStorm", "WebStorm"),
            ("GoLand", "GoLand"),
            ("RustRover", "RustRover"),
            ("Fleet", "Fleet"),
            ("Nova", "Nova"),
            ("BBEdit", "BBEdit"),
            ("Xcode", "Xcode"),
        ];
        let home_apps = dirs::home_dir().map(|h| h.join("Applications"));
        for (label, app) in apps {
            let found = Path::new("/Applications")
                .join(format!("{app}.app"))
                .exists()
                || home_apps
                    .as_ref()
                    .is_some_and(|h| h.join(format!("{app}.app")).exists());
            if found {
                add(label, format!("open -a \"{app}\""));
            }
        }
    }
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                add(&format!("${var} ({v})"), v);
            }
        }
    }
    out
}

/// Resolve a name typed by the user ("cursor", "pycharm") to a detected editor's command,
/// or take it as a command as-is.
pub fn resolve_editor(input: &str) -> String {
    let i = input.trim();
    let lower = i.to_lowercase();
    detect_editors()
        .into_iter()
        .find(|e| e.label.to_lowercase() == lower || e.command.to_lowercase() == lower)
        .or_else(|| {
            detect_editors()
                .into_iter()
                .find(|e| e.label.to_lowercase().starts_with(&lower))
        })
        .map(|e| e.command)
        .unwrap_or_else(|| i.to_string())
}

/// The editor to use: the one you chose (`editor` in the config), else `$CCTRAIL_EDITOR`.
/// None means "ask".
pub fn chosen_editor(cfg_editor: Option<&str>) -> Option<String> {
    cfg_editor
        .map(str::to_string)
        .or_else(|| std::env::var("CCTRAIL_EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
}

/// Command that opens `dir` with `editor`. The bool is true for GUI editors.
pub fn editor_command(editor: &str, dir: &str) -> Result<(Command, bool)> {
    if !Path::new(dir).is_dir() {
        bail!("directory {dir} no longer exists");
    }
    let gui = is_gui(editor);
    // Let the shell parse the command, so `code -w`, `open -a "Visual Studio Code"` etc. work.
    let mut cmd = if cfg!(unix) {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(dir);
        c
    } else {
        let mut parts = editor.split_whitespace();
        let mut c = Command::new(parts.next().unwrap_or("notepad"));
        c.args(parts).arg(dir);
        c
    };
    cmd.current_dir(dir);
    Ok((cmd, gui))
}

pub fn open_url(url: &str) -> Result<()> {
    let (bin, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/c", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(b: &[u8]) -> String {
        let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16
                | (*c.get(1).unwrap_or(&0) as u32) << 8
                | *c.get(2).unwrap_or(&0) as u32;
            o.push(T[(n >> 18) as usize & 63] as char);
            o.push(T[(n >> 12) as usize & 63] as char);
            o.push(if c.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            o.push(if c.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        o
    }
}
