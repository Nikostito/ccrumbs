"""
cctrail.tui - interactive terminal UI for browsing Claude Code sessions.

A lazygit-style panel layout:
    - top: live filter/search input
    - left: project list (select one to scope the session list to it)
    - middle: session list (filtered by project + search text)
    - right: detail panel for the selected session
    - footer: keybindings

Launched via `cctrail tui`. Requires the `textual` package.
"""

import asyncio
import json

from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Header, Footer, Input, ListView, ListItem, Label, Static, Markdown
from textual import work

from cctrail.core import (
    get_db, fmt_duration, cmd_index, list_projects, get_resume_directory,
    add_tags, remove_tag, set_note, get_lineage,
)
from cctrail.terminal import build_resume_command, system_clipboard_copy, open_new_terminal_and_resume
from cctrail.viewer import read_transcript_turns, format_transcript_markup
from cctrail.gitutil import is_git_repo, commits_in_window, diffstat_for_commits


MAX_ROWS = 300
ALL_PROJECTS_ID = "__all__"


def fetch_sessions(query: str, project: str | None = None):
    """Return session rows matching the current filter text and, if given,
    scoped to an exact project path. Empty query -> most recent sessions.
    Non-empty -> FTS search with prefix wildcards for a live-filter feel,
    falling back to a plain LIKE scan if FTS finds nothing (handles short
    fragments FTS5 tokenizing might miss)."""
    conn = get_db()
    cur = conn.cursor()

    query = query.strip()

    if not query:
        sql = "SELECT uuid, project, start_time, message_count, first_prompt, duration_seconds, tags FROM sessions"
        params = []
        if project:
            sql += " WHERE project = ?"
            params.append(project)
        sql += " ORDER BY start_time DESC LIMIT ?"
        params.append(MAX_ROWS)
        rows = cur.execute(sql, params).fetchall()
        conn.close()
        return rows

    terms = query.split()
    fts_query = " ".join(f'{t}*' for t in terms if t)

    try:
        sql = """
            SELECT s.uuid, s.project, s.start_time, s.message_count, s.first_prompt, s.duration_seconds, s.tags
            FROM session_fts
            JOIN sessions s ON s.uuid = session_fts.uuid
            WHERE session_fts MATCH ?
        """
        params = [fts_query]
        if project:
            sql += " AND s.project = ?"
            params.append(project)
        sql += " ORDER BY rank LIMIT ?"
        params.append(MAX_ROWS)
        rows = cur.execute(sql, params).fetchall()
    except Exception:
        rows = []

    if not rows:
        like = f"%{query}%"
        sql = """
            SELECT uuid, project, start_time, message_count, first_prompt, duration_seconds, tags
            FROM sessions
            WHERE (project LIKE ? OR first_prompt LIKE ?)
        """
        params = [like, like]
        if project:
            sql += " AND project = ?"
            params.append(project)
        sql += " ORDER BY start_time DESC LIMIT ?"
        params.append(MAX_ROWS)
        rows = cur.execute(sql, params).fetchall()

    conn.close()
    return rows


def fetch_session_detail(uuid: str):
    conn = get_db()
    cur = conn.cursor()
    row = cur.execute("SELECT * FROM sessions WHERE uuid = ?", (uuid,)).fetchone()
    if row is None:
        conn.close()
        return None
    cols = [d[0] for d in cur.description]
    conn.close()
    return dict(zip(cols, row))


def fetch_global_stats():
    conn = get_db()
    cur = conn.cursor()
    total = cur.execute("SELECT COUNT(*) FROM sessions").fetchone()[0]
    total_duration = cur.execute(
        "SELECT SUM(duration_seconds) FROM sessions WHERE duration_seconds IS NOT NULL"
    ).fetchone()[0] or 0
    avg_duration = cur.execute(
        "SELECT AVG(duration_seconds) FROM sessions WHERE duration_seconds IS NOT NULL"
    ).fetchone()[0] or 0
    total_messages = cur.execute("SELECT SUM(message_count) FROM sessions").fetchone()[0] or 0
    by_project = cur.execute("""
        SELECT project, COUNT(*) as cnt, SUM(duration_seconds) as dur
        FROM sessions GROUP BY project ORDER BY cnt DESC LIMIT 15
    """).fetchall()
    token_row = cur.execute("""
        SELECT SUM(input_tokens), SUM(output_tokens), SUM(cache_creation_tokens), SUM(cache_read_tokens)
        FROM sessions
    """).fetchone()
    conn.close()
    return {
        "total": total,
        "total_duration": total_duration,
        "avg_duration": avg_duration,
        "total_messages": total_messages,
        "by_project": by_project,
        "tokens": tuple((v or 0) for v in token_row),
    }


