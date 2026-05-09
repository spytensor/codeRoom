# CodeRoom

> A coordination shell for multi-role agent CLI sessions in a single chat-style
> terminal. Each role is a separate `claude` / `codex` / `gemini` subprocess,
> loaded with its own priors, addressed via `@`-mention. Cross-role messages
> route automatically.

[![CI](https://github.com/spytensor/codeRoom/actions/workflows/ci.yml/badge.svg)](https://github.com/spytensor/codeRoom/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> **Status: pre-alpha.** Architecture is locked
> ([docs/architecture.md](docs/architecture.md)); v0.1 implementation is in
> progress. Not yet usable. The first user-runnable build will be tagged
> `v0.1.0`.

## Why

A single `CLAUDE.md` is a global namespace. As projects accumulate years of
conventions, one-off compliance rules, and decisions buried in commit messages
or comments, one file forces three problems: bloat, attention dilution, and
no way to express "this rule only matters to backend".

CodeRoom partitions organizational knowledge by role. Each role is a separate
agent CLI subprocess loaded with its own priors. The user `@`-mentions roles
to address them. Cross-role routing happens when one role writes `@x` in its
reply.

## What you get

- **Role-pinned engines.** `@backend` can run on `claude`, `@security` on
  `codex`, `@frontend` on `gemini`. No other tool does this today.
- **One chat stream, not split panes.** Single message log per project,
  colored by role.
- **Daily journals.** Every role writes an end-of-session log with cited
  evidence. Auto-loaded for the next 7 days.
- **Patches.** `/patch <role> "..."` saves a session-time correction; the
  role picks it up on next refresh. v0.2 promotes high-signal patches into
  base priors.
- **Permission gate.** `--dangerously-skip-permissions` + a `PreToolUse`
  hook handed to each engine; the wrapper is the sole arbiter, with a
  `--max-budget-usd` ceiling as backstop.

## Status / Roadmap

| Milestone | Scope |
| --------- | ----- |
| v0.1 (in progress) | Single-engine CC adapter, REPL, `@`-mention routing, journal, patch (manual promote) |
| v0.2 | Codex + Gemini adapters, `cr review` (patch clustering), `cr verify` (journal fact-check) |
| v0.x | Team mode (per-role human owners), auto-router (opt-in), replay viewer |

See [docs/architecture.md](docs/architecture.md) for the v0.1 constitution
and [docs/spike-2026-05-09.md](docs/spike-2026-05-09.md) for the feasibility
spike that grounds it.

## Install

Not yet released. Once v0.1.0 ships:

```bash
# from a release binary (linux / macOS):
curl -fsSL https://github.com/spytensor/codeRoom/releases/latest/download/cr-$(uname -s)-$(uname -m).tar.gz | tar xz
sudo mv cr /usr/local/bin/

# or from source:
cargo install --git https://github.com/spytensor/codeRoom
```

## Quickstart (target shape, not yet runnable)

```bash
cd your-project
cr init                         # walk through role setup, pick a host
cr role add backend --engine cc
cr role add security --engine codex
cr role add frontend --engine gemini

cr start                        # enter the REPL
> we want email verification on signup
[host] looked at the auth module. @backend / @security please scope it.
[backend] would add /v1/auth/email-verify behind the gateway, plus...
[security] hard requirements: 5/hr/IP rate limit, single-use 24h tokens, ...
> /patch backend rate limit lives in gateway/routes/auth.yaml, not in code
[backend] noted; will configure in gateway.
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). TL;DR: PRs follow conventional
commits, must pass `fmt + clippy + test` in CI, and must not amend a locked
architecture decision without an entry in
[`docs/proposed-amendments.md`](docs/proposed-amendments.md).

## License

MIT. See [LICENSE](LICENSE).
