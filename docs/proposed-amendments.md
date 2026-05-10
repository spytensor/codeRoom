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

- **Status:** proposed
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

Pending review.

## A-002: Permission and observability are per-engine capabilities

- **Status:** proposed
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
| gemini | `--system-instruction-file` required | not reliable yet | bypass-only until hook bridge | not reliable yet |

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

Pending review.

## A-003: Concurrent REPL rendering requires a StatusRegion contract

- **Status:** proposed
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

`ThinkingSpinner` remains the N=1 view until the concurrent renderer lands.

### Migration impact

No user-visible change until concurrent rendering is enabled.

### Decision

Pending review.

## A-004: `cr show` filtering is part of the public CLI surface

- **Status:** proposed
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

Pending review.

## Implemented amendments

*(none — moved here once the implementing PR ships.)*

## Rejected amendments

*(none — kept for paper trail, not deleted.)*