def session_row_label(row) -> str:
    uuid, project, start_time, msg_count, first_prompt, duration, tags = row
    short_uuid = uuid[:8]
    date = (start_time or "?")[:16].replace("T", " ")
    prompt = (first_prompt or "").replace("\n", " ")[:35]
    proj_short = project.split("/")[-1] if project else "?"
    tag_marker = "*" if tags else " "
    return f"{tag_marker}{short_uuid}  {date}  [{proj_short:<16}]  {msg_count:>3}m  {prompt}"


def project_row_label(project: str, count: int) -> str:
    short = project.split("/")[-1] if project else "?"
    return f"{count:>4}  {short}"


class StatsScreen(ModalScreen):
    """Overlay screen showing aggregate statistics."""

    BINDINGS = [
        Binding("escape", "dismiss", "Back"),
        Binding("q", "dismiss", "Back"),
    ]

    def compose(self) -> ComposeResult:
        stats = fetch_global_stats()
        lines = [
            "# Session Statistics\n",
            f"**Total sessions:** {stats['total']}",
            f"**Total messages:** {stats['total_messages']}",
            f"**Total time tracked:** {fmt_duration(stats['total_duration'])}",
            f"**Average session length:** {fmt_duration(stats['avg_duration'])}",
        ]
        in_tok, out_tok, cache_c, cache_r = stats["tokens"]
        if in_tok or out_tok:
            lines += [
                "\n## Token usage\n",
                f"- Input: **{in_tok:,}**",
                f"- Output: **{out_tok:,}**",
                f"- Cache creation: **{cache_c:,}**",
                f"- Cache read: **{cache_r:,}**",
            ]
        lines.append("\n## By project\n")
        for project, cnt, dur in stats["by_project"]:
            lines.append(f"- **{cnt}** sessions, {fmt_duration(dur or 0)} — `{project}`")
        lines.append("\n*Press Esc or q to go back.*")
        yield Vertical(Markdown("\n".join(lines)), id="stats-container")

    def action_dismiss(self) -> None:
        self.app.pop_screen()


class TranscriptScreen(ModalScreen):
    """Full conversation viewer for one session."""

    BINDINGS = [
        Binding("escape", "dismiss", "Back"),
        Binding("q", "dismiss", "Back"),
    ]

    def __init__(self, data: dict):
        super().__init__()
        self._data = data

    def compose(self) -> ComposeResult:
        turns = read_transcript_turns(self._data["file_path"])
        header = f"# Transcript: {self._data['uuid'][:8]}...  ({self._data.get('project', '')})\n\n"
        body = format_transcript_markup(turns)
        yield VerticalScroll(Static(header + body, markup=True), id="transcript-container")

    def action_dismiss(self) -> None:
        self.app.pop_screen()


class DiffScreen(ModalScreen):
    """Git commits + diffstat made during a session's time window."""

    BINDINGS = [
        Binding("escape", "dismiss", "Back"),
        Binding("q", "dismiss", "Back"),
    ]

    def __init__(self, data: dict):
        super().__init__()
        self._data = data

    def compose(self) -> ComposeResult:
        yield VerticalScroll(Static("Loading git info…", id="diff-content"), id="diff-container")

    async def on_mount(self) -> None:
        directory = get_resume_directory(self._data)
        content_widget = self.query_one("#diff-content", Static)

        if not directory:
            content_widget.update("No directory known for this session.")
            return

        is_repo = await asyncio.to_thread(is_git_repo, directory)
        if not is_repo:
            content_widget.update(f"'{directory}' is not a git repository (or git isn't installed).")
            return

        commits = await asyncio.to_thread(
            commits_in_window, directory, self._data.get("start_time"), self._data.get("end_time")
        )
        if not commits:
            content_widget.update(
                f"No commits found in {directory} during this session's time window."
            )
            return

        lines = [f"[b]Commits in {directory}:[/b]\n"]
        for sha, author_date, subject in commits:
            escaped_subject = subject.replace("[", "\\[")
            lines.append(f"  {sha[:8]}  {author_date}  {escaped_subject}")

        diffstat = await asyncio.to_thread(diffstat_for_commits, directory, commits)
        if diffstat:
            lines.append("\n[b]Diffstat:[/b]\n")
            lines.append(diffstat.replace("[", "\\["))

        content_widget.update("\n".join(lines))

    def action_dismiss(self) -> None:
        self.app.pop_screen()


