"""
cctrail.config - user-editable configuration (~/.cctrail/config.json).

Kept deliberately small: an override for where to scan for Claude Code
transcripts, and a list of project-path substrings to exclude from
indexing entirely (e.g. for a client project you don't want searchable
even locally).
"""

import json
from pathlib import Path

CONFIG_PATH = Path.home() / ".cctrail" / "config.json"

DEFAULT_CONFIG = {
    "claude_projects_dir": None,   # null = use the default ~/.claude/projects
    "ignored_projects": [],        # substrings; matched against the decoded project path
}


def load_config() -> dict:
    if not CONFIG_PATH.exists():
        return dict(DEFAULT_CONFIG)
    try:
        with open(CONFIG_PATH, "r", encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return dict(DEFAULT_CONFIG)
    merged = dict(DEFAULT_CONFIG)
    merged.update(data or {})
    return merged


def save_config(cfg: dict) -> None:
    CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(CONFIG_PATH, "w", encoding="utf-8") as f:
        json.dump(cfg, f, indent=2)
        f.write("\n")


def is_ignored(project: str, cfg: dict | None = None) -> bool:
    cfg = cfg or load_config()
    if not project:
        return False
    return any(substr in project for substr in cfg.get("ignored_projects", []))
