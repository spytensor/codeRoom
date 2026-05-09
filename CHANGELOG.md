# Changelog

All notable changes to CodeRoom are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

(nothing yet)

## [0.1.10] - 2026-05-09

### Fixed

- The interactive role picker (and the engine picker, and the confirm
  screen) no longer renders diagonally down-right across the terminal.
  `WizardTerminal::render` writes its body with `\n` line endings, but
  raw mode disables ONLCR — so each `\n` only moved the cursor down
  without returning to column 0, and every row started where the row
  above ended. Width-aware row formatting (added in 0.1.8) was correct;
  the line terminator wasn't. Now translates `\n` → `\r\n` once before
  writing. This is the bug users saw as "garbled picker" in 0.1.7,
  0.1.8, and 0.1.9.

## [0.1.9] - 2026-05-09

### Fixed

- `cr update` no longer lies about success. The previous implementation
  shelled out to `npm install -g …@latest` and printed `✓ updated`
  whenever npm exited 0. When npm's tarball cache was stale, npm
  returned success but extracted the cached old tarball — so users saw
  `✓ updated` while `cr --version` was unchanged. This was the failure
  mode that left users stuck on 0.1.7 despite 0.1.8 being available.
- The new `cr upgrade` re-execs the binary at `current_exe()` after
  npm finishes and parses `--version` to confirm the bytes on disk
  actually changed. If the post-install version still matches the
  pre-install version while the registry has a newer one, it prints
  the exact remediation (`npm cache clean --force && cr upgrade`) and
  exits non-zero instead of claiming success.

### Changed

- `cr update` and `cr upgrade` are now distinct commands, brew-style:
  - `cr update` is read-only. It queries the npm registry for the
    `@latest` version, prints local vs registry, and tells you whether
    a new version is available. No side effects.
  - `cr upgrade` is the side-effecting install path with the
    verification described above. It pre-flight-checks the registry
    and skips the install entirely when you're already on latest.

### Notes

- If you're stuck on a prior version after a botched `cr update`,
  run `npm cache clean --force && npm install -g @spytensor/coderoom@latest`
  once. Future upgrades via `cr upgrade` will detect the cache-stale
  case automatically.

## [0.1.8] - 2026-05-09

### Fixed

- Role expansion picker no longer bleeds across rows in real terminals.
  The previous implementation padded `StyledContent` values with `{:<N}`
  format specifiers; SGR escape bytes were counted in the padding budget,
  so visible row widths drifted unpredictably and lines wrapped onto each
  other. The picker is now rendered as a single line per role with
  fixed-width plain padding applied before color, and the description is
  truncated (with `…`) to fit terminal width minus a 22-char prefix.
  Verified at 60 / 80 / 120 columns.
- `print_engine_summary`, `print_role_summary`, `push_engine_status_compact`,
  `push_tree_preview`, `print_role_plan_to_buffer`, and the engine picker
  shared the same SGR-pollution bug; all are corrected.

### Added

