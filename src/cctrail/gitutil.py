"""
cctrail.gitutil - git-aware diff/commit lookup for a session's time window.

Best-effort: if `directory` isn't a git repo, or `git` isn't installed,
functions here return empty results rather than raising, so callers can
just check for emptiness.
"""

import shutil
import subprocess
from datetime import datetime, timedelta


def _git_available() -> bool:
    return shutil.which("git") is not None


def is_git_repo(directory: str) -> bool:
    if not directory or not _git_available():
        return False
    try:
        result = subprocess.run(
            ["git", "-C", directory, "rev-parse", "--is-inside-work-tree"],
            capture_output=True, text=True, timeout=5,
        )
        return result.returncode == 0 and result.stdout.strip() == "true"
    except Exception:
        return False


def _pad_iso(ts: str, minutes: int) -> str:
    """Widen a session start/end timestamp by a small buffer so commits
    made right at the boundary aren't missed."""
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        dt = dt + timedelta(minutes=minutes)
        return dt.isoformat()
    except (ValueError, AttributeError):
        return ts


def commits_in_window(directory: str, start_time: str, end_time: str):
    """Return a list of (sha, author_date, subject) commits made in
    `directory` within [start_time - 2min, end_time + 2min]. Empty list
    if not a git repo, no matching commits, or git isn't available."""
    if not is_git_repo(directory) or not start_time or not end_time:
        return []

    since = _pad_iso(start_time, -2)
    until = _pad_iso(end_time, 2)

    try:
        result = subprocess.run(
            [
                "git", "-C", directory, "log",
                f"--since={since}", f"--until={until}",
                "--pretty=format:%H|%ai|%s",
                "--all",
            ],
            capture_output=True, text=True, timeout=10,
        )
    except Exception:
        return []

    if result.returncode != 0 or not result.stdout.strip():
        return []

    commits = []
    for line in result.stdout.strip().split("\n"):
        parts = line.split("|", 2)
        if len(parts) == 3:
            commits.append(tuple(parts))
    return commits


def diffstat_for_commits(directory: str, commits) -> str:
    """Aggregate diffstat across the given commits (oldest..newest).
    Returns empty string if there's nothing to show."""
    if not commits:
        return ""

    # Git's canonical empty-tree object hash - diffing against it works
    # even when the oldest commit in the window has no parent (e.g. it's
    # the repo's very first commit, where "<sha>^" doesn't resolve).
    EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"

    shas = [c[0] for c in commits]
    try:
        if len(shas) == 1:
            result = subprocess.run(
                ["git", "-C", directory, "show", "--stat", "--format=", shas[0]],
                capture_output=True, text=True, timeout=10,
            )
            return result.stdout.strip() if result.returncode == 0 else ""

        # oldest..newest since git log prints newest-first
        oldest, newest = shas[-1], shas[0]
        result = subprocess.run(
            ["git", "-C", directory, "diff", "--stat", f"{oldest}^..{newest}"],
            capture_output=True, text=True, timeout=10,
        )
        if result.returncode == 0:
            return result.stdout.strip()

        # oldest^ likely doesn't exist (oldest has no parent) - diff
        # against the empty tree instead.
        result = subprocess.run(
            ["git", "-C", directory, "diff", "--stat", f"{EMPTY_TREE}..{newest}"],
            capture_output=True, text=True, timeout=10,
        )
        return result.stdout.strip() if result.returncode == 0 else ""
    except Exception:
        return ""
