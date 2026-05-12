# Proposed amendments to the v0.1 constitution

`docs/architecture.md` is locked. Any change to a locked decision must land
here first as a discrete proposal, get accepted by the project owner via PR
review, and *then* be implemented in a subsequent PR.

## Format for an amendment

```markdown
## A-NNN: <Short title>

- **Status:** proposed | accepted | rejected | implemented in vX.Y.Z
- **Filed:** YYYY-MM-DD
- **Touches:** <which locked decision number(s) in architecture.md, or which section>

### Problem

<Why the current rule is wrong / inadequate.>

### Alternatives considered

<Other ways to solve the problem.>

### Proposed change

<The exact rewording of the constitution. Diff-shaped is best.>

### Migration impact

<What breaks for existing users / code if we change this.>

### Decision

<Filled in at PR review time.>
```

## Open / accepted amendments

## A-001: Adapter contract is role-handle based, not method-per-action

- **Status:** implemented in v0.1.12
- **Filed:** 2026-05-10
- **Touches:** Locked decision 3, Engine adapters / Adapter contract

### Problem

The locked architecture describes `EngineAdapter` as exposing `start`,
`send_user`, `deny_tool`, `allow_tool`, `stop`, and `cost_so_far`. The
implementation can only make `start` engine-polymorphic cleanly because
the live session owns channels, subprocess state, and engine-specific
request bookkeeping.

### Alternatives considered

Expose every method on the trait and make the REPL call adapters by role
name. That centralizes control but duplicates live-role lookup inside
each adapter. Keep the existing public `tx_user` only. That is too weak:
`/stop`, `/refresh`, Ctrl-C, and timeouts need an explicit shutdown path.

### Proposed change

Replace the contract text with:

```rust
trait EngineAdapter {
    async fn start(role_config) -> RoleHandle;
}

struct RoleHandle {
    tx_user,
    rx_events,
    stop_tx,
}
```

The handle is the live contract. `tx_user` sends paced user prompts,
`rx_events` emits CREP, and `stop_tx` requests graceful termination. Cost
reporting is derived from CREP, not polled through `cost_so_far`.

### Migration impact

Internal API only. Existing users see more reliable `/stop`, `/refresh`,
timeout, and Ctrl-C behavior.

### Decision

Accepted in #67 and implemented in v0.1.12.

## A-002: Permission and observability are per-engine capabilities

- **Status:** implemented in v0.1.12
- **Filed:** 2026-05-10
- **Touches:** Locked decisions 5, 13, 14; Engine adapters; README claims

### Problem

The v0.1 document promises wrapper-side permission gating and budget caps
for every engine. That is only fully true for Claude Code today. Codex and
Gemini have different surfaces for approvals, tool traces, and usage data.

### Alternatives considered

Keep claiming a uniform wrapper gate and fill gaps later. That misleads
users. Disable non-CC engines until parity exists. That removes the main
multi-engine value.

### Proposed change

Document a capability matrix and render unsupported values as `—`:

| Engine | Prompt isolation | Tool events | Permission mode | Cost |
| ------ | ---------------- | ----------- | --------------- | ---- |
| cc | system-prompt file | proposed/executed | wrapper hook target | per turn |
| codex | MCP base instructions | exec notifications when emitted | `—` until approval bridge exists | not reliable yet |
| gemini | `--system-instruction-file` required | stream-json `tool_use` / `tool_result` | bypass-only until hook bridge | not reliable yet |

Permission modes become explicit:

- `ask`: wrapper or engine asks before risky tools.
- `auto`: low-risk tools may proceed; risky tools ask.
- `bypass`: user opted into engine-native bypass/yolo behavior.

Gemini is refused when the installed CLI cannot isolate priors through a
system-instruction file.

### Migration impact

Users may see `—` in `cr cost` for Codex/Gemini instead of `$0.00`. This is
intentional truth-in-advertising.

### Decision

Accepted in #67 and implemented in v0.1.12.

