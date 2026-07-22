"""
cctrail.viewer - render a full session transcript for humans to read,
for both the CLI (`cctrail view`) and the TUI's transcript screen.
"""

import json
from pathlib import Path


def read_transcript_turns(file_path: str):
    """Parse a .jsonl transcript into a list of {speaker, timestamp, text}
    turns, in order. Returns [] if the file can't be read or parsed -
    same defensive posture as core.parse_transcript, since this format
    is internal to Claude Code and not guaranteed stable."""
    turns = []
    try:
        with open(file_path, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue

                etype = entry.get("type")
                if etype not in ("user", "assistant"):
                    continue
                msg = entry.get("message")
                if not isinstance(msg, dict):
                    continue

                text = _format_message_content(msg.get("content"))
                if not text:
                    continue

                turns.append({
                    "speaker": "You" if etype == "user" else "Claude",
                    "timestamp": entry.get("timestamp", ""),
                    "text": text,
                })
    except OSError:
        return []
    return turns


def _format_message_content(content) -> str:
    if content is None:
        return ""
    if isinstance(content, str):
        return content.strip()

    parts = []
    if isinstance(content, list):
        for block in content:
            if not isinstance(block, dict):
                continue
            btype = block.get("type")
            if btype == "text":
                t = block.get("text", "").strip()
                if t:
                    parts.append(t)
            elif btype == "tool_use":
                name = block.get("name", "?")
                inp = block.get("input", {})
                inp_str = json.dumps(inp)[:200]
                parts.append(f"[used tool: {name}]  {inp_str}")
            elif btype == "tool_result":
                c = block.get("content", "")
                if isinstance(c, list):
                    c = " ".join(b.get("text", "") for b in c if isinstance(b, dict))
                c = str(c).strip()
                if c:
                    parts.append(f"[tool result]  {c[:300]}")
    return "\n".join(parts)


def format_transcript_plain(turns) -> str:
    """Plain-text rendering suitable for terminal stdout."""
    lines = []
    for turn in turns:
        ts = (turn["timestamp"] or "")[:19].replace("T", " ")
        lines.append(f"--- {turn['speaker']}  ({ts}) ---")
        lines.append(turn["text"])
        lines.append("")
    return "\n".join(lines) if lines else "(no readable messages in this transcript)"


def format_transcript_markup(turns) -> str:
    """Rich-markup rendering for the Textual detail/transcript screen."""
    lines = []
    for turn in turns:
        ts = (turn["timestamp"] or "")[:19].replace("T", " ")
        color = "cyan" if turn["speaker"] == "You" else "green"
        lines.append(f"[b {color}]{turn['speaker']}[/b {color}] [dim]{ts}[/dim]")
        # Escape stray brackets so they don't get interpreted as markup
        escaped = turn["text"].replace("[", "\\[")
        lines.append(escaped)
        lines.append("")
    return "\n".join(lines) if lines else "(no readable messages in this transcript)"
