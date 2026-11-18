# cctrail

Find a Claude Code session in seconds, see what it did and which PRs it
**created** or **reviewed**, and jump straight back into it.

```
 Sessions 4/4  PRs 3   ❯ █ type to search · #123 for a PR · ctrl-f full-text
   5d what changed in https://github.com/acme/web/pu…   │ Rust rewrite of the session indexer
      web ⎇ main                                        │ /work/acme/web  ⎇ feat/rust-indexer
   6d Fix flaky login test                              │ 2026-11-10 09:00 · 2 prompts · 1h04m · 1k out ·
      web ⎇ claude/fix-login ☁ ●#13                     │ claude-opus-5-5
   7d /review 12                                        │ id aaaa1111-0000-4000-8000-000000000001
      web ⎇ main ◆#12                                   │
▌  8d Rust rewrite of the session indexer               │ Description
      web ⎇ feat/rust-indexer ●#12                      │ Asked: Rewrite the indexer in Rust for speed, keep the
                                                        │ SQLite schema
                                                        │ Then: looks good, commit and open a PR
                                                        │ Edited 2 files: index.rs, Cargo.toml · ran tests · 1 commit
                                                        │ Opened PR #12
                                                        │
                                                        │ Pull requests
                                                        │  ● CREATED   acme/web#12 Rewrite indexer in Rust
                                                        │      ↳ reviewed in another session: bbbb2222 "/review 12"
enter resume  ^o new tab  ^y copy cmd  ^d AI describe  tab sessions/PRs  ^b browser  ^f full-text  ←→ scroll  esc quit
```

Type a few words (or `#123` for a PR), press **Enter**, and you're back in
that session: cctrail `cd`s into its directory and runs
`claude --resume <id>` for you.

## What it gives you

- **A description for every session.** Each session gets one for free at
  index time: what you asked, what followed, which files were edited,
  whether tests ran, commits, and PRs. `cctrail describe` can also have
  Claude write a short title and summary (needs `ANTHROPIC_API_KEY`).
- **PRs born from Claude sessions.** cctrail reads the transcripts for
  `gh pr create`, `gh pr review/merge/comment`, `gh api …/pulls/…`,
  GitHub MCP tools (`create_pull_request`, `pull_request_review_write`, …),
  and prompts like `/review 42` or "review https://github.com/o/r/pull/7".
  Failed commands and text inside heredocs or quoted strings don't count.
- **PR lookup by number.** `cctrail pr 42` (or `#42`, `owner/repo#42`, or a
  PR URL) lists every session that touched the PR and says how.
- **Created by one session, reviewed by another, stated plainly:**

```
acme/web#12  [open]
Rewrite indexer in Rust
https://github.com/acme/web/pull/12
Born from a Claude session

Created by aaaa1111 "Rust rewrite of the session indexer" · reviewed by a DIFFERENT session: bbbb2222 "/review 12"

  ● CREATED    aaaa1111  2026-11-10 10:03  Rust rewrite of the session indexer
               /work/acme/web · gh pr create
  ◆ REVIEWED   bbbb2222  2026-11-11 14:00  /review 12
               /work/acme/web · you asked: /review 12 (also viewed)
  · MENTIONED  dddd4444  2026-11-13 16:00  what changed in https://github.com/acme/web/pull/12 ?
               /work/acme/web · mentioned in a prompt

Jump in: cctrail go aaaa1111
```

  You'll see one of: *"Created by A · reviewed by a DIFFERENT session: B"*,
  *"Created and reviewed in the SAME session"*, or *"Reviewed by B · created
  outside your Claude sessions"*. A session's detail view also says
  *"↳ created in another session: …"* next to PRs it only reviewed.
- **Fast.** It's written in Rust: transcripts are parsed in parallel, and only
  changed files are re-parsed. On a synthetic 641 MB history of 2,000
  sessions, the first index takes about 4 s; after that the automatic
  re-index on every launch takes about 10 ms, so the picker opens at once.

## Install

