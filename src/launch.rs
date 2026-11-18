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
        std::io::stdout(),
        "\x1b]52;c;{}\x07",
        encode(text.as_bytes())
    );
    let _ = std::io::stdout().flush();
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

/// Open a new terminal tab/window running the resume command. Returns a status message.
pub fn open_tab(s: &SessionRow) -> Result<String> {
    let dir = s.resume_dir();
    if !Path::new(&dir).is_dir() {
        bail!("directory {dir} no longer exists");
    }
    let run = format!("claude --resume {}", s.id);
    if std::env::var_os("TMUX").is_some() {
        Command::new("tmux")
            .args(["new-window", "-c", &dir, &run])
            .output()?;
        return Ok("opened a new tmux window".into());
    }
    if let Some(z) = std::env::var_os("ZELLIJ") {
        let _ = z;
        Command::new("zellij")
            .args(["action", "new-tab", "--cwd", &dir])
            .output()?;
        Command::new("zellij")
            .args(["action", "write-chars", &format!("{run}\n")])
            .output()?;
        return Ok("opened a new zellij tab".into());
    }
    let script = resume_command(s);
    if cfg!(target_os = "macos") {
        let esc = script.replace('\\', "\\\\").replace('"', "\\\"");
        let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
        let osa = if term == "iTerm.app" {
            format!(
                "tell application \"iTerm2\" to tell current window\ncreate tab with default profile\ntell current session to write text \"{esc}\"\nend tell"
            )
        } else {
            format!("tell application \"Terminal\"\nactivate\ndo script \"{esc}\"\nend tell")
        };
        let out = Command::new("osascript").args(["-e", &osa]).output()?;
        if !out.status.success() {
            bail!(
                "osascript failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        return Ok("opened a new terminal tab".into());
    }
    if cfg!(windows) {
        if which("wt.exe") || which("wt") {
            Command::new("wt")
                .args(["-d", &dir, "cmd", "/k", &run])
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
        return Ok("opened a new cmd window".into());
    }
    let wrapped = format!("{script}; exec $SHELL");
    let candidates: [(&str, Vec<&str>); 6] = [
        (
            "wezterm",
            vec!["cli", "spawn", "--cwd", &dir, "--", "sh", "-c", &wrapped],
        ),
        (
            "kitty",
            vec![
                "@",
                "launch",
                "--type=tab",
                "--cwd",
                &dir,
                "sh",
                "-c",
                &wrapped,
            ],
        ),
        ("gnome-terminal", vec!["--tab", "--", "sh", "-c", &wrapped]),
        ("konsole", vec!["--new-tab", "-e", "sh", "-c", &wrapped]),
        ("xfce4-terminal", vec!["--tab", "-x", "sh", "-c", &wrapped]),
        ("xterm", vec!["-e", "sh", "-c", &wrapped]),
    ];
    for (bin, args) in candidates {
        if which(bin) {
            let ok = Command::new(bin)
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok();
            if ok {
                return Ok(format!("opened a new {bin} tab"));
            }
        }
    }
    bail!(
        "no supported terminal found (tmux, zellij, wezterm, kitty, gnome-terminal, konsole, xfce4-terminal, xterm)"
    )
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