class LineageScreen(ModalScreen):
    """Sessions related to this one (explicit resume chain, or a
    time-adjacency heuristic when no explicit link is recorded)."""

    BINDINGS = [
        Binding("escape", "dismiss", "Back"),
        Binding("q", "dismiss", "Back"),
    ]

    def __init__(self, data: dict):
        super().__init__()
        self._data = data

    def compose(self) -> ComposeResult:
        lineage = get_lineage(self._data["uuid"])
        lines = ["# Session lineage\n"]

        if lineage is None or lineage["mode"] == "none":
            lines.append(
                "No related sessions found - no recorded link, and no adjacent "
                "session in the same directory within the heuristic time window."
            )
        else:
            label = "explicitly recorded" if lineage["mode"] == "explicit" else "heuristic (same directory, time-adjacent - not certain)"
            lines.append(f"_{label}_\n")
            for s in lineage["before"]:
                gap = f"  (gap: {s['_gap_hours']:.1f}h)" if "_gap_hours" in s else ""
                lines.append(f"- BEFORE `{s['uuid'][:8]}...`  {s['start_time']}  {(s['first_prompt'] or '')[:50]}{gap}")
            lines.append(f"- **>>> {self._data['uuid'][:8]}...  {self._data['start_time']}  (this session)**")
            for s in lineage["after"]:
                gap = f"  (gap: {s['_gap_hours']:.1f}h)" if "_gap_hours" in s else ""
                lines.append(f"- AFTER  `{s['uuid'][:8]}...`  {s['start_time']}  {(s['first_prompt'] or '')[:50]}{gap}")

        lines.append("\n*Press Esc or q to go back.*")
        yield Vertical(Markdown("\n".join(lines)), id="lineage-container")

    def action_dismiss(self) -> None:
        self.app.pop_screen()


class TextPromptScreen(ModalScreen):
    """Small modal with a single text input - used for tag/note entry.
    Dismisses with the entered string, or None if cancelled."""

    BINDINGS = [
        Binding("escape", "cancel", "Cancel"),
    ]

    def __init__(self, prompt: str, initial: str = ""):
        super().__init__()
        self._prompt = prompt
        self._initial = initial

    def compose(self) -> ComposeResult:
        with Vertical(id="prompt-container"):
            yield Label(self._prompt)
            yield Input(value=self._initial, id="prompt-input")

    def on_mount(self) -> None:
        self.query_one("#prompt-input", Input).focus()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        self.dismiss(event.value)

    def action_cancel(self) -> None:
        self.dismiss(None)


class SessionDetail(Static):
    """Right-hand detail panel."""

    def render_data(self, data: dict | None):
        if data is None:
            self.update("No session selected.")
            return

        files = json.loads(data.get("files_touched") or "[]")
        directory = get_resume_directory(data)
        lines = [
            f"[b]UUID[/b]      {data['uuid']}",
            f"[b]Project[/b]   {data['project']}",
            f"[b]Directory[/b] {directory or '(unknown)'}",
            f"[b]Start[/b]     {data['start_time']}",
            f"[b]End[/b]       {data['end_time']}",
            f"[b]Duration[/b]  {fmt_duration(data['duration_seconds'] or 0)}",
            f"[b]Messages[/b]  {data['message_count']} ({data['user_message_count']} from you)",
        ]
        if data.get("input_tokens") or data.get("output_tokens"):
            lines.append(
                f"[b]Tokens[/b]    in={data['input_tokens']:,} out={data['output_tokens']:,} "
                f"cache_r={data['cache_read_tokens']:,}"
            )
        tags = (data.get("tags") or "").strip(",")
        if tags:
            lines.append(f"[b]Tags[/b]      {tags.replace(',', ', ')}")
        if data.get("note"):
            note_escaped = data["note"].replace("[", "\\[")
            lines.append(f"[b]Note[/b]      {note_escaped}")
        lines += [
            "",
            "[b]First prompt:[/b]",
            data["first_prompt"] or "(none)",
            "",
        ]
        if files:
            lines.append(f"[b]Files touched ({len(files)}):[/b]")
            for f in files[:20]:
                lines.append(f"  - {f}")
            if len(files) > 20:
                lines.append(f"  ... and {len(files) - 20} more")
        lines.append("")
        lines.append(f"[b]Resume:[/b]   {build_resume_command(data['uuid'])}")
        lines.append(f"[dim]{data['file_path']}[/dim]")
        lines.append("")
        lines.append("[dim]Enter: transcript  d: diff  L: lineage[/dim]")
        lines.append("[dim]y: copy UUID  Y: copy resume  o: open+resume[/dim]")
        lines.append("[dim]t: add tag  n: set note[/dim]")
        self.update("\n".join(lines))


