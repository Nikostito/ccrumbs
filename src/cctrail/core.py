"""
cctrail.core - indexing, parsing, and query logic for Claude Code sessions.

This module contains no CLI/argparse code; see cctrail.cli for the
command-line entry point and cctrail.tui for the interactive interface.
Everything here is safe to import standalone (e.g. from the TUI).
"""

import json
import os
import sqlite3
from datetime import datetime, timedelta, timezone
from pathlib import Path

from cctrail.config import load_config, is_ignored

CLAUDE_PROJECTS_DIR = Path.home() / ".claude" / "projects"
DB_PATH = Path.home() / ".cctrail" / "sessions.db"

# Candidate top-level field names that might record an explicit link back
# to a prior session (e.g. when Claude Code's /resume creates a branch
# rather than continuing the same file). Speculative - not confirmed
# against a real transcript, so this is best-effort: if none of these
# are present, lineage falls back to a time-adjacency heuristic instead.
RESUME_LINK_FIELDS = (
    "parentSessionId",
    "resumedFromSessionId",
    "resumeFromSessionId",
    "sourceSessionId",
    "originSessionId",
)


def effective_projects_dir() -> Path:
    cfg = load_config()
    override = cfg.get("claude_projects_dir")
    return Path(override).expanduser() if override else CLAUDE_PROJECTS_DIR


# --------------------------------------------------------------------------
# DB setup
# --------------------------------------------------------------------------

# (column_name, sql_type) - anything not in the original CREATE TABLE gets
# added via ALTER TABLE on open, so upgrading cctrail never requires
# wiping an existing index.
SCHEMA_COLUMNS = [
    ("uuid", "TEXT PRIMARY KEY"),
    ("project", "TEXT"),
    ("cwd", "TEXT"),
    ("file_path", "TEXT"),
    ("file_mtime", "REAL"),
    ("start_time", "TEXT"),
    ("end_time", "TEXT"),
    ("duration_seconds", "REAL"),
    ("message_count", "INTEGER"),
    ("user_message_count", "INTEGER"),
    ("first_prompt", "TEXT"),
    ("files_touched", "TEXT"),
    ("indexed_at", "TEXT"),
    ("resumed_from", "TEXT"),
    ("input_tokens", "INTEGER"),
    ("output_tokens", "INTEGER"),
    ("cache_creation_tokens", "INTEGER"),
    ("cache_read_tokens", "INTEGER"),
    ("tags", "TEXT"),
    ("note", "TEXT"),
]


def get_db():
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(DB_PATH))

    cols_sql = ",\n            ".join(f"{name} {sqltype}" for name, sqltype in SCHEMA_COLUMNS)
    conn.execute(f"CREATE TABLE IF NOT EXISTS sessions (\n            {cols_sql}\n        )")

    conn.execute("""
        CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
            uuid UNINDEXED,
            content
        )
    """)

    # Migration: add any columns that didn't exist in older DBs.
    existing_cols = {row[1] for row in conn.execute("PRAGMA table_info(sessions)")}
    for name, sqltype in SCHEMA_COLUMNS:
        if name not in existing_cols:
            conn.execute(f"ALTER TABLE sessions ADD COLUMN {name} {sqltype.replace(' PRIMARY KEY', '')}")

    conn.commit()
    return conn


# --------------------------------------------------------------------------
# Parsing transcripts
# --------------------------------------------------------------------------

def decode_project_dir(dirname: str) -> str:
    """Best-effort reversal of the '/' -> '-' encoding Claude Code uses."""
    if dirname.startswith("-"):
        return "/" + dirname[1:].replace("-", "/")
    return dirname.replace("-", "/")


def extract_text_from_message(msg) -> str:
    """Pull plain text out of a message's content, which may be a string
    or a list of content blocks (text / tool_use / tool_result / thinking)."""
    if msg is None:
        return ""
    content = msg.get("content") if isinstance(msg, dict) else None
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    parts = []
    if isinstance(content, list):
        for block in content:
            if not isinstance(block, dict):
                continue
            btype = block.get("type")
            if btype == "text":
                parts.append(block.get("text", ""))
            elif btype == "tool_use":
                parts.append(f"[tool:{block.get('name','?')}] {json.dumps(block.get('input', {}))[:300]}")
            elif btype == "tool_result":
                c = block.get("content", "")
                if isinstance(c, list):
                    c = " ".join(b.get("text", "") for b in c if isinstance(b, dict))
                parts.append(str(c)[:500])
    return "\n".join(parts)