- `cr` aborts cleanly with install instructions when none of `claude`,
  `codex`, or `gemini` is on `$PATH`. `cr config` and `cr update` are
  exempted (both are useful when fixing the very setup that's missing).
- `src/output.rs` centralises the color palette and semantic helpers
  per the new `docs/colors.md` spec. Truecolor RGB replaces the previous
  ANSI-name palette; FNV-1a hashing replaces `DefaultHasher` so a role's
  color is stable across Rust toolchain versions, not just within a
  single build.
- `docs/DEVELOPMENT.md` now requires screenshot verification at
  60 × 20, 80 × 24, and 120 × 40 for any PR that touches the wizard,
  pickers, dashboard, or palette. `cargo test` cannot validate layout.
- `cargo test --lib picker_visual_smoke -- --nocapture --ignored`
  renders the picker rows at three widths to stderr for human review.

### Changed

- The boot dashboard, REPL status messages, tool traces, and system
  bracket lines (`[@role ready]`, `[@role stopped: ...]`) all route
  through `output::*`. The previous palette of ANSI color names
  rendered inconsistently across terminals; the new truecolor palette
  is muted L 70–83% pastels chosen for AA-text contrast on common
  dark backgrounds.
- `init.rs` no longer maintains its own role color table; it imports
  `output::role_color` so the wizard and the REPL agree on the palette.
- Picker descriptions no longer show a per-row `0.2k` token estimate or
  the cursor-row preview tree (`└─ knows X, Y`). Both were noise that
  fragmented the layout.

## [0.1.7] - 2026-05-09

### Added

- `cr` / `cr start` now detect existing projects that still have only
  the default `@host` role and offer an opt-in role suggestion flow.
  Users can checkbox the specialists they want, and CodeRoom appends
  config + priors in one loadable write.

### Changed

- Generated host / specialist / shared priors are now compact role
  boundaries instead of long placeholder instructions. The default
  guidance now points long procedures and reference material toward
  engine skills or project docs rather than burning context in every
  role prompt.
- Role expansion never blocks non-TTY sessions, never overwrites
  existing role priors, and avoids cross-engine default-model leaks
  when adding suggested roles.

## [0.1.6] - 2026-05-09

### Changed

- Live REPL turns now fold `ToolCallProposed` / `ToolCallExecuted`
  chatter into one dim activity summary, so internal Read / Bash /
  search traces no longer flood the main conversation.
- Full tool traces still persist in `.coderoom/messages.jsonl` and
  remain visible through `cr show` for audits and debugging.

## [0.1.5] - 2026-05-09

### Changed

- Bare `cr` now enters the CodeRoom REPL directly; `cr start` remains
  as the explicit spelling.
- Missing `.coderoom/` on an interactive terminal now opens the guided
  setup flow instead of silently accepting defaults.
- `cr start` now renders a persistent home dashboard on every launch:
  effective config layers, host role, role count, priors token total,
  and each role's engine / model / context / token profile.
- npm installs now expose both `cr` and `croom` command names, and
  release archives include a `croom` binary alias for environments
  where `cr` conflicts with an existing command.

## [0.1.4] - 2026-05-09

### Changed

- `cr init` now uses a polished default terminal flow on interactive
  terminals: role picking, per-role engine assignment, and a final
  file-tree confirmation before writing `.coderoom/`.
- `cr init -y` keeps the clean summary but skips prompts, instead of
  taking the terse `cr start` auto-init path.
- `cr start` first-run and steady-state screens now present CodeRoom as
  a product surface (`cr ›` prompt, compact project / role / token
  summary) rather than plain logs.
- README install / quickstart / roadmap copy now matches the released
  npm + binary distribution path.

## [0.1.3] - 2026-05-09

UI redesign release. Closes 4 of the 5 issues opened after the
multi-agent UX review (#30–#33). #34 (opt-in ratatui wizard) is
deferred — the agent reviews themselves flagged it as low-value,
and `cr init` after #33 already covers ~95% of its purpose.

### Added

- **Inline thinking indicator (#30):** `  @<role> thinking ⠋`
  spinner repaints in place via carriage-return + ANSI clear-line
  while waiting for events. The single highest-leverage UX bet per
  three independent reviewers — finally answers "is anything
  happening?" without the scrollback corruption a sticky bottom bar
  would introduce in tmux/screen/Apple Terminal/ConPTY/mosh.
- **First-run welcome (#31, Variant E):** one-time card explaining
  the project, listing roles with token estimates, three-things-to-
  know, real docs URL, contract line "won't show this again. type
  /welcome to revisit." `.coderoom/.welcomed` marker disambiguates
  first-run vs. return.
- **Steady-state two-line summary (#31, Variant B):** every
  subsequent launch — version + tagline, then `<project> · @roles
  · 21.5k base tokens`. Total token count is the only "live"
  signal that earns its pixels.
- **`/welcome` REPL command:** re-show the welcome on demand.
- **Project detection (#32):** `detect::scan` reads filenames at
  the project root (plus `package.json`'s `dependencies` keys for
  UI-framework discrimination), suggests roles deterministically.
  Inputs are filenames-only by design — never source contents,
  never `.git/`, never the network. The user-facing copy says
  `(local, no network)` and we keep it true.
- **`cr init` redesign (#33):**
  - Scans the project, prints what it found.
  - Probes `$PATH` for `claude` / `codex` / `gemini` and shows
    install URLs for missing engines.
  - Renders the file tree it's about to create *before* any disk
    write, then asks `proceed? [Y/n]`.
  - Suggests roles based on detected stack (Cargo.toml →
    backend + security; package.json + react → frontend;
    migrations/ → data; Dockerfile → devops; etc.). Each role
    gets a templated priors file with `{ROLE}` substituted.
  - Acknowledges existing `CLAUDE.md` with line count; doesn't
    auto-split (deferred).
  - `cr init -y` skips the prompt for dotfile / onboarding scripts.
- **`cr start` auto-init now uses the same path** with
  `InitOptions::auto()` (silent confirm, brief notice).

### Changed

- `cr init`'s default behaviour creates *all suggested roles*, not
  just `host`. Multi-role projects no longer require a follow-up
  `cr role add` per role.

### Deferred

- **Issue #34 (opt-in `cr init --advanced` wizard with ratatui).**
  Per the agent reviews' own warnings about "performing
  thoughtfulness" and the marginal value vs. the now-good
  default `cr init`, this is paused. Re-scoping discussion in the
  issue thread.

## [0.1.2] - 2026-05-09

Distribution release. No behavioral changes.

### Added

- **`@spytensor/coderoom` on npm.** Same install story as the
  underlying engines (`@anthropic-ai/claude-code`, `@openai/codex`,
  `@google/gemini-cli`):

  ```bash
  npm install -g @spytensor/coderoom
  ```

  The package is a thin wrapper. Its postinstall script downloads the
  matching pre-built binary for the user's platform from this release,
  verifies its SHA-256, and installs it on the user's PATH via npm's
  standard `bin` field. Pure Node stdlib — no runtime npm dependencies.

### Changed

- README install section restructured: npm path is now the default
  surface; direct binary tarball + cargo build are tucked into
  `<details>` disclosures so the default reader sees one thing.

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

[Unreleased]: https://github.com/spytensor/codeRoom/compare/v0.1.7...HEAD
[0.1.7]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.7
[0.1.6]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.6
[0.1.5]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.5
[0.1.4]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.4
[0.1.3]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.3
[0.1.2]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.2
[0.1.1]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.1
[0.1.0]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.0
