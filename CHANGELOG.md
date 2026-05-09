# Changelog

All notable changes to CodeRoom are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

(nothing yet)

## [0.1.1] - 2026-05-09

UX-only release. No behavioral changes to the engine adapters or CREP.

### Added

- **`cr start` auto-init.** First-time users no longer need to run
  `cr init` separately; if `.coderoom/` is missing, `cr start`
  bootstraps a default `@host` role and proceeds into the REPL with
  the placeholder priors. `cr init` is still available for users who
  want to set things up explicitly.
- **Pre-built release binaries.** Tag pushes now trigger
  `.github/workflows/release.yml`, which builds `cr` for
  `linux-x86_64`, `linux-aarch64` (via `cross`), `macos-x86_64`, and
  `macos-aarch64`. Each platform's tarball ships with `cr`, LICENSE,
  README, CHANGELOG, plus a sha256, uploaded as Release assets.

### Changed

- README install section now leads with the pre-built binary
  one-liner; `cargo install` is documented as the alternative path
  with a note that it requires Rust 1.85+ via rustup (the v0.1.0
  install foot-gun for users with an older system `rustc`).

## [0.1.0] - 2026-05-09

First user-runnable release. Implements the v0.1 scope locked in
`docs/architecture.md`. Note: per semver, 0.x.y means the public API is
not yet stable — expect breaking changes during v0.x. v1.0.0 will mark
API stability, not feature completeness.

### Added

#### Engines

- Claude Code adapter (`engine = "cc"`) via stream-JSON over stdio:
  long-lived subprocess with cache reuse, structured event mapping for
  RoleStarted / RoleSpoke / ToolCallProposed / ToolCallExecuted /
  PermissionDenied / RoleStopped.
- Codex adapter (`engine = "codex"`) via `codex mcp-server` JSON-RPC:
  single-turn-per-message at v0.1, hard-coded `approval-policy=never`
  + `sandbox=workspace-write`.
- Gemini adapter (`engine = "gemini"`) via per-turn `gemini -p` with
  priors prepended to the user prompt; `-y` (yolo) for approvals.
- Per-role engine pinning in `config.toml` — the differentiator vs
  Agent Teams / Agentrooms / OpenCode (none of which let you mix
  engines per role).

#### Knowledge model

- `priors::compose_for` composes a role's full system prompt at spawn
  time from `shared.md` + `roles/<role>.md` + active patches + last
  7 days of journal entries.
- `/patch <role> <text>` saves a session-time correction under
  `patches/<role>/NNN-slug.md` with monotonic numbering across active
  + archived files.
- 50-cap FIFO archive: when the active patch count for a role exceeds
  `MAX_ACTIVE_PATCHES_PER_ROLE`, the oldest is moved to `_archive/`
  (still grep-able, no longer auto-loaded).
- `/refresh <role>` re-instantiates a role with the freshly composed
  priors so a just-saved patch takes effect.
- `/journal <role>` asks the role to write a cited end-of-session
  summary to `journal/YYYY-MM-DD/<role>.md`. Auto-loaded on next spawn
  via `compose_for`.

#### REPL

- `cr start` spawns every configured role in parallel, forwards each
  role's CREP events into a shared `MessageBus`, and enters a synchronous
  line-mode REPL.
- Bare text → host role; `@<role> <text>` → that role.
- One-hop cross-role auto-routing: when role A's `RoleSpoke` mentions
  `@B` (where B is a running role), the wrapper auto-forwards a
  brief ("From @A: …") to B and drains its turn.
- `/stop <role>`, `/help`, `/exit`, `/transcript <role>` (last 5 turns
  from the message log).
- Color-coded inline rendering via `crossterm`: deterministic per-role
  colors hashed from the role name, dim-italic for status events, dim
  one-liners for tool-call lifecycle, yellow for `PermissionDenied`.

#### CLI

- `cr init` non-interactive bootstrap of `.coderoom/` with one default
  `host` role and a `.gitignore` that hides runtime artifacts.
- `cr role add <name> [--engine] [--model]` / `cr role list` /
  `cr role rm <name>` (refuses for the configured host).
- `cr show` replays the entire `messages.jsonl` log through the live
  renderer.
- `cr cost [--since YYYY-MM-DD]` aggregates `RoleSpoke.cost_usd` and
  `cache_read` per role, prints a fixed-column table.

#### Infra

- CodeRoom Event Protocol (CREP) — six-variant tagged enum that every
  adapter emits and the rest of the wrapper consumes.
- `MessageBus` (append-only JSONL log + `tokio::broadcast` fan-out)
  is the single source of truth for events. Disk write happens before
  subscriber notification, so the log and the live stream agree.
- GitHub Actions CI: `fmt`, `clippy`, multi-OS `test` (ubuntu + macOS),
  `shellcheck` of the spike harness. Manual `Integration` workflow
  (`workflow_dispatch`) runs real-engine smokes against `claude` /
  `codex` / `gemini` with `engines` input filtering.
- Dependabot (cargo + actions, weekly).
- Issue + PR templates enforcing conventional commits and the
  architecture-amendment workflow.

### Known limitations (deferred to v0.2 or later)

- **No PreToolUse hook gate.** All three engines run with their
  own dangerous-skip equivalents; the wrapper observes tool calls but
  doesn't yet adjudicate them. Documented in
  `docs/proposed-amendments.md`.
- **Codex is single-turn per user message.** No `codex-reply` plumbing
  yet; each message starts a fresh codex session.
- **Gemini lacks streaming + tool events.** Per-turn `gemini -p`
  gives final text only; mid-session events are invisible.
- **Multi-hop auto-routing.** Cross-role @ forwarding is one hop only;
  no hop-depth ≥ 3 escalation yet.
- **Journal write is manual** (`/journal <role>`). Auto-write at
  session end / on `/refresh`, plus JSON-schema-validated entries
  with citation enforcement, are v0.2.
- **No `cr review`** for clustering correction patches and proposing
  promotion to base priors.
- **No timestamps in CREP events.** `cr cost --since` honors the log
  file's mtime only; per-event timestamps land in v0.2.

[Unreleased]: https://github.com/spytensor/codeRoom/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.1
[0.1.0]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.0