## A-003: Concurrent REPL rendering requires a StatusRegion contract

- **Status:** N=1 StatusRegion implemented after v0.1.12
- **Filed:** 2026-05-10
- **Touches:** REPL rendering, cross-role routing

### Problem

`ThinkingSpinner` owns one terminal row. Concurrent multi-role turns need a
stable bottom status region, otherwise role spinners and event output race
for the same cursor position.

### Alternatives considered

Interleave free-form output as events arrive. That maximizes throughput but
is hard to read and impossible to snapshot. Keep sequential routing forever.
That avoids UI work but blocks v0.2 concurrency.

### Proposed change

Introduce a `StatusRegion` primitive before enabling parallel role turns:

- One slot per active role, anchored above the prompt.
- Per-role streams are FIFO.
- Cross-role dispatch announcements print at dispatch time.
- Unsupported counters render as `—`.

`StatusRegion` remains the N=1 view until the concurrent renderer lands.

### Migration impact

No user-visible change until concurrent rendering is enabled.

### Decision

Accepted for the N=1 contract and implemented after v0.1.12. Full parallel
role dispatch still lands with the concurrent renderer.

## A-004: `cr show` filtering is part of the public CLI surface

- **Status:** implemented after v0.1.12
- **Filed:** 2026-05-10
- **Touches:** CLI, CREP replay

### Problem

Unfiltered replay is sufficient for one role and a short session. It becomes
unusable for multi-role, multi-day logs.

### Alternatives considered

Tell users to pipe through `grep`. That makes the accidental JSONL shape the
public interface. Build a full TUI viewer now. Too large for v0.2.

### Proposed change

Lock this CLI shape:

```text
cr show [--role <name>] [--since YYYY-MM-DD] [--tail N]
```

Replay must warn when malformed JSONL lines were skipped.

### Migration impact

Additive CLI flags only.

### Decision

Accepted and implemented after v0.1.12.

## A-005: Auto-routing is unbounded; user halts when satisfied

- **Status:** accepted / partially implemented
- **Filed:** 2026-05-11
- **Touches:** Locked decision on "Per-thread hop-depth counter, ≥3 hops triggers escalation" in architecture.md (Failure-mode mitigations table) and the "enforces hop-depth limit" line in the layered architecture diagram.

### Problem

v0.1 capped cross-role auto-routing at one hop and the failure-mode table
locked a "≥3 cross-role hops triggers escalation" depth counter. In
practice this severs the conversational loop the room metaphor advertises:

- User → @host (turn 1)
- @host @-mentions @security → auto-route fires (turn 2)
- @security finishes its analysis and writes `@host my recommendation is...`
- @host **never wakes up** — the auto-router only iterates the *originating*
  turn's mentions, not the dispatched-turn's mentions
- The user has to manually copy @security's reply back into the prompt to
  get @host to synthesize a final answer

This is single-turn consultation, not a chat room. It also means the
quote-block, handoff-banner, and visual handoff work shipped in #98 / #99
only ever fires once per user message — exactly the case the user already
saw.

The "smart models will loop forever" risk that motivated the 1-hop cap in
v0.1 has not held up under 2026 frontier models. They reliably converge
("we're done here", "no further questions") on their own when the prompts
don't push them into adversarial roles. The escape hatches that matter
(per-role budget, Ctrl-C two-press halt, `/halt`, grounding-gate skip on
all-denied tools) are already in place and are *not* depth-based.

### Alternatives considered

1. **Keep a depth limit but raise to ~5.** Still arbitrary, still cuts off
   legitimate longer back-and-forth, still requires reasoning about which
   number is right. Tunable knobs accumulate. Rejected.
2. **Require explicit `@role:report` syntax for cross-role replies.**
   Violates LLM natural-language norms and forces every prior template to
   teach the syntax. Rejected.
3. **Surface a confirmation prompt before each follow-up hop.** Breaks the
   chat illusion; user becomes the manual router. Rejected.
