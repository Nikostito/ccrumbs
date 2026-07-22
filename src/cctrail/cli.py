"""
cctrail.cli - command-line entry point.

Installed as the `cctrail` console script (see pyproject.toml). Run
`cctrail --help` for usage, or see the project README.
"""

import argparse
import sys
import time

from cctrail.core import (
    cmd_index,
    cmd_stats,
    cmd_search,
    cmd_show,
    cmd_list,
    cmd_projects,
    cmd_tag,
    cmd_untag,
    cmd_note,
    cmd_lineage,
    cmd_prune,
    cmd_forget,
    cmd_config,
    cmd_ignore,
    cmd_unignore,
    get_session_by_uuid_or_prefix,
    get_resume_directory,
    perform_index,
)
from cctrail.terminal import build_resume_command, system_clipboard_copy, open_new_terminal_and_resume
from cctrail.viewer import read_transcript_turns, format_transcript_plain
from cctrail.gitutil import is_git_repo, commits_in_window, diffstat_for_commits


def cmd_resume(args):
    data = get_session_by_uuid_or_prefix(args.uuid)
    if data is None:
        print(f"No session found matching '{args.uuid}'")
        return 1

    uuid = data["uuid"]
    directory = get_resume_directory(data)
    command = build_resume_command(uuid)

    print(f"UUID:      {uuid}")
    print(f"Directory: {directory or '(unknown)'}")
    print(f"Command:   {command}")

    if not args.no_copy:
        if system_clipboard_copy(command):
            print("(copied resume command to clipboard)")
        else:
            print("(clipboard copy failed - no clipboard tool found for this platform)")

    if args.open:
        ok, message = open_new_terminal_and_resume(directory, uuid)
        print(message)
        if not ok:
            return 1

    return 0


def cmd_view(args):
    data = get_session_by_uuid_or_prefix(args.uuid)
    if data is None:
        print(f"No session found matching '{args.uuid}'")
        return 1
    turns = read_transcript_turns(data["file_path"])
    print(format_transcript_plain(turns))
    return 0


def cmd_diff(args):
    data = get_session_by_uuid_or_prefix(args.uuid)
    if data is None:
        print(f"No session found matching '{args.uuid}'")
        return 1

    directory = get_resume_directory(data)
    if not directory:
        print("No directory known for this session.")
        return 1
    if not is_git_repo(directory):
        print(f"'{directory}' is not a git repository (or git isn't installed).")
        return 1

    commits = commits_in_window(directory, data["start_time"], data["end_time"])
    if not commits:
        print(f"No commits found in {directory} during this session's time window "
              f"({data['start_time']} to {data['end_time']}).")
        return 0

    print(f"Commits in {directory} during this session:\n")
    for sha, author_date, subject in commits:
        print(f"  {sha[:8]}  {author_date}  {subject}")

    diffstat = diffstat_for_commits(directory, commits)
    if diffstat:
        print("\nDiffstat:\n")
        print(diffstat)

    return 0


def cmd_watch(args):
    print(f"Watching for new/changed sessions every {args.interval}s. Ctrl+C to stop.")
    try:
        while True:
            result = perform_index()
            if result is not None:
                new_count, updated_count, _skipped, _ignored = result
                if new_count or updated_count:
                    ts = time.strftime("%H:%M:%S")
                    print(f"[{ts}] {new_count} new, {updated_count} updated")
            time.sleep(args.interval)
    except KeyboardInterrupt:
        print("\nStopped.")
    return 0


