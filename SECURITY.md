# Security

## Reporting a vulnerability

Please don't open a public issue for security problems. Use GitHub's
[private vulnerability reporting](https://github.com/nikostito/ccrumbs/security/advisories/new)
instead. You'll get a reply within a few days, and a fix or a plan soon after.

## What to keep in mind

ccrumbs reads Claude Code transcripts, which can contain anything that
passed through a session: code, command output, and sometimes secrets.

- **Storage:** the index (`~/.ccrumbs/index.db`) holds excerpts of prompts,
  replies and shell commands for search. Treat it like the transcripts
  themselves.
- **Text from transcripts never runs as code.** It's never executed or
  passed to a shell unquoted. Session ids are restricted to plain
  characters, and paths are quoted. Terminal escape sequences are stripped
  before anything is printed.
- **Network:** ccrumbs only uses the network for features you invoke:
  `describe` (Anthropic API) and `sync` (GitHub, via `gh`). See the Privacy
  section of the README.
- **`ccrumbs mcp`** gives the Claude Code session it's added to read access
  to your session history.