def parse_transcript(path: Path):
    """Parse one .jsonl transcript. Returns a dict of session metadata or
    None if the file couldn't be meaningfully parsed (defensive against
    format drift, since this format is internal/undocumented-stable)."""
    lines_data = []
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    lines_data.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError:
        return None

    if not lines_data:
        return None

    uuid = path.stem
    timestamps = []
    user_msgs = 0
    total_msgs = 0
    first_prompt = None
    files_touched = set()
    fts_chunks = []
    cwd_counter = {}
    resumed_from = None
    input_tokens = 0
    output_tokens = 0
    cache_creation_tokens = 0
    cache_read_tokens = 0

    for entry in lines_data:
        ts = entry.get("timestamp")
        if ts:
            timestamps.append(ts)

        # Claude Code records the working directory on most entry types
        # (top-level "cwd" field). Tally occurrences so an overwhelming
        # majority wins if the directory ever changed mid-session.
        entry_cwd = entry.get("cwd")
        if isinstance(entry_cwd, str) and entry_cwd:
            cwd_counter[entry_cwd] = cwd_counter.get(entry_cwd, 0) + 1

        if resumed_from is None:
            for field in RESUME_LINK_FIELDS:
                val = entry.get(field)
                if isinstance(val, str) and val:
                    resumed_from = val
                    break

        etype = entry.get("type")
        msg = entry.get("message")

        if etype in ("user", "assistant") and isinstance(msg, dict):
            total_msgs += 1
            text = extract_text_from_message(msg)
            if text:
                fts_chunks.append(text)
            if etype == "user":
                user_msgs += 1
                if first_prompt is None:
                    # Skip pure tool-result "user" turns when looking for the
                    # opening prompt
                    content = msg.get("content")
                    if isinstance(content, str) and content.strip():
                        first_prompt = content.strip()[:300]
                    elif isinstance(content, list):
                        for block in content:
                            if isinstance(block, dict) and block.get("type") == "text":
                                first_prompt = block.get("text", "").strip()[:300]
                                break

            if etype == "assistant":
                usage = msg.get("usage")
                if isinstance(usage, dict):
                    input_tokens += int(usage.get("input_tokens") or 0)
                    output_tokens += int(usage.get("output_tokens") or 0)
                    cache_creation_tokens += int(usage.get("cache_creation_input_tokens") or 0)
                    cache_read_tokens += int(usage.get("cache_read_input_tokens") or 0)

            # Try to pick up file paths from tool_use inputs (Read/Write/Edit)
            content = msg.get("content")
            if isinstance(content, list):
                for block in content:
                    if isinstance(block, dict) and block.get("type") == "tool_use":
                        inp = block.get("input", {})
                        if isinstance(inp, dict):
                            fp = inp.get("file_path") or inp.get("path")
                            if fp:
                                files_touched.add(fp)

    if total_msgs == 0:
        return None

    timestamps.sort()
    start_time = timestamps[0] if timestamps else None
    end_time = timestamps[-1] if timestamps else None
    duration = None
    if start_time and end_time:
        try:
            t0 = datetime.fromisoformat(start_time.replace("Z", "+00:00"))
            t1 = datetime.fromisoformat(end_time.replace("Z", "+00:00"))
            duration = (t1 - t0).total_seconds()
        except ValueError:
            pass

    project_dir = path.parent.name
    project = decode_project_dir(project_dir)

    # The exact cwd (if the transcript recorded one) is authoritative and
    # not subject to the '-' vs '/' ambiguity that decode_project_dir has
    # to guess around. Fall back to the decoded folder name if no
    # transcript in this Claude Code version records "cwd".
    cwd = max(cwd_counter, key=cwd_counter.get) if cwd_counter else None

    return {
        "uuid": uuid,
        "project": project,
        "cwd": cwd,
        "file_path": str(path),
        "file_mtime": path.stat().st_mtime,
        "start_time": start_time,
        "end_time": end_time,
        "duration_seconds": duration,
        "message_count": total_msgs,
        "user_message_count": user_msgs,
        "first_prompt": first_prompt or "",
        "files_touched": json.dumps(sorted(files_touched)),
        "fts_content": "\n".join(fts_chunks),
        "resumed_from": resumed_from,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_tokens": cache_creation_tokens,
        "cache_read_tokens": cache_read_tokens,
    }


