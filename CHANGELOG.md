# Changelog

All notable changes to CodeRoom are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed (v0.2 PR b — trust + interrupt)

- **Deleted `PER_TURN_TIMEOUT`.** The REPL no longer kills a role's
  turn after 5 minutes of wall-clock. Modern Claude / Codex / Gemini
  models self-terminate; the wrapper does not adjudicate timing. Long
  security scans and refactors that legitimately take 10–30 minutes
  now run to completion. The `RoleStopped { reason: "timed_out" }`
  variant is retired (kept on the wire for v0.1 log replay).
- **Ctrl-C is two-press now.** First press cancels every in-flight
  turn (each role's `interrupt_tx` fires). The REPL stays. A second
  press within 2 seconds force-stops all roles and exits — the
  v0.1 single-press behaviour. Documented in splash + `docs/architecture.md`.
- **Codex stdio idle watchdog bumped from 6 to 10 minutes** and
  reframed in code comments as a "stdio protocol watchdog" (per
  `docs/v0.2-trust-and-interrupt.md` § B): fires only on engine
  silence, not on slow-but-active model work. cc and gemini idle
  watchdogs are scoped to PR c since their cancel paths already
  terminate via `stop_tx`.

### Added (v0.2 PR b — trust + interrupt)

- **`/halt` and `/halt @role`** at the prompt. `/halt` cancels every
  in-flight turn; `/halt @role` cancels just one. Roles stay alive —
  only the turn ends. Plumbed through `interrupt_tx` to all three
  adapters: codex (MCP `notifications/cancelled`), gemini (SIGTERM
  the per-turn child), cc (emit `TurnInterrupted` for the drain to
  honour; cc subprocess keeps running per § F.1's spike-pending plan).
- **Cancel SLO of 5 seconds.** If an adapter does not produce a
  turn-final event within 5s of `/halt` or Ctrl-C, the REPL escalates
  by removing the role and force-stopping its process via `stop_tx`,
  with a clear diagnostic. Per `docs/v0.2-trust-and-interrupt.md` § H.1.
- **Adapters emit `CrepEvent::TurnInterrupted`** on cancel: codex
  detects the user-cancel branch via a shared flag and emits the
  event instead of an error `RoleSpoke`; gemini flushes its streaming
  accumulator into `partial_text` and parses `partial_mentions`
  (REPL surfaces these as a hint but never auto-routes them per
  § H.3); cc emits the event directly so the drain unblocks even
  while the cc subprocess keeps running.
- **`docs/architecture.md` constitution amendments** in lockstep
  with PR b (§ K): Role Invariance Principle bullet 2 lists
  `/halt`/`/stop`/`/refresh` with auto-routing-as-delegation
  footnote; CREP table covers all v0.2 events; hop-depth limit
  restated per `thread_id`; REPL command list adds `/halt` and
  Ctrl-C two-press.

### Added

- v0.2 plumbing **(wire format only — no producers yet; PR b lights
  them up).** Every CREP event tied to a specific turn now carries
  `turn_id` and `thread_id` (or `Option<turn_id>` on `RoleStopped`,
  serialized only when `Some`, for crash-during-turn attribution). Two
  new wire variants — `TurnDispatched` and `TurnInterrupted` — define
  the shape PR b's REPL will emit when it dispatches a turn or honours
  `/halt`. v0.1-shaped `messages.jsonl` lines deserialize unchanged
  thanks to `#[serde(default)]` on every new field. See
  `docs/v0.2-trust-and-interrupt.md` § D.
- New `crate::turn` module with monotonic `new_turn_id` (`tu-` prefix)
  and `new_thread_id` (`th-` prefix) generators. The prefixes are
  deliberately disjoint so a `\btu-` grep only matches turn ids and
  `\bth-` only thread ids — the earlier `\bt-` collision is gone.
- `RoleHandle` exposes a new `interrupt_tx: mpsc::Sender<TurnId>` for
  turn-level cancellation. The codex adapter wires it end-to-end with
  an explicit in-flight `tools/call` id tracker (no inferring from
  `pending`, so initialize ids and stale ids never get cancelled) and
  emits the MCP 2024-11-05 `notifications/cancelled` shape; gemini
  wires it via SIGTERM on the per-turn child while a streaming
  accumulator captures whatever partial output was produced before
  the kill. cc keeps a stub drain pending the
  `spike/L4-cc-interrupt.sh` probe in PR b. The drain channels are
  bound but **dormant** — PR a never produces interrupt traffic, so
  no behavior change reaches users.
- `Engine::session_kind()` returns the new `SessionKind` enum
  (`SessionBound` for cc, `StatelessDispatch` for codex/gemini), so
  PR b's per-role queue can decide whether queueing preserves cached
  state (cc) or just schedules fresh dispatches (codex/gemini).
- cc work-title dedup is now keyed on `turn_id` (a `HashSet<TurnId>`
  per reader) instead of a single boolean, so PR b's pipelined cc
  turns won't lose the second turn's first title to the first turn's
  carry-over flag (per `docs/v0.2-trust-and-interrupt.md` § F.4).

### Fixed

- Codex roles now stream WorkCard steps in real time. The adapter was listening
  for `notifications/exec_command_*` methods, but codex 0.130+ wraps every
  lifecycle update inside `codex/event` with the variant in `params.msg.type`.
  Long-running codex turns no longer look frozen — `exec_command_begin/end`
  fill in the same `ToolCallProposed/Executed` events that drive cc work cards.
- The codex JSON-RPC request timeout is now an **idle** timeout: every
  `codex/event` for the in-flight request resets the deadline. Long but
  productive turns (security scans, deep refactors) are no longer killed at
  the old six-minute hard cap; only a fully wedged server gets cut off.

## [0.1.18] - 2026-05-10

### Added

- Role work now renders through WorkCards with engine-neutral `cr-task` titles
  and clean separation between role work metadata and role chat text.
- Claude Code, Codex, and Gemini adapters now share the same work-title parsing
  path, with Codex/Gemini marked as partial trace sources where appropriate.

### Fixed

- Timed-out Codex and Gemini turns now terminate the active engine process
  instead of allowing stale tool output or replies after the REPL has returned.
- `cr show` normalizes legacy replies that embedded `cr-task` blocks, and
  mention parsing no longer treats email addresses as role mentions.

## [0.1.17] - 2026-05-10

### Fixed

- Codex `permission_mode="bypass"` now disables Codex's own command sandbox
  (`danger-full-access`) instead of pairing `approval-policy=never` with
  `workspace-write`, so yolo/bypass roles do not hang behind an unavailable
  Linux sandbox.
- Codex subprocess shutdown now signals the spawned process group, so killing
  the npm wrapper after a turn timeout does not leave the vendor binary running
  as an orphan.

## [0.1.16] - 2026-05-10

### Fixed

- Codex session-scoped approval choices now update CodeRoom's permission
  policy and are reused for later Codex MCP approval requests.
- Active-turn permission prompts now cancel their blocking key reader when the
  role turn times out or is interrupted, so raw mode is not left behind by an
  orphaned prompt reader.
- README, architecture notes, and `/help` now describe the current Codex
  approval bridge and `@role` completion keys instead of the old bypass-only
  behavior.

## [0.1.15] - 2026-05-10

### Changed

- Split the oversized REPL and init modules into focused submodules for
  command parsing, input, rendering, splash/status UI, log replay, turn
  draining, init labels, init rendering, init writes, and tests. This keeps
  the user-facing behavior unchanged while making the codebase safer to patch.

### Fixed

- Interactive REPL input now uses a terminal raw-mode line editor on TTYs, so
  deleting CJK / wide characters no longer leaves stale glyphs behind.
- Ctrl-C at the REPL prompt is handled immediately by the input layer and no
  longer requires pressing Enter before CodeRoom interrupts and shuts roles
  down.

## [0.1.14] - 2026-05-10

### Fixed

- Claude Code hook settings tempfiles now stay alive for the full role
  lifetime, so `cr start` no longer leaks `Settings file not found` warnings
  after the boot dashboard.
- Claude Code stderr lines are now hidden from the default REPL output and
  remain available through debug logging when diagnosing adapter issues.

## [0.1.13] - 2026-05-10

### Fixed

- Existing Codex and Gemini roles created before per-role permission modes
  now start with `permission_mode="bypass"` when they omit a role-level
  override, instead of inheriting project-wide `ask` and aborting `cr start`.
  Explicit `ask` or `auto` on those engines still fails fast until their
  approval bridges exist.
- The boot dashboard no longer displays the literal `model` placeholder for
  Codex or Gemini roles without a configured model.
- Startup no longer prints terminal truecolor diagnostics by default; use
  `CODEROOM_TERMINAL_PROBE=1 cr` when collecting color-rendering reports.

## [0.1.12] - 2026-05-10

### Added

- `cr show` now supports `--role`, `--tail`, and `--since` filters for
  focused event-log replay.
- Gemini `stream-json` `tool_use` / `tool_result` events now map into
  CREP `ToolCallProposed` / `ToolCallExecuted` events.
- `@all <text>` broadcasts a prompt to every running role, and `/host <role>`
  switches the host role for the current REPL session.
- `cr role host <name>` persists a new project host role.
- `cr compact <role>` appends archived patches and old journals into a
  deterministic compacted-history section in that role's priors.
- `cr config get <key>` and scalar `cr config set <key> <value>` cover the
  common config edits without opening an editor.
- Boot dashboard, init wizard, and CREP renderer snapshots now lock down the
  terminal surfaces most likely to regress.

### Changed

- Default host, specialist, and shared priors now describe the CodeRoom
  protocol directly, including `From @role`, `/patch`, `/journal`, host, and
  peer context.
- Role priors now include a team roster when composed, so each spawned role
  knows the configured host and peers.
- Gemini roles require a CLI that advertises `--system-instruction-file`; the
  old inline-priors fallback is only available behind
  `CODEROOM_GEMINI_UNTRUSTED_PRIORS=1`.
- Engine capability gaps are documented and rendered honestly: unsupported
  cost and permission fields use `—` instead of fake zeroes or implied parity.
- Integration workflow now runs on a weekly schedule in addition to manual
  dispatch.

### Fixed

- Ctrl-C now stops running roles during active turns, and the raw-mode init
  wizard restores the terminal before exiting on SIGINT/SIGTERM.
- `/stop`, `/refresh`, Ctrl-C, and per-turn timeouts now signal role adapters
  explicitly and terminate child processes instead of leaking subprocesses.
- Claude Code and Codex adapters drain stderr so noisy child processes cannot
  block on a full stderr pipe.
- Claude Code turn pacing now waits for a `RoleSpoke` / `RoleStopped` boundary
  before accepting the next prompt, avoiding accidental turn merges.
- Codex pending RPC requests are cleaned up on timeout / disconnect, tool
  notifications are translated into CREP tool events, and the adapter stays on
  `approval-policy=never` until a real approval bridge exists.
- `.coderoom/messages.jsonl` writes are single-record appends under an
  advisory process lock; replay reports malformed lines instead of silently
  dropping corruption, and second-session lock failures now explain what to
  close.
- `cr cost` excludes unsupported engines from numeric totals instead of
  reporting `$0.00` when an engine never supplied reliable usage data.
- Journal writes and role spawning no longer do blocking file work directly on
  the async REPL path.

## [0.1.11] - 2026-05-10

### Fixed

- Refreshed the terminal role palette with the OKLCH-selected eight-color
  set; `@host` is always lavender, and generated role colors now use the
  new lavender / jade / coral / rose / sky / blossom / honey / teal ramp.
- The boot dashboard no longer uses yellow as decoration. Box borders now
  share the dim `#6a6a6a` rule color, panel headings use bold `#f0f0f0`,
  and all dashboard borders use single-line box drawing.
- Startup now prints a one-line `TERM` / `COLORTERM` truecolor diagnostic
  to stderr so bad terminal color negotiation is visible in user reports.
- Codex-backed roles no longer display the literal placeholder `model` in
  ready banners when the adapter does not report a concrete model name.

### Changed

- Removed `cr update` from the in-REPL welcome dashboard because users
  cannot run shell-level `cr ...` commands from inside the REPL prompt.

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

[Unreleased]: https://github.com/spytensor/codeRoom/compare/v0.1.18...HEAD
[0.1.18]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.18
[0.1.17]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.17
[0.1.16]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.16
[0.1.15]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.15
[0.1.14]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.14
[0.1.13]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.13
[0.1.12]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.12
[0.1.11]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.11
[0.1.10]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.10
[0.1.9]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.9
[0.1.8]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.8
[0.1.7]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.7
[0.1.6]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.6
[0.1.5]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.5
[0.1.4]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.4
[0.1.3]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.3
[0.1.2]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.2
[0.1.1]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.1
[0.1.0]: https://github.com/spytensor/codeRoom/releases/tag/v0.1.0
