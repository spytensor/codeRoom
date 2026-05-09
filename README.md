# CodeRoom

> A coordination shell for multi-role agent CLI sessions in a single chat-style
> terminal. Each role is a separate `claude` / `codex` / `gemini` subprocess,
> loaded with its own priors, addressed via `@`-mention. Cross-role messages
> route automatically.

[![CI](https://github.com/spytensor/codeRoom/actions/workflows/ci.yml/badge.svg)](https://github.com/spytensor/codeRoom/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> **Status: v0.1.4 — user-runnable, still pre-1.0.** Claude Code,
> Codex, and Gemini adapters are wired up; `cr init` now opens with a
> polished role / engine setup flow on interactive terminals and a clean
> non-interactive summary for scripts. Per semver, 0.x.y means the public
> API is not yet stable.

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
| v0.1 | Multi-engine REPL, role priors, `@` routing, patch / refresh / journal / show / cost, npm install |
| v0.1.x | First-run UI polish, config layering, updater, release hardening |
| v0.2 | `cr review` (patch clustering), `cr verify` (journal fact-check) |
| v0.x | Team mode (per-role human owners), auto-router (opt-in), replay viewer |

See [docs/architecture.md](docs/architecture.md) for the v0.1 constitution
and [docs/spike-2026-05-09.md](docs/spike-2026-05-09.md) for the feasibility
spike that grounds it.

## Install

```bash
npm install -g @spytensor/coderoom
cr --version
```

That's it. `cr` is now on your PATH. Same install story as
`@anthropic-ai/claude-code`, `@openai/codex`, and `@google/gemini-cli` —
which CodeRoom drives.

The npm package is a thin wrapper: on install, its postinstall script
downloads the right pre-built binary for your platform from the
matching [GitHub Release](https://github.com/spytensor/codeRoom/releases)
and verifies its SHA-256. Supported platforms: linux + macOS, x86_64 and
aarch64.

<details>
<summary>Don't have npm? Direct binary install.</summary>

```bash
TAG=v0.1.4
ARCH=$(uname -m); case "$ARCH" in arm64|aarch64) ARCH=aarch64 ;; *) ARCH=x86_64 ;; esac
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
curl -fsSL "https://github.com/spytensor/codeRoom/releases/download/${TAG}/cr-${TAG}-${OS}-${ARCH}.tar.gz" \
  | tar -xz
sudo mv "cr-${TAG}-${OS}-${ARCH}/cr" /usr/local/bin/
cr --version
```

</details>

<details>
<summary>Building from source.</summary>

Requires Rust 1.85+. Use [rustup](https://rustup.rs) — the
distro-shipped `rustc` is usually too old (we depend on `edition2024`
in the wider ecosystem).

```bash
git clone https://github.com/spytensor/codeRoom
cd codeRoom
cargo build --release
sudo cp target/release/cr /usr/local/bin/
```

`cargo install --git ...` works too if your active toolchain is
1.85+; otherwise the install fails inside a transitive dep.

</details>

## Quickstart

```bash
cd your-project
cr init                         # polished role + engine setup
cr start                        # enter the REPL
$EDITOR .coderoom/roles/host.md # optional: give @host real priors

cr › hello
[@host ready · model=claude-opus-4-7]
@host  Hi — what would you like to work on?

cr › @host scope out adding email verification
@host  This touches auth, DB schema, and probably the front-end signup flow…
```

Useful commands:

- `cr start` auto-creates `.coderoom/` with sensible defaults when you skip
  `cr init`.
- `cr role add <name> --engine codex` adds or pins a specialist role.
- `/patch <role> <text>`, `/refresh <role>`, `/transcript <role>`, and
  `/journal <role>` are available inside the REPL.
- `cr show`, `cr cost`, `cr config`, and `cr update` handle inspection,
  spend tracking, layered config, and package upgrades.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). TL;DR: PRs follow conventional
commits, must pass `fmt + clippy + test` in CI, and must not amend a locked
architecture decision without an entry in
[`docs/proposed-amendments.md`](docs/proposed-amendments.md).

## License

MIT. See [LICENSE](LICENSE).