# --------------------------------------------------------------------------
# Indexing
# --------------------------------------------------------------------------

def perform_index():
    """Scan and (re)index all transcripts. Returns (new, updated, skipped,
    ignored) counts. No printing - cmd_index and the watch loop both wrap
    this and decide what to show."""
    projects_dir = effective_projects_dir()
    if not projects_dir.exists():
        return None

    cfg = load_config()
    conn = get_db()
    cur = conn.cursor()

    jsonl_files = list(projects_dir.rglob("*.jsonl"))

    new_count = 0
    updated_count = 0
    skipped_count = 0
    ignored_count = 0

    for path in jsonl_files:
        project_dir_name = path.parent.name
        decoded = decode_project_dir(project_dir_name)
        if is_ignored(decoded, cfg):
            ignored_count += 1
            continue

        uuid = path.stem
        mtime = path.stat().st_mtime

        row = cur.execute(
            "SELECT file_mtime FROM sessions WHERE uuid = ?", (uuid,)
        ).fetchone()

        if row is not None and row[0] == mtime:
            skipped_count += 1
            continue

        parsed = parse_transcript(path)
        if parsed is None:
            continue

        is_update = row is not None

        cur.execute("""
            INSERT INTO sessions
                (uuid, project, cwd, file_path, file_mtime, start_time, end_time,
                 duration_seconds, message_count, user_message_count,
                 first_prompt, files_touched, indexed_at, resumed_from,
                 input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(uuid) DO UPDATE SET
                project=excluded.project,
                cwd=excluded.cwd,
                file_path=excluded.file_path,
                file_mtime=excluded.file_mtime,
                start_time=excluded.start_time,
                end_time=excluded.end_time,
                duration_seconds=excluded.duration_seconds,
                message_count=excluded.message_count,
                user_message_count=excluded.user_message_count,
                first_prompt=excluded.first_prompt,
                files_touched=excluded.files_touched,
                indexed_at=excluded.indexed_at,
                resumed_from=excluded.resumed_from,
                input_tokens=excluded.input_tokens,
                output_tokens=excluded.output_tokens,
                cache_creation_tokens=excluded.cache_creation_tokens,
                cache_read_tokens=excluded.cache_read_tokens
        """, (
            parsed["uuid"], parsed["project"], parsed["cwd"], parsed["file_path"], parsed["file_mtime"],
            parsed["start_time"], parsed["end_time"], parsed["duration_seconds"],
            parsed["message_count"], parsed["user_message_count"], parsed["first_prompt"],
            parsed["files_touched"], datetime.now(timezone.utc).isoformat(), parsed["resumed_from"],
            parsed["input_tokens"], parsed["output_tokens"],
            parsed["cache_creation_tokens"], parsed["cache_read_tokens"],
        ))

        cur.execute("DELETE FROM session_fts WHERE uuid = ?", (uuid,))
        cur.execute(
            "INSERT INTO session_fts (uuid, content) VALUES (?, ?)",
            (uuid, parsed["fts_content"])
        )

        if is_update:
            updated_count += 1
        else:
            new_count += 1

    conn.commit()
    conn.close()
    return new_count, updated_count, skipped_count, ignored_count


def cmd_index(args):
    result = perform_index()
    if result is None:
        print(f"No Claude Code projects directory found at {effective_projects_dir()}")
        return 1
    new_count, updated_count, skipped_count, ignored_count = result
    msg = f"Indexed: {new_count} new, {updated_count} updated, {skipped_count} unchanged (skipped)"
    if ignored_count:
        msg += f", {ignored_count} skipped (ignored project)"
    print(msg)
    return 0


