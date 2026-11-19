# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.1.0] - unreleased

First public release, as **ccrumbs**. It is a rewrite in Rust of an earlier
Python tool called cctrail.

### Added
- Picker TUI: sessions grouped by Pinned / Here / day, with a detail card
  (facts, what happened, next step, PRs and who did what, same task, files).
  It also has Tasks and PRs views and a `?` overlay.
- Ranking by the repo and branch you're in; `ccrumbs go` continues the
  latest session there.
- PR roles detected from transcripts (`gh pr …`, `gh api …/pulls/…`, GitHub
  MCP tools, `/review N`): created, reviewed, merged, commented, worked on.
  `ccrumbs pr <n>` says when creator and reviewer were different sessions.
- Tasks: sessions grouped by branch, PR or ticket, with total cost.
- Jira and Linear ticket keys from branches, prompts and titles, with links
  set via `ccrumbs tickets set` or the `^k` prompt.
- Search filters: `#PR`, ticket keys, `p:`, `b:`, `f:`, `repo:`, `@today`,
  `@3d`…, `has:pr`, `has:note`, `is:pinned`, `is:hidden`, `ctx:>80`.
- Context-window fill, compaction count and cost per session.
- Conversation viewer (`^v`, `ccrumbs view`), Markdown export, notes, pins and
  hiding.
- Editor integration with detection and a remembered choice; resuming in a
  new tab of the terminal in use.
- AI titles and descriptions with Claude (`ccrumbs describe`).
- `ccrumbs sync` for PR titles and state from GitHub, including PRs opened
  from claude.ai sessions.
- MCP server (`ccrumbs mcp`) with `search_sessions`, `get_session`,
  `read_transcript` and `find_pr`.
- Shell integration (`shell-init`: Alt-S and `ccd`) and tab completion
  (`completions`).
- Incremental, parallel indexing into SQLite with full-text search. Sessions
  whose transcripts Claude Code deleted stay searchable.

### Migrating from cctrail
- `~/.cctrail` (index, pins, notes, settings) is copied to `~/.ccrumbs` on
  first run, and the `CCTRAIL_*` environment variables still work.

[0.1.0]: https://github.com/nikostito/ccrumbs/releases/tag/v0.1.0