def main():
    parser = argparse.ArgumentParser(prog="cctrail", description="Claude Code session tracker")
    sub = parser.add_subparsers(dest="command")

    sub.add_parser("index", help="Scan and index all Claude Code sessions")

    sub.add_parser("stats", help="Show usage statistics")

    p_search = sub.add_parser("search", help="Full-text keyword search across sessions")
    p_search.add_argument("keywords", nargs="+", help="Keywords to search for")
    p_search.add_argument("--project", help="Restrict search to projects matching this substring")
    p_search.add_argument("--tag", help="Restrict search to sessions with this tag")
    p_search.add_argument("--limit", type=int, default=15)

    p_show = sub.add_parser("show", help="Show details for one session")
    p_show.add_argument("uuid", help="Session UUID (or unique prefix)")

    p_list = sub.add_parser("list", help="List recent sessions")
    p_list.add_argument("--project", help="Filter by project path substring")
    p_list.add_argument("--tag", help="Filter by tag")
    p_list.add_argument("--limit", type=int, default=20)

    sub.add_parser("projects", help="List all projects with session counts")

    p_resume = sub.add_parser("resume", help="Print (and copy) the command to resume a session")
    p_resume.add_argument("uuid", help="Session UUID (or unique prefix)")
    p_resume.add_argument("--open", action="store_true",
                           help="Also open a new terminal tab/window, cd into the session's "
                                "directory, and run the resume command there")
    p_resume.add_argument("--no-copy", action="store_true",
                           help="Don't copy the resume command to the clipboard")

    p_view = sub.add_parser("view", help="Print the full conversation transcript for a session")
    p_view.add_argument("uuid", help="Session UUID (or unique prefix)")

    p_diff = sub.add_parser("diff", help="Show git commits/diffstat made during a session's time window")
    p_diff.add_argument("uuid", help="Session UUID (or unique prefix)")

    p_tag = sub.add_parser("tag", help="Add one or more tags to a session")
    p_tag.add_argument("uuid", help="Session UUID (or unique prefix)")
    p_tag.add_argument("tags", nargs="+", help="Tag(s) to add")

    p_untag = sub.add_parser("untag", help="Remove a tag from a session")
    p_untag.add_argument("uuid", help="Session UUID (or unique prefix)")
    p_untag.add_argument("tag", help="Tag to remove")

    p_note = sub.add_parser("note", help="Set (or clear) a note on a session")
    p_note.add_argument("uuid", help="Session UUID (or unique prefix)")
    p_note.add_argument("text", nargs="?", default="", help="Note text (omit to clear)")

    p_lineage = sub.add_parser("lineage", help="Show sessions related to this one (resume chain)")
    p_lineage.add_argument("uuid", help="Session UUID (or unique prefix)")

    sub.add_parser("prune", help="Remove indexed sessions whose transcript file no longer exists")

    p_forget = sub.add_parser("forget", help="Remove one session from the local index only")
    p_forget.add_argument("uuid", help="Session UUID (or unique prefix)")

    sub.add_parser("config", help="Show the current configuration")

    p_ignore = sub.add_parser("ignore", help="Exclude a project (substring match) from indexing")
    p_ignore.add_argument("substring", help="Substring to match against project paths")

    p_unignore = sub.add_parser("unignore", help="Remove a project substring from the ignore list")
    p_unignore.add_argument("substring", help="Substring previously passed to `ignore`")

    p_watch = sub.add_parser("watch", help="Continuously reindex on an interval (Ctrl+C to stop)")
    p_watch.add_argument("--interval", type=int, default=30, help="Seconds between scans (default: 30)")

    sub.add_parser("tui", help="Launch interactive terminal UI (lazygit-style)")

    args = parser.parse_args()

    if args.command == "index":
        sys.exit(cmd_index(args))
    elif args.command == "stats":
        sys.exit(cmd_stats(args))
    elif args.command == "search":
        sys.exit(cmd_search(args))
    elif args.command == "show":
        sys.exit(cmd_show(args))
    elif args.command == "list":
        sys.exit(cmd_list(args))
    elif args.command == "projects":
        sys.exit(cmd_projects(args))
    elif args.command == "resume":
        sys.exit(cmd_resume(args))
    elif args.command == "view":
        sys.exit(cmd_view(args))
    elif args.command == "diff":
        sys.exit(cmd_diff(args))
    elif args.command == "tag":
        sys.exit(cmd_tag(args))
    elif args.command == "untag":
        sys.exit(cmd_untag(args))
    elif args.command == "note":
        sys.exit(cmd_note(args))
    elif args.command == "lineage":
        sys.exit(cmd_lineage(args))
    elif args.command == "prune":
        sys.exit(cmd_prune(args))
    elif args.command == "forget":
        sys.exit(cmd_forget(args))
    elif args.command == "config":
        sys.exit(cmd_config(args))
    elif args.command == "ignore":
        sys.exit(cmd_ignore(args))
    elif args.command == "unignore":
        sys.exit(cmd_unignore(args))
    elif args.command == "watch":
        sys.exit(cmd_watch(args))
    elif args.command == "tui":
        try:
            from cctrail.tui import run_app
        except ImportError:
            print("The TUI requires the 'textual' package. Install it with:")
            print("  pip install cctrail[tui]")
            print("or, if installed from source:")
            print("  pip install '.[tui]'")
            sys.exit(1)
        run_app()
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