# --------------------------------------------------------------------------
# Stats
# --------------------------------------------------------------------------

def cmd_stats(args):
    conn = get_db()
    cur = conn.cursor()

    total = cur.execute("SELECT COUNT(*) FROM sessions").fetchone()[0]
    if total == 0:
        print("No sessions indexed yet. Run `cctrail index` first.")
        return 0

    total_duration = cur.execute(
        "SELECT SUM(duration_seconds) FROM sessions WHERE duration_seconds IS NOT NULL"
    ).fetchone()[0] or 0
    avg_duration = cur.execute(
        "SELECT AVG(duration_seconds) FROM sessions WHERE duration_seconds IS NOT NULL"
    ).fetchone()[0] or 0
    total_messages = cur.execute("SELECT SUM(message_count) FROM sessions").fetchone()[0] or 0

    print("=== Claude Code Session Stats ===\n")
    print(f"Total sessions:        {total}")
    print(f"Total messages:        {total_messages}")
    print(f"Total time tracked:    {fmt_duration(total_duration)}")
    print(f"Average session:       {fmt_duration(avg_duration)}")

    print("\n--- By project ---")
    rows = cur.execute("""
        SELECT project, COUNT(*) as cnt, SUM(duration_seconds) as dur
        FROM sessions GROUP BY project ORDER BY cnt DESC LIMIT 15
    """).fetchall()
    for project, cnt, dur in rows:
        print(f"  {cnt:>4}  {fmt_duration(dur or 0):>10}   {project}")

    print("\n--- By day of week ---")
    rows = cur.execute("""
        SELECT strftime('%w', start_time) as dow, COUNT(*) as cnt
        FROM sessions WHERE start_time IS NOT NULL
        GROUP BY dow ORDER BY dow
    """).fetchall()
    day_names = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
    for dow, cnt in rows:
        if dow is not None:
            print(f"  {day_names[int(dow)]:<4} {'#' * cnt} ({cnt})")

    print("\n--- By hour of day (start time) ---")
    rows = cur.execute("""
        SELECT strftime('%H', start_time) as hr, COUNT(*) as cnt
        FROM sessions WHERE start_time IS NOT NULL
        GROUP BY hr ORDER BY hr
    """).fetchall()
    for hr, cnt in rows:
        if hr is not None:
            print(f"  {hr}:00  {'#' * cnt} ({cnt})")

    token_row = cur.execute("""
        SELECT SUM(input_tokens), SUM(output_tokens),
               SUM(cache_creation_tokens), SUM(cache_read_tokens)
        FROM sessions
    """).fetchone()
    total_input, total_output, total_cache_creation, total_cache_read = (
        (v or 0) for v in token_row
    )
    if total_input or total_output:
        print("\n--- Token usage (sessions that recorded it) ---")
        print(f"  Input:           {total_input:>12,}")
        print(f"  Output:          {total_output:>12,}")
        print(f"  Cache creation:  {total_cache_creation:>12,}")
        print(f"  Cache read:      {total_cache_read:>12,}")
        print("  (see docs.claude.com for current per-token pricing)")

    print("\n--- Most recent sessions ---")
    rows = cur.execute("""
        SELECT uuid, project, start_time, message_count
        FROM sessions ORDER BY start_time DESC LIMIT 5
    """).fetchall()
    for uuid, project, start_time, msg_count in rows:
        print(f"  {uuid[:8]}...  {start_time}  {project}  ({msg_count} msgs)")

    conn.close()
    return 0


