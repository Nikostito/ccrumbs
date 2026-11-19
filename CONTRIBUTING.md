# Contributing to ccrumbs

Thanks for helping. Bug reports, ideas and pull requests are all welcome.

## Reporting a bug

Please include:

- the output of `ccrumbs --version` and your OS and terminal;
- what you ran, what you expected and what happened;
- for parsing problems, the record types involved. Transcripts contain
  your code and prompts, so please don't attach a whole one: a few lines with
  the content replaced by placeholders is plenty.

`CCRUMBS_TIMING=1 ccrumbs index --full` prints per-phase timings for
performance reports.

## Development

You need Rust 1.89 or newer.

```bash
cargo test                                # unit tests and end-to-end tests in tests/cli.rs
cargo clippy --all-targets -- -D warnings
cargo fmt
```

To try changes without touching your real index, point ccrumbs at a scratch
folder:

```bash
CCRUMBS_HOME=/tmp/ccrumbs-dev cargo run -- list
```

`CLAUDE_CONFIG_DIR=/path/to/fixtures` makes it read transcripts from
`/path/to/fixtures/projects` instead of `~/.claude/projects`. The end-to-end
tests in `tests/cli.rs` show how to build small fixture transcripts.

## Where things live

| File | What it does |
|---|---|
| `src/index.rs` | finds changed transcripts, parses them in parallel, writes the index |
| `src/parse.rs` | turns one JSONL transcript into a session record |
| `src/prdetect.rs` | PR roles from shell commands, MCP tool calls and prompts |
| `src/db.rs` | SQLite schema, migrations and queries |
| `src/search.rs` | query syntax, filters and ranking |
| `src/tasks.rs`, `src/prview.rs` | grouping into tasks; one entry per PR |
| `src/tui.rs`, `src/tui/view.rs` | the picker: keys and actions; layout |
| `src/launch.rs` | resuming, new terminal tabs, editors, browser |
| `src/mcp.rs` | the MCP server |

## Pull requests

- Keep each PR focused, and add a test for any behaviour you change. Parser
  and detection fixes almost always fit a small unit test next to the code.
- CI runs the tests on Linux, macOS and Windows, plus `fmt`, `clippy` and the
  minimum Rust version.
- Note user-visible changes in `CHANGELOG.md` under "Unreleased".

## Releasing (maintainers)

1. Update `version` in `Cargo.toml` and the changelog; commit.
2. Tag and push: `git tag v0.1.0 && git push origin v0.1.0`. The release
   workflow builds the binaries and creates the GitHub release.
3. Publish the crate: `cargo publish`.