class CctrailApp(App):
    CSS = """
    #search-input {
        dock: top;
        margin: 0 1;
    }
    #body {
        height: 1fr;
    }
    #project-list {
        width: 25%;
        border: round $accent;
    }
    #session-list {
        width: 45%;
        border: round $accent;
    }
    #detail-panel {
        width: 30%;
        border: round $accent;
        padding: 1 2;
    }
    #stats-container, #lineage-container {
        padding: 2 4;
        background: $panel;
    }
    #transcript-container, #diff-container {
        padding: 1 2;
        background: $panel;
    }
    #prompt-container {
        padding: 1 2;
        background: $panel;
        border: round $accent;
        width: 60%;
        height: auto;
        margin: 2 4;
    }
    """

    BINDINGS = [
        Binding("q", "quit", "Quit"),
        Binding("slash", "focus_search", "Search"),
        Binding("p", "focus_projects", "Projects"),
        Binding("s", "show_stats", "Stats"),
        Binding("i", "reindex", "Reindex"),
        Binding("y", "copy_uuid", "Copy UUID"),
        Binding("Y", "copy_resume_cmd", "Copy resume cmd"),
        Binding("o", "open_and_resume", "Open & resume"),
        Binding("d", "show_diff", "Diff"),
        Binding("L", "show_lineage", "Lineage"),
        Binding("t", "add_tag", "Tag"),
        Binding("n", "set_note", "Note"),
        Binding("escape", "clear_search", "Clear"),
    ]

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Input(placeholder="Type to filter... (Enter = full-text search, Esc = clear)", id="search-input")
        with Horizontal(id="body"):
            yield ListView(id="project-list")
            yield ListView(id="session-list")
            yield SessionDetail("Select a session to view details.", id="detail-panel")
        yield Footer()

    def on_mount(self) -> None:
        self.title = "cctrail"
        self.sub_title = "Claude Code session browser"
        self._refresh_token = 0
        self._selected_project = None  # None = all projects
        self._selected_data = None     # full row dict for the highlighted session
        self.call_later(self.refresh_projects)
        self.call_later(self.refresh_list, "")
        self.call_later(self.query_one("#session-list", ListView).focus)

    async def refresh_projects(self) -> None:
        project_list = self.query_one("#project-list", ListView)
        await project_list.clear()

        all_item = ListItem(Label("  All projects"), id=f"p-{ALL_PROJECTS_ID}")
        all_item.project_path = None
        await project_list.append(all_item)

        for project, count, _last_seen in list_projects():
            safe_id = "p-" + "".join(c if c.isalnum() else "_" for c in project)
            item = ListItem(Label(project_row_label(project, count)), id=safe_id)
            item.project_path = project
            await project_list.append(item)

        project_list.index = 0

    def _select_session(self, uuid: str | None) -> None:
        """Fetch and store the full row for the given uuid, and refresh
        the detail panel. Central place other actions (copy/open/tag/etc)
        read the currently-selected session from."""
        data = fetch_session_detail(uuid) if uuid else None
        self._selected_data = data
        self.query_one("#detail-panel", SessionDetail).render_data(data)

    async def refresh_list(self, query: str) -> None:
        # Guard against out-of-order results if keystrokes arrive faster
        # than a refresh can complete (each call gets a token; only the
        # most recent one is allowed to touch the widget tree).
        self._refresh_token += 1
        my_token = self._refresh_token

        rows = fetch_sessions(query, project=self._selected_project)

        if my_token != self._refresh_token:
            return  # a newer keystroke superseded this refresh

        list_view = self.query_one("#session-list", ListView)
        await list_view.clear()

        if my_token != self._refresh_token:
            return

        self._current_rows = {row[0]: row for row in rows}
        for row in rows:
            uuid = row[0]
            item = ListItem(Label(session_row_label(row)), id=f"s-{uuid.replace('-', '_')}")
            item.session_uuid = uuid
            await list_view.append(item)
            if my_token != self._refresh_token:
                return

        self._select_session(rows[0][0] if rows else None)
        if rows:
            list_view.index = 0

    async def on_input_changed(self, event: Input.Changed) -> None:
        if event.input.id == "search-input":
            await self.refresh_list(event.value)

    async def on_input_submitted(self, event: Input.Submitted) -> None:
        if event.input.id == "search-input":
            await self.refresh_list(event.value)
            self.query_one("#session-list", ListView).focus()

    async def on_list_view_highlighted(self, event: ListView.Highlighted) -> None:
        item = event.item
        if item is None:
            return
        list_id = event.list_view.id

        if list_id == "project-list" and hasattr(item, "project_path"):
            new_project = item.project_path
            if new_project != self._selected_project:
                self._selected_project = new_project
                current_query = self.query_one("#search-input", Input).value
                await self.refresh_list(current_query)

        elif list_id == "session-list" and hasattr(item, "session_uuid"):
            self._select_session(item.session_uuid)

    def on_list_view_selected(self, event: ListView.Selected) -> None:
        # ListView's own binding claims the "enter" key (-> select_cursor,
        # firing this Selected message) before it can reach an app-level
        # action binding, so we hook the transcript viewer here instead.
        if event.list_view.id == "session-list":
            self.action_view_transcript()

    def action_focus_search(self) -> None:
        self.query_one("#search-input", Input).focus()

    def action_focus_projects(self) -> None:
        self.query_one("#project-list", ListView).focus()

    async def action_clear_search(self) -> None:
        search = self.query_one("#search-input", Input)
        if self.focused is search and search.value:
            search.value = ""
            await self.refresh_list("")
        else:
            self.query_one("#session-list", ListView).focus()

    def action_show_stats(self) -> None:
        self.push_screen(StatsScreen())

    async def action_reindex(self) -> None:
        class Args:
            pass
        cmd_index(Args())
        await self.refresh_projects()
        current_query = self.query_one("#search-input", Input).value
        await self.refresh_list(current_query)
        self.notify("Reindexed.")

    def _copy_text(self, text: str, label: str) -> None:
        """Copy via both the OSC-52 terminal escape (works over SSH, most
        terminals except macOS Terminal.app) and a native clipboard tool
        (pbcopy/xclip/etc, which does cover macOS Terminal.app) for the
        best chance of actually landing in the system clipboard."""
        self.copy_to_clipboard(text)
        system_ok = system_clipboard_copy(text)
        if system_ok:
            self.notify(f"Copied {label} to clipboard.")
        else:
            self.notify(
                f"Copied {label} via terminal escape sequence "
                "(may not work in macOS Terminal.app).",
                severity="warning",
            )

    def action_copy_uuid(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        self._copy_text(self._selected_data["uuid"], "UUID")

    def action_copy_resume_cmd(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        command = build_resume_command(self._selected_data["uuid"])
        self._copy_text(command, "resume command")

    async def action_open_and_resume(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        directory = get_resume_directory(self._selected_data)
        uuid = self._selected_data["uuid"]
        if not directory:
            self.notify("No directory known for this session.", severity="error")
            return

        self.notify("Opening new terminal…")
        # Spawning a terminal shells out (osascript/gnome-terminal/etc),
        # which can briefly block - run it off the event loop thread.
        ok, message = await asyncio.to_thread(open_new_terminal_and_resume, directory, uuid)
        self.notify(message, severity="information" if ok else "error")

    def action_view_transcript(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        self.push_screen(TranscriptScreen(self._selected_data))

    def action_show_diff(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        self.push_screen(DiffScreen(self._selected_data))

    def action_show_lineage(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        self.push_screen(LineageScreen(self._selected_data))

    @work
    async def action_add_tag(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        result = await self.push_screen_wait(TextPromptScreen("Add tag(s), space-separated:"))
        if result:
            tags = add_tags(self._selected_data["uuid"], result.split())
            self._select_session(self._selected_data["uuid"])
            await self._refresh_current_row_label()
            self.notify(f"Tags: {', '.join(tags) if tags else '(none)'}")

    @work
    async def action_set_note(self) -> None:
        if not self._selected_data:
            self.notify("No session selected.", severity="warning")
            return
        current_note = self._selected_data.get("note") or ""
        result = await self.push_screen_wait(TextPromptScreen("Note (empty to clear):", initial=current_note))
        if result is not None:
            set_note(self._selected_data["uuid"], result)
            self._select_session(self._selected_data["uuid"])
            self.notify("Note updated." if result else "Note cleared.")

    async def _refresh_current_row_label(self) -> None:
        """After tagging, refresh just the session list's tag markers
        without losing the current scroll position / selection."""
        current_query = self.query_one("#search-input", Input).value
        await self.refresh_list(current_query)


def run_app():
    CctrailApp().run()


if __name__ == "__main__":
    run_app()