def fmt_duration(seconds: float) -> str:
    seconds = seconds or 0
    hours = int(seconds // 3600)
    minutes = int((seconds % 3600) // 60)
    if hours > 0:
        return f"{hours}h {minutes}m"
    return f"{minutes}m"


# --------------------------------------------------------------------------
# Search
# --------------------------------------------------------------------------

def cmd_search(args):
    query = " ".join(args.keywords)
    if not query.strip():
        print("Provide one or more keywords to search for.")
        return 1

    conn = get_db()
    cur = conn.cursor()

    fts_query = " ".join(f'"{term}"' for term in args.keywords)

    sql = """
        SELECT s.uuid, s.project, s.start_time, s.first_prompt,
               snippet(session_fts, 1, '[', ']', '...', 12) as snip
        FROM session_fts
        JOIN sessions s ON s.uuid = session_fts.uuid
        WHERE session_fts MATCH ?
    """
    params = [fts_query]
    if args.project:
        sql += " AND s.project LIKE ?"
        params.append(f"%{args.project}%")
    tag = getattr(args, "tag", None)
    if tag:
        sql += " AND (',' || s.tags || ',') LIKE ?"
        params.append(f"%,{tag},%")
    sql += " ORDER BY rank LIMIT ?"
    params.append(args.limit)

    rows = cur.execute(sql, params).fetchall()

    if not rows:
        scope = f" in project matching '{args.project}'" if args.project else ""
        print(f"No sessions found matching: {query}{scope}")
        return 0

    print(f"Found {len(rows)} matching session(s):\n")
    for uuid, project, start_time, first_prompt, snip in rows:
        print(f"UUID:    {uuid}")
        print(f"Project: {project}")
        print(f"Date:    {start_time}")
        print(f"Prompt:  {first_prompt}")
        print(f"Match:   {snip.strip()}")
        print("-" * 60)

    conn.close()
    return 0


# --------------------------------------------------------------------------
# Show
# --------------------------------------------------------------------------

def get_session_by_uuid_or_prefix(uuid_or_prefix: str):
    """Look up one session by exact uuid or unique prefix. Returns a dict
    of columns, or None if not found."""
    conn = get_db()
    cur = conn.cursor()
    row = cur.execute(
        "SELECT * FROM sessions WHERE uuid = ? OR uuid LIKE ?",
        (uuid_or_prefix, uuid_or_prefix + "%")
    ).fetchone()
    if row is None:
        conn.close()
        return None
    cols = [d[0] for d in cur.description]
    data = dict(zip(cols, row))
    conn.close()
    return data


def cmd_show(args):
    data = get_session_by_uuid_or_prefix(args.uuid)

    if data is None:
        print(f"No session found matching '{args.uuid}'")
        return 1

    print(f"UUID:          {data['uuid']}")
    print(f"Project:       {data['project']}")
    print(f"Directory:     {get_resume_directory(data) or '(unknown)'}")
    print(f"Start:         {data['start_time']}")
    print(f"End:           {data['end_time']}")
    print(f"Duration:      {fmt_duration(data['duration_seconds'] or 0)}")
    print(f"Messages:      {data['message_count']} ({data['user_message_count']} from you)")
    if data.get("input_tokens") or data.get("output_tokens"):
        print(f"Tokens:        in={data['input_tokens']:,} out={data['output_tokens']:,} "
              f"cache_creation={data['cache_creation_tokens']:,} cache_read={data['cache_read_tokens']:,}")
    tags = (data.get("tags") or "").strip(",")
    if tags:
        print(f"Tags:          {tags.replace(',', ', ')}")
    if data.get("note"):
        print(f"Note:          {data['note']}")
    print(f"First prompt:  {data['first_prompt']}")
    files = json.loads(data['files_touched'] or "[]")
    if files:
        print(f"Files touched ({len(files)}):")
        for f in files[:20]:
            print(f"  - {f}")
        if len(files) > 20:
            print(f"  ... and {len(files) - 20} more")
    print(f"Transcript:    {data['file_path']}")
    print(f"Resume:        claude --resume {data['uuid']}")

    return 0


def get_resume_directory(data: dict):
    """The directory to `cd` into before resuming. Prefers the exact `cwd`
    recorded in the transcript (not always present, depending on Claude
    Code version); falls back to the decoded project folder name, which
    can be ambiguous if the real path contained a literal '-'."""
    return data.get("cwd") or data.get("project")


# --------------------------------------------------------------------------
# Projects
# --------------------------------------------------------------------------

def list_projects():
    """Return (project, session_count, last_start_time) for every distinct
    project, most recently active first."""
    conn = get_db()
    cur = conn.cursor()
    rows = cur.execute("""
        SELECT project, COUNT(*) as cnt, MAX(start_time) as last_seen
        FROM sessions
        GROUP BY project
        ORDER BY last_seen DESC
    """).fetchall()
    conn.close()
    return rows


def cmd_projects(args):
    rows = list_projects()
    if not rows:
        print("No sessions indexed yet. Run `cctrail index` first.")
        return 0
    print(f"{'SESSIONS':>9}  {'LAST ACTIVE':<20}  PROJECT")
    for project, cnt, last_seen in rows:
        last = (last_seen or "?")[:16].replace("T", " ")
        print(f"{cnt:>9}  {last:<20}  {project}")
    return 0


# --------------------------------------------------------------------------
# List
# --------------------------------------------------------------------------

def cmd_list(args):
    conn = get_db()
    cur = conn.cursor()

    query = "SELECT uuid, project, start_time, message_count, first_prompt FROM sessions"
    clauses = []
    params = []
    if args.project:
        clauses.append("project LIKE ?")
        params.append(f"%{args.project}%")
    tag = getattr(args, "tag", None)
    if tag:
        clauses.append("(',' || tags || ',') LIKE ?")
        params.append(f"%,{tag},%")
    if clauses:
        query += " WHERE " + " AND ".join(clauses)
    query += " ORDER BY start_time DESC LIMIT ?"
    params.append(args.limit)

    rows = cur.execute(query, params).fetchall()
    if not rows:
        print("No sessions found.")
        return 0

    for uuid, project, start_time, msg_count, first_prompt in rows:
        prompt_preview = (first_prompt or "")[:60]
        print(f"{uuid[:8]}...  {start_time}  {project:<30}  {msg_count:>4} msgs  {prompt_preview}")

    conn.close()
    return 0


# --------------------------------------------------------------------------
# Tags & notes
# --------------------------------------------------------------------------

def _get_tags_set(data: dict) -> set:
    raw = (data.get("tags") or "").strip(",")
    return {t for t in raw.split(",") if t} if raw else set()


def add_tags(uuid_or_prefix: str, new_tags: list):
    data = get_session_by_uuid_or_prefix(uuid_or_prefix)
    if data is None:
        return None
    tags = _get_tags_set(data)
    tags.update(t.strip().lower() for t in new_tags if t.strip())
    joined = "," + ",".join(sorted(tags)) + "," if tags else None
    conn = get_db()
    conn.execute("UPDATE sessions SET tags = ? WHERE uuid = ?", (joined, data["uuid"]))
    conn.commit()
    conn.close()
    return sorted(tags)


def remove_tag(uuid_or_prefix: str, tag: str):
    data = get_session_by_uuid_or_prefix(uuid_or_prefix)
    if data is None:
        return None
    tags = _get_tags_set(data)
    tags.discard(tag.strip().lower())
    joined = "," + ",".join(sorted(tags)) + "," if tags else None
    conn = get_db()
    conn.execute("UPDATE sessions SET tags = ? WHERE uuid = ?", (joined, data["uuid"]))
    conn.commit()
    conn.close()
    return sorted(tags)


def set_note(uuid_or_prefix: str, note: str):
    data = get_session_by_uuid_or_prefix(uuid_or_prefix)
    if data is None:
        return False
    conn = get_db()
    conn.execute("UPDATE sessions SET note = ? WHERE uuid = ?", (note or None, data["uuid"]))
    conn.commit()
    conn.close()
    return True


def cmd_tag(args):
    tags = add_tags(args.uuid, args.tags)
    if tags is None:
        print(f"No session found matching '{args.uuid}'")
        return 1
    print(f"Tags: {', '.join(tags) if tags else '(none)'}")
    return 0


def cmd_untag(args):
    tags = remove_tag(args.uuid, args.tag)
    if tags is None:
        print(f"No session found matching '{args.uuid}'")
        return 1
    print(f"Tags: {', '.join(tags) if tags else '(none)'}")
    return 0


def cmd_note(args):
    ok = set_note(args.uuid, args.text)
    if not ok:
        print(f"No session found matching '{args.uuid}'")
        return 1
    print("Note updated." if args.text else "Note cleared.")
    return 0


# --------------------------------------------------------------------------
# Lineage (resume chains)
# --------------------------------------------------------------------------

HEURISTIC_GAP_HOURS = 4


def get_lineage(uuid_or_prefix: str):
    """Return a dict describing this session's place in a resume chain:
      {"mode": "explicit"|"heuristic"|"none", "before": [...], "after": [...]}
    "explicit" means we found a recorded parent-session field; "heuristic"
    means we're guessing from same-directory + time-adjacency (labelled
    as such, never asserted as fact); "none" means neither found anything.
    """
    data = get_session_by_uuid_or_prefix(uuid_or_prefix)
    if data is None:
        return None

    conn = get_db()
    cur = conn.cursor()

    if data.get("resumed_from"):
        chain_before = []
        cursor = data
        seen = {cursor["uuid"]}
        while cursor.get("resumed_from"):
            parent_row = cur.execute(
                "SELECT * FROM sessions WHERE uuid = ?", (cursor["resumed_from"],)
            ).fetchone()
            if parent_row is None:
                break
            cols = [d[0] for d in cur.description]
            parent = dict(zip(cols, parent_row))
            if parent["uuid"] in seen:
                break
            chain_before.insert(0, parent)
            seen.add(parent["uuid"])
            cursor = parent

        children = cur.execute(
            "SELECT * FROM sessions WHERE resumed_from = ?", (data["uuid"],)
        ).fetchall()
        cols = [d[0] for d in cur.description]
        chain_after = [dict(zip(cols, r)) for r in children]

        conn.close()
        return {"mode": "explicit", "before": chain_before, "after": chain_after}

    # Heuristic fallback: same directory (cwd, or project if cwd unknown),
    # ordered by time, looking only at the immediate neighbor on each side.
    directory = get_resume_directory(data)
    if not directory or not data.get("start_time"):
        conn.close()
        return {"mode": "none", "before": [], "after": []}

    same_dir_rows = cur.execute("""
        SELECT * FROM sessions
        WHERE (cwd = ? OR (cwd IS NULL AND project = ?))
        ORDER BY start_time
    """, (directory, directory)).fetchall()
    cols = [d[0] for d in cur.description]
    same_dir = [dict(zip(cols, r)) for r in same_dir_rows]
    conn.close()

    idx = next((i for i, s in enumerate(same_dir) if s["uuid"] == data["uuid"]), None)
    if idx is None:
        return {"mode": "none", "before": [], "after": []}

    before, after = [], []
    if idx > 0:
        prev = same_dir[idx - 1]
        gap = _gap_hours(prev.get("end_time"), data.get("start_time"))
        if gap is not None and gap <= HEURISTIC_GAP_HOURS:
            prev["_gap_hours"] = gap
            before = [prev]
    if idx < len(same_dir) - 1:
        nxt = same_dir[idx + 1]
        gap = _gap_hours(data.get("end_time"), nxt.get("start_time"))
        if gap is not None and gap <= HEURISTIC_GAP_HOURS:
            nxt["_gap_hours"] = gap
            after = [nxt]

    mode = "heuristic" if (before or after) else "none"
    return {"mode": mode, "before": before, "after": after}


def _gap_hours(earlier_iso, later_iso):
    if not earlier_iso or not later_iso:
        return None
    try:
        t0 = datetime.fromisoformat(earlier_iso.replace("Z", "+00:00"))
        t1 = datetime.fromisoformat(later_iso.replace("Z", "+00:00"))
        return max(0.0, (t1 - t0).total_seconds() / 3600)
    except ValueError:
        return None


def cmd_lineage(args):
    lineage = get_lineage(args.uuid)
    if lineage is None:
        print(f"No session found matching '{args.uuid}'")
        return 1

    data = get_session_by_uuid_or_prefix(args.uuid)

    if lineage["mode"] == "none":
        print("No related sessions found (no recorded link, and no adjacent "
              "session in the same directory within the heuristic time window).")
        return 0

    label = "explicitly recorded" if lineage["mode"] == "explicit" else "heuristic (same directory, time-adjacent)"
    print(f"Lineage for {data['uuid']} [{label}]:\n")

    for s in lineage["before"]:
        gap_note = f"  (gap: {s['_gap_hours']:.1f}h)" if "_gap_hours" in s else ""
        print(f"  BEFORE  {s['uuid'][:8]}...  {s['start_time']}  {(s['first_prompt'] or '')[:50]}{gap_note}")
    print(f"  >>>>>>  {data['uuid'][:8]}...  {data['start_time']}  {(data['first_prompt'] or '')[:50]}  (this session)")
    for s in lineage["after"]:
        gap_note = f"  (gap: {s['_gap_hours']:.1f}h)" if "_gap_hours" in s else ""
        print(f"  AFTER   {s['uuid'][:8]}...  {s['start_time']}  {(s['first_prompt'] or '')[:50]}{gap_note}")

    return 0


# --------------------------------------------------------------------------
# Prune / forget
# --------------------------------------------------------------------------

def cmd_prune(args):
    """Remove indexed sessions whose transcript file no longer exists on
    disk (e.g. purged by Claude Code's own retention settings)."""
    conn = get_db()
    cur = conn.cursor()
    rows = cur.execute("SELECT uuid, file_path FROM sessions").fetchall()

    removed = 0
    for uuid, file_path in rows:
        if not file_path or not os.path.exists(file_path):
            cur.execute("DELETE FROM sessions WHERE uuid = ?", (uuid,))
            cur.execute("DELETE FROM session_fts WHERE uuid = ?", (uuid,))
            removed += 1

    conn.commit()
    conn.close()
    print(f"Pruned {removed} session(s) whose transcript file no longer exists.")
    return 0


def cmd_forget(args):
    """Remove one session from the local index only (the original
    transcript on disk is untouched) - e.g. to exclude something
    sensitive from your locally searchable database."""
    data = get_session_by_uuid_or_prefix(args.uuid)
    if data is None:
        print(f"No session found matching '{args.uuid}'")
        return 1
    conn = get_db()
    conn.execute("DELETE FROM sessions WHERE uuid = ?", (data["uuid"],))
    conn.execute("DELETE FROM session_fts WHERE uuid = ?", (data["uuid"],))
    conn.commit()
    conn.close()
    print(f"Removed {data['uuid']} from the local index (transcript file untouched).")
    return 0


# --------------------------------------------------------------------------
# Config / ignore list
# --------------------------------------------------------------------------

def cmd_config(args):
    cfg = load_config()
    from cctrail.config import CONFIG_PATH
    print(f"Config file: {CONFIG_PATH}")
    print(f"Projects dir: {effective_projects_dir()}"
          f"{' (override)' if cfg.get('claude_projects_dir') else ' (default)'}")
    ignored = cfg.get("ignored_projects") or []
    if ignored:
        print("Ignored project substrings:")
        for p in ignored:
            print(f"  - {p}")
    else:
        print("Ignored project substrings: (none)")
    return 0


def cmd_ignore(args):
    from cctrail.config import save_config
    cfg = load_config()
    ignored = cfg.setdefault("ignored_projects", [])
    if args.substring not in ignored:
        ignored.append(args.substring)
        save_config(cfg)
        print(f"Added '{args.substring}' to the ignore list. "
              f"Sessions matching it won't be indexed on the next `cctrail index`.")
    else:
        print(f"'{args.substring}' is already ignored.")
    return 0


def cmd_unignore(args):
    from cctrail.config import save_config
    cfg = load_config()
    ignored = cfg.setdefault("ignored_projects", [])
    if args.substring in ignored:
        ignored.remove(args.substring)
        save_config(cfg)
        print(f"Removed '{args.substring}' from the ignore list.")
    else:
        print(f"'{args.substring}' wasn't on the ignore list.")
    return 0