4. **Unbounded with semantic guards only.** Accepted (this amendment).

### Proposed change

Replace the architecture.md failure-mode entry:

```
| Routing loops (`@a` ↔ `@b` ↔ `@a`) | Per-thread hop-depth counter ... |
```

with:

```
| Routing loops (`@a` ↔ `@b` ↔ `@a`) | Trust the model + bounded by user. Auto-router skips three cases only: self-mention (`@a` mentioning `@a`), unknown role (`@<not-running>`), and ungrounded turn (tool calls were systematically denied → reply is a guess, not data). Hop depth is unbounded. User halts a runaway chain with Ctrl-C twice or `/halt`; per-role budgets cap total spend per chain. |
```

Replace the layered diagram line "enforces hop-depth limit, inflight
tracking" with "tracks inflight turns, supervises grounding gate".

Internally, `send_and_drain` becomes a worklist over a FIFO queue of
`(role, brief)` pairs. The originating turn's explicit delegation blocks
push onto the queue; each dispatched turn's delegation blocks push too.
Plain prose mentions, tables, quotes, code fences, and pasted transcript
lines are attribution/context only. The loop ends when the queue drains,
when a turn is interrupted (`drain` returns `None`), or when the user
halts.

### Migration impact

User-visible: chains can go deeper than one hop. Most existing prompts
already encourage @-mentioning back to host for synthesis, so this turns
single-shot consultations into the closed loops users expected from the
"chat room" framing.

CREP protocol: unchanged shape. `RoleSpoke` / `TurnDispatched` /
`TurnInterrupted` already carry `turn_id` / `thread_id` / `parent_turn_id`
fields. **Caveat:** today's adapters still emit `crate::turn::LEGACY_TURN_ID`
(empty string) for these fields — wiring the IDs through every adapter is
tracked as a separate v0.2.x deliverable. The dispatcher works without
them; once the IDs land, `cr show` will be able to reconstruct chains by
walking `parent_turn_id` ancestry.

Spend: a chain can burn more tokens than before. The structural cap is
the per-role engine budget — `budget_per_role_usd` is wired into the cc
adapter (`--max-budget-usd` on spawn). **Caveat:** Codex ignores the
value pending its own `--max-*` config, and Gemini has no native budget
flag, so on those engines the only real spend bound is the user's
`Ctrl-C` and any platform-side quota. Users running unbounded routing
on chatty Codex/Gemini roles should keep that in mind.

### Decision

*(pending review)*

## A-006: Resume the prior session by default; `--fresh` opts out

- **Status:** proposed
- **Filed:** 2026-05-11
- **Touches:** v0.1 implicit behaviour: each `cr start` was a fresh engine session per role. README "Quickstart" and "Useful commands". Adapter contract (`RoleConfig`).

### Problem

Every modern AI CLI ships a resume primitive: `claude --resume <id>` /
`--continue`, `codex --resume`, `gemini` equivalents. CodeRoom does
not. Each `cr start` spawns every role as a brand-new engine session
loaded with priors but no conversation history; the user loses the
context they built up the previous time they used the room. The
session ids the wrapper *does* capture (the cc adapter parses them
out of stream-json init events and emits them on `RoleStarted`) are
discarded as soon as `cr start` exits.

In practice this means:

- Long-running projects can't accumulate working context per role
- The grounding-gate, journal, and patch infrastructure all work
  around the missing context instead of complementing it
- New users are surprised: every other CLI they have on their
  machine resumes; codeRoom alone forgets

### Alternatives considered

1. **Status quo (`fresh per start`).** Simple, predictable, every
   user can re-issue from scratch. But it makes codeRoom strictly
   worse than typing into the underlying CLI directly.
2. **`cr resume` as an explicit alias for `cr start --resume`.**
   Discoverable but means the default flow still forgets — users
   have to know to type the extra command.
3. **Default resume; explicit `--fresh` to opt out.** Matches every
   other CLI's behaviour and matches user mental model ("of course
   it picks up where I left off"). Accepted.

