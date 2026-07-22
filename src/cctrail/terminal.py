"""
cctrail.terminal - clipboard access and spawning a new terminal tab/window
to resume a Claude Code session.

Everything here is best-effort: terminal emulators and clipboard mechanisms
vary a lot across platforms, so each function returns (success, message)
rather than raising, and callers should surface the message to the user
either way.
"""

import os
import platform
import shlex
import shutil
import subprocess


def build_resume_command(uuid: str) -> str:
    return f"claude --resume {uuid}"


def system_clipboard_copy(text: str) -> bool:
    """Try OS-level clipboard tools directly (pbcopy / xclip / xsel /
    wl-copy / clip). Returns True on the first one that succeeds."""
    system = platform.system()
    try:
        if system == "Darwin":
            subprocess.run(["pbcopy"], input=text.encode(), check=True)
            return True
        elif system == "Linux":
            for cmd in (
                ["wl-copy"],
                ["xclip", "-selection", "clipboard"],
                ["xsel", "--clipboard", "--input"],
            ):
                if shutil.which(cmd[0]):
                    subprocess.run(cmd, input=text.encode(), check=True)
                    return True
            return False
        elif system == "Windows":
            subprocess.run(["clip"], input=text.encode(), check=True, shell=False)
            return True
    except Exception:
        return False
    return False


def open_new_terminal_and_resume(directory: str, uuid: str):
    """Best-effort: open a new terminal tab/window, cd into `directory`,
    and run `claude --resume <uuid>` there.

    Returns (success: bool, message: str).
    """
    command = build_resume_command(uuid)

    if not directory:
        return False, "No directory known for this session."

    # Prefer tmux if we're inside a tmux session, regardless of OS -
    # this is the most reliable option when it applies.
    if os.environ.get("TMUX"):
        try:
            subprocess.run(
                ["tmux", "new-window", "-c", directory, command],
                check=True,
                capture_output=True,
            )
            return True, "Opened a new tmux window."
        except Exception as e:
            return False, f"tmux new-window failed: {e}"

    system = platform.system()
    script_cmd = f"cd {shlex.quote(directory)} && {command}"

    if system == "Darwin":
        term_program = os.environ.get("TERM_PROGRAM", "")
        escaped = script_cmd.replace("\\", "\\\\").replace('"', '\\"')

        if term_program == "iTerm.app":
            osa = f'''
            tell application "iTerm2"
                tell current window
                    create tab with default profile
                    tell current session
                        write text "{escaped}"
                    end tell
                end tell
            end tell
            '''
            try:
                subprocess.run(["osascript", "-e", osa], check=True, capture_output=True)
                return True, "Opened a new iTerm2 tab."
            except Exception as e:
                return False, f"iTerm2 AppleScript failed: {e}"
        else:
            osa = f'''
            tell application "Terminal"
                activate
                do script "{escaped}"
            end tell
            '''
            try:
                subprocess.run(["osascript", "-e", osa], check=True, capture_output=True)
                return True, "Opened a new Terminal.app window."
            except Exception as e:
                return False, f"Terminal.app AppleScript failed: {e}"

    elif system == "Linux":
        wrapped = f"{script_cmd}; exec bash"
        candidates = [
            ["gnome-terminal", "--", "bash", "-c", wrapped],
            ["konsole", "-e", "bash", "-c", wrapped],
            ["xfce4-terminal", "-e", f"bash -c {shlex.quote(wrapped)}"],
            ["xterm", "-e", f"bash -c {shlex.quote(wrapped)}"],
        ]
        for cmd in candidates:
            if shutil.which(cmd[0]):
                try:
                    subprocess.Popen(cmd, start_new_session=True)
                    return True, f"Opened a new {cmd[0]} window."
                except Exception:
                    continue
        return False, (
            "No supported terminal emulator found "
            "(tried gnome-terminal, konsole, xfce4-terminal, xterm)."
        )

    elif system == "Windows":
        if shutil.which("wt"):
            try:
                subprocess.run(
                    ["wt", "-d", directory, "cmd", "/k", command],
                    check=True,
                    capture_output=True,
                )
                return True, "Opened a new Windows Terminal tab."
            except Exception as e:
                return False, f"Windows Terminal launch failed: {e}"
        else:
            try:
                subprocess.Popen(
                    f'start cmd /k "cd /d {directory} && {command}"',
                    shell=True,
                )
                return True, "Opened a new cmd.exe window."
            except Exception as e:
                return False, f"cmd.exe launch failed: {e}"

    return False, f"Unsupported platform: {system}"