You need a Rust toolchain ([rustup.rs](https://rustup.rs)).

```bash
cargo install --git https://github.com/nikostito/cctrail --locked
```

Or, from a clone, run `cargo install --path . --locked`. That puts a single
`cctrail` binary in `~/.cargo/bin`.

## Everyday use

| Command | What it does |
|---|---|
| `cctrail` | Open the picker |
| `cctrail go <words / id / #PR>` | Jump straight in when there's exactly one match; otherwise open the picker with that query |
| `cctrail pr 42` | Who created / reviewed / merged PR 42, then `cctrail go <id>` |
| `cctrail prs [--claude] [--repo web]` | PRs your sessions created or reviewed; `--claude` keeps only the ones born from a Claude session |
| `cctrail list [-n 50] [-p project] [-s]` | Recent sessions with descriptions |
| `cctrail search <words>` | Full-text search across every prompt, reply and shell command |
| `cctrail show <id>` | Everything about one session |
| `cctrail resume <id> [--tab \| --print]` | Resume here, in a new terminal tab, or just print the command |

Session ids can be abbreviated to their first few characters, like git
hashes.

### Picker keys

| Key | Action |
|---|---|
| typing | filter (fuzzy on title, project, branch, PR numbers; falls back to description and prompts) |
| `#42`, `acme/web#42`, PR URL | only sessions linked to that PR, creator first |
| **Enter** | resume the session (on the PRs tab: show that PR's sessions) |
| **Tab** | switch between Sessions and PRs |
| `Ctrl-O` | resume in a new terminal tab (tmux, zellij, wezterm, kitty, iTerm2, Terminal.app, GNOME Terminal, Konsole, Windows Terminal, …) |
| `Ctrl-Y` | copy `cd … && claude --resume …` |
| `Ctrl-F` | toggle full-text search |
| `Ctrl-D` | have Claude describe the selected session |
| `Ctrl-B` | open the PR, or the claude.ai cloud session, in your browser |
| `Ctrl-R` | re-index |
| `←` / `→` | scroll the preview |
| `Esc` | clear the query, or quit |

## AI descriptions

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cctrail describe            # the 20 most recent sessions without one
cctrail describe --all      # every session that needs one
cctrail describe a1b2c3d4   # one session (or Ctrl-D in the picker)
```

Claude gets a compact digest of the session: your prompts, files edited,
PR roles, and the end of the last reply. It writes back a title of up to 8
words and a one- or two-sentence summary, which are cached in the index.
Sessions are described again only after they grow noticeably. The default
model is `claude-opus-5-5` at low effort; set `"describe_model"` in the
config or pass `--model` to change it.

## Cloud sessions (claude.ai/code) and GitHub sync

PRs opened from claude.ai cloud sessions carry a `claude.ai/code/session_…`
link in their body. With the [GitHub CLI](https://cli.github.com)
logged in:

```bash
cctrail sync                       # every repo seen in your sessions
cctrail sync --repo acme/web -n 500
cctrail pr 42 --sync               # refresh one PR
```

This fetches PR titles and state, and marks PRs whose body links a cloud
session, or carries the "Generated with Claude Code" footer, as **born from
Claude**, even when there's no transcript on this machine. It also links
sessions that worked on a PR's head branch.

## How detection works

| Role | Evidence |
|---|---|
| ● created | `gh pr create` (number taken from its output), `gh api -X POST repos/o/r/pulls`, MCP `create_pull_request`, or a synced PR whose body links this session |
| ◆ reviewed | `gh pr review`, review/comment POSTs to `…/pulls/N/reviews`, MCP review tools, `/review N` / `/code-review N`, or a prompt asking to review a PR |
| ✔ merged | `gh pr merge`, MCP `merge_pull_request` |
| ✎ commented | `gh pr comment`, `…/issues/N/comments`, MCP `add_issue_comment` |
| ↻ worked on | `gh pr checkout/edit`, MCP `update_pull_request`, PR activity subscriptions, "address the review comments on #N", or the PR's head branch |
| · viewed / mentioned | `gh pr view/diff`, MCP `pull_request_read`, PR URLs in prompts or replies |

A bare `#42` is resolved to a repo using, in order: `-R owner/repo`, the
session directory's `origin` remote, `git push` output in the transcript,
and other PR URLs in the same session.

## Configuration

`cctrail config` shows the paths in use. `~/.cctrail/config.json`:

```json
{
  "claude_projects_dir": null,
  "ignored_projects": [],
  "describe_model": "claude-opus-5-5"
}
```

- `claude_projects_dir` defaults to `$CLAUDE_CONFIG_DIR/projects` or
  `~/.claude/projects`.
- `cctrail ignore <substring>` / `cctrail unignore <substring>` stops (or
  resumes) indexing matching projects.
- `CCTRAIL_HOME` moves the index and config elsewhere.
- `--no-index` skips the automatic incremental index for one command.

The index is a SQLite file. If Claude Code's retention deletes a transcript,
the session stays searchable but is marked *transcript deleted* and can't be
resumed.

## Limitations

- The JSONL transcript format is internal to Claude Code. Parsing is
  defensive, but a big format change may need a parser update.
- Role detection is evidence-based, not authoritative. A PR you reviewed by
  reading the diff in chat counts as *reviewed* only if you asked for a
  review, or the session posted one.
- Cloud-only sessions can't be resumed locally; `Ctrl-B` opens them on
  claude.ai.

## Development

```bash
cargo test
cargo clippy
cargo run --release -- list
```