### Proposed change

Replace the implicit "each `cr start` is a fresh session" behaviour
with explicit per-role session persistence:

- The REPL's event forwarder writes each role's session id (emitted
  on `RoleStarted` or `RoleSessionUpdated`) to
  `.coderoom/sessions/ids/<role>.id` (sibling of the init wizard's
  `sessions/role-suggestions-dismissed` marker; the `ids/` subdir
  keeps the two from colliding). Overwrites on every new id.
- `cr` / `cr start` reads `.coderoom/sessions/ids/<role>.id` for
  each role before spawn; when present, it is plumbed into the
  `RoleConfig::resume_session_id` field and the adapter wires the
  engine's native resume mechanism (`--resume <id>` on cc,
  `codex-reply` with the prior `threadId` on codex; gemini lands in
  follow-up adapter work).
- When the engine rejects a stored id (session cleaned up
  locally, project moved disks) the REPL clears the stale id, logs
  one warning, and retries the spawn with a fresh conversation —
  the user never gets stuck in a "can't start" loop because of
  resume state.
- `cr start --fresh` (wired in PR-7) clears
  `.coderoom/sessions/ids/` before spawning so every role starts a
  brand-new conversation. The flag is the explicit escape hatch
  for "I want to forget".
- `/refresh @role` (PR-7 also extends this) clears that role's
  session id alongside its reload — the refresh semantic is
  "reload priors + start over", so its conversation history
  should reset to match.
- CodeRoom also keeps room-level snapshots under
  `.coderoom/sessions/rooms/`. Each snapshot is a set of per-role
  engine session ids. `/resume` lists them, and
  `/resume <number|id|prefix|latest>` switches the running room to
  that saved set.

Engines that do not support resume (or whose adapters haven't
plumbed the flag yet) silently degrade to a fresh session at the
engine layer; the REPL filters stale synthetic placeholders before
they reach native resume paths.

Currently wired:

- **cc**: `--resume <session-id>`. Sessions live under
  `~/.claude/projects/<hash>/sessions/`.
- **codex**: wired through `codex mcp-server`'s `codex-reply` tool.
  The first turn starts a thread with `codex`; CodeRoom persists the
  returned `threadId` via `RoleSessionUpdated`, then later turns and
  future `cr start` invocations continue with `codex-reply`.
- **gemini**: wired through `gemini --resume <session-id>`. CodeRoom
  captures the real session id from Gemini's `stream-json` init event
  via `RoleSessionUpdated`; upgraded projects discard older synthetic
  `gemini-<role>` placeholders and start fresh once before persisting
  the real id.

### Migration impact

User-facing: the next `cr start` after this amendment lands will
behave like users already expect every modern CLI to behave. There
is no migration step — first-run after upgrade has no
`.coderoom/sessions/` entries, so the first session is fresh; from
the second session onward, resume kicks in.

Storage: `.coderoom/sessions/` is already in the default
`.coderoom/.gitignore` shipped by `cr init` (it was earmarked for
this earlier). Session ids are pointers into the engine's *local*
storage at e.g. `~/.claude/projects/<hash>/sessions/` and don't
survive across machines, so committing them would be misleading.
**Caveat:** existing projects initialised before that gitignore
entry shipped may not have the line; users running unbounded
`git status` will see `sessions/ids/<role>.id` as untracked.
They're one-line opaque strings — low risk to commit but
recommended to add the gitignore line manually.

CREP protocol: unchanged. `RoleStarted` already carries
`session_id`; the wrapper just persists it now.

Failure modes: a stale session id (engine cleaned it up, project
moved disks) causes the engine to fail at spawn. The error surfaces
as a normal "spawning role X" anyhow context; the user can recover
with `cr start --fresh`.

### Decision

*(pending review)*

## Implemented amendments

Implemented amendments are marked inline with `implemented in vX.Y.Z`.

## Rejected amendments

*(none — kept for paper trail, not deleted.)*
