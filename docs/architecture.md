# CodeRoom — v0.1 Architecture

This is the constitution for v0.1. Anything in here is locked unless explicitly
revisited via amendment. New ideas during implementation that contradict this
doc go to `docs/proposed-amendments.md` first, not into code.

Live spike that grounds this design: `docs/spike-2026-05-09.md`. Everything
below assumes the three GREEN findings of that spike still hold.

## What CodeRoom is

A coordination shell that runs multiple agent CLI sessions as named "roles"
(e.g. `@backend`, `@security`, `@frontend`) sharing a single chat-style
terminal stream. Each role is a separate CLI subprocess loaded with a
role-specific knowledge file (its **priors**). One role is designated the
**host** — it catches any message the user types without an explicit `@`.
Other roles are addressed by `@`-mention. Cross-role routing happens when one
role writes `@x` in its reply.

The CLI binary is `cr`.

## Role Invariance Principle

CodeRoom's roles are not replacements for the engineering roles in your
organization. They are perspectives — borrowed eyes attached to priors —
that you summon while you still hold the work. The user is the single
accountability anchor: every change of state in the project repo lands
through code that the user reviews, accepts, and commits.

Concretely, v0.1 enforces this by construction:

- **No autonomous execution path.** Roles propose; tool execution still
  passes through the engine's permission contract, which the wrapper gates.
- **The user is the only routing source.** Roles can `@` each other in
  their replies, but the user alone introduces new threads and can
  `/stop` or `/refresh` any role at any time.
- **Journal citations are mandatory.** A role's "what I learned today" is
  rejected without a transcript anchor or repo file path. Roles cannot
  self-promote belief into long-term memory.
- **Patches are explicit and user-written.** Promoting a patch into base
  priors is always a user action (`/patch promote`, v0.2). Roles never
  rewrite their own priors.

In a future team-usage mode (v0.x), individual roles will be assigned a
human `owner` who acts as the source-of-truth for that role's priors.
That decision is logged in `docs/proposed-amendments.md`, not in v0.1.

## Why

A single `CLAUDE.md` is a global namespace. As projects accumulate years of
conventions, one-off compliance rules, and decisions buried in commit messages
or comments, a single file forces three problems:

- **Bloat** — grows linearly with project age
- **Attention dilution** — long context reduces the model's effective use of
  any individual rule
- **No expressivity** — "this rule only matters to backend" cannot be said

Real organizational knowledge is partitioned by role. CodeRoom mirrors that
partition: each role carries only its own priors, addresses only what it
knows, and contributes its viewpoint to the shared chat.

## Non-goals (explicit, in priority order)

These are deliberately out of scope. Implementations that drift toward them
violate the constitution.

- **No new agent runtime.** Tool execution, file editing, sandboxing, MCP —
  all stay inside the engine subprocess. We do not re-implement these.
- **No automatic task router.** The user `@`-mentions roles. No "smart" PM
  agent reads requirements and decides who participates. (Models *can* `@`
  each other within their replies — that is delegation, not routing logic
  that we own.)
- **No permission sandbox of our own.** We forward permission decisions
  via the engine's hook contract; we do not invent a new sandbox.
- **No paid SaaS, no hosted version.** v0.1 is a local CLI. v1 too.
- **No general LLM ops platform.** This is a focused tool, not a framework.

## High-level architecture

```
┌──────────────────────────────────────────────────────────┐
│                     cr (CLI binary)                       │
├──────────────────────────────────────────────────────────┤
│  REPL                                                     │
│    parses @mentions, /commands, free text                 │
├──────────────────────────────────────────────────────────┤
│  Message Bus                                              │
│    central event log, append-only JSONL                   │
│    speaks the CodeRoom Event Protocol (CREP)              │
├──────────────────────────────────────────────────────────┤
│  Role Manager                                             │
│    spawn / pace / refresh / stop role processes           │
│    composes system prompt from priors + patches + journal │
│    enforces hop-depth limit, inflight tracking            │
├──────────────────────────────────────────────────────────┤
│  Engine Adapters                                          │
│    claude-code → stream-json IO + hooks                   │
│    gemini      → stream-json IO + (CC-compatible) hooks   │
│    codex       → MCP JSON-RPC over stdio                  │
├──────────────────────────────────────────────────────────┤
│  Patch Store · Journal Store · Transcript Archive         │
└──────────────────────────────────────────────────────────┘
        │              │              │
        ▼              ▼              ▼
   ┌────────┐    ┌────────┐    ┌────────┐
   │ claude │    │ codex  │    │ gemini │
   │@backend│    │@security    │@frontend
   └────────┘    └────────┘    └────────┘
        │              │              │
        └──────────────┴──────────────┘
                       ▼
                  Project Repo
```

## Locked design decisions

Each is a one-liner with rationale. Detailed sections follow.

1. **Wrapper, not runtime.** Tool execution belongs to the engine.
2. **Multi-engine from day 1.** Each role pins its engine
   (`engine: cc | codex | gemini`). This is the actual differentiator vs.
   Agent Teams / Agentrooms / OpenCode — none of them let you mix.
3. **Engine adapters emit CREP.** A normalized internal event protocol
   (~6 event types) so the rest of the wrapper is engine-agnostic.
4. **Live-session pacing.** One long-lived subprocess per role, fed via
   stream-json over stdin. Wrapper waits for each `result` event before
   writing the next user message. Verified in spike L2.
5. **Permission modes are explicit.** `permission_mode = "ask" | "auto" |
   "bypass"` is resolved per role. Claude Code uses a settings-injected
   PreToolUse hook. Codex and Gemini are bypass-only until CodeRoom can
   supervise their approvals. For compatibility with older generated
   configs, Codex/Gemini roles that omit a per-role permission mode resolve
   to bypass; explicit ask/auto still fail fast.
6. **Trust-the-model routing.** Each role's system prompt includes a team
   roster. When a role writes `@x` in its reply, the wrapper routes a
   focused brief to `x`'s session. No syntactic protocol.
7. **CC-style brief routing.** Cross-role `@` does NOT forward full chat
   history. Wrapper sends a brief = the `@`-paragraph + thread sticky +
   pointer to `.coderoom/transcripts/`. Receiving role reads more on demand.
8. **Equal repo R/W for all roles.** Differentiation is priors, not
   permission walls.
9. **Host role catches un-addressed text.** The user designates one role as
   `host` in `config.toml`. Bare text in the REPL goes to the host. `@<role>`
   still routes explicitly. The host is a normal role with its own priors —
   it routes onward by deciding to `@` other roles itself. This is *not*
   auto-PM-router: the host's behavior is whatever its priors say; the
   wrapper just does the un-addressed-text → host mapping.
10. **Roles are re-instantiable, not permanent.** A role's identity =
    priors + patches + journal + last transcript summary. `/refresh` rebuilds
    a fresh subprocess from those files when a session degrades.
11. **Daily journal layer.** `/journal/YYYY-MM-DD/<role>.md` is written by
    the role itself at session end. Last 7 days auto-loaded into context.
12. **Manual patch promotion in v0.1.** No automatic priors evolution.
    `cr review` (clustering + propose-promote) lands in v0.2.
13. **Hard caps from day 1.** 50 patches per role, FIFO archive on overflow.
    Hop-depth ≥3 across roles in one thread escalates back to user.
    `--max-budget-usd` ceiling on every engine call.
14. **Default-deny on hook failure.** If the PreToolUse hook script crashes,
    the wrapper treats it as deny + alerts the user. Never silent-approve.

## CodeRoom Event Protocol (CREP)

The internal lingua franca. Every engine adapter translates native events to
these. UI, message bus, and patch logic only ever see CREP.

| Event              | When fired                                       | Key fields                                       |
| ------------------ | ------------------------------------------------ | ------------------------------------------------ |
| `RoleStarted`      | Subprocess up, system prompt loaded              | role, engine, model, session_id, priors_hash     |
| `RoleSpoke`        | Role emitted a final assistant turn              | role, text, mentions[], cost_usd, cache_read     |
| `ToolCallProposed` | PreToolUse fired                                 | role, tool_name, tool_input, tool_use_id         |
| `ToolCallExecuted` | PostToolUse fired                                | role, tool_use_id, ok, output_summary            |
| `PermissionDenied` | Wrapper denied via PreToolUse                    | role, tool_name, tool_input, reason              |
| `RoleStopped`      | Subprocess exited or `/refresh` invoked          | role, reason: completed/refreshed/crashed/budget |

`RoleSpoke.mentions` is the parsed list of `@x` references found in `text`.
The wrapper uses this to route briefs.

JSONL append-only log at `.coderoom/messages.jsonl`. The full transcript view
the user sees is just a render of this stream filtered to events that humans
care about (RoleStarted, RoleSpoke, ToolCallProposed/Executed summary,
PermissionDenied, RoleStopped).

## Engine adapters

### Claude Code adapter

- Spawn: `claude --print --input-format=stream-json --output-format=stream-json
  --verbose --dangerously-skip-permissions --append-system-prompt-file <priors>
  --settings <hooks-config> --max-budget-usd <cap>` when permission mode is
  `ask` or `auto`; `bypass` omits the hook settings.
- Input: stream-json messages on stdin. `content` must be array of blocks
  (`[{"type":"text","text":"…"}]`), not bare string.
- Output: stream-json on stdout (`system`, `assistant`, `result`,
  `rate_limit_event`).
- Pacing: write next user message only after reading the `result` event for
  the previous turn.
- Permission: settings-injected PreToolUse hook command writes JSON
  `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny|allow|ask","permissionDecisionReason":"…"}}`
  to stdout. Hook input arrives on stdin as JSON. `/allow <tool>` and
  `/deny <tool>` update the session policy file read by that hook. In
  Claude Code's non-interactive stream mode, `ask` is represented as a
  safe denial in `permission_denials`; the user can `/allow` and retry.
- Session ID: extracted from `system.subtype="init"` event at start.
- Cost: per-turn `result.total_cost_usd`. Wrapper aggregates per role per day.

### Codex adapter

- Spawn: `codex mcp-server` over stdio.
- Wrapper acts as MCP client. Initialize → tools/list → tools/call.
- Two tools available: `codex` (start session) and `codex-reply` (continue).
- Permission: CodeRoom currently starts Codex only when
  `permission_mode="bypass"` and sends `approval-policy="never"` in the
  `tools/call` payload. Missing per-role permission modes on Codex roles
  resolve to bypass for older generated configs; explicit `ask` and `auto`
  fail fast until CodeRoom can answer Codex approval requests over MCP.

### Gemini adapter

- Spawn: `gemini -p "<prompt>" --output-format stream-json -y`.
- Prompt isolation: CodeRoom requires a Gemini CLI that advertises
  `--system-instruction-file`. The unsafe inline-priors fallback is gated
  behind `CODEROOM_GEMINI_UNTRUSTED_PRIORS=1`.
- Output: `message` events become `RoleSpoke`; `tool_use` and `tool_result`
  become CREP `ToolCallProposed` / `ToolCallExecuted`.
- Permission: Gemini remains bypass-only in CodeRoom until a hook or approval
  protocol can be supervised by the wrapper. Starting a Gemini role in `ask`
  or `auto` fails fast with a configuration error.

### Adapter contract

Every adapter exposes:

```
trait EngineAdapter {
    fn start(role_config) -> RoleHandle;          // emits RoleStarted
    fn send_user(role, text) -> ();                // pace internally
    fn deny_tool(role, tool_use_id, reason);       // wrapper's veto
    fn allow_tool(role, tool_use_id);
    fn stop(role) -> ();                           // emits RoleStopped
    fn cost_so_far(role) -> Decimal;
}
```

## Knowledge model

```
.coderoom/
├── config.toml                    # default_engine, default_model, host_role, caps
├── roles/
│   └── <role>.md                  # base priors (manual edits)
├── shared.md                      # priors loaded by every role
├── patches/
│   └── <role>/
│       ├── 001-slug.md            # session-time corrections
│       ├── 002-slug.md
│       └── _archive/              # FIFO overflow (>50)
├── journal/
│   └── YYYY-MM-DD/
│       ├── <role>.md              # role-written end-of-session log
│       └── _thread-<id>.md        # project-level activity summary
├── transcripts/
│   └── YYYY-MM-DD/
│       └── <role>-<session>.jsonl # raw CREP archive (grep, not auto-loaded)
├── messages.jsonl                 # active session message bus
└── sessions/
    └── <role>.state.json          # current session_id, cost, inflight
```

A role's effective system prompt is composed at spawn time:

```
<role priors>
<shared.md>
<active patches in numeric order>
<journal entries from last 7 days for this role>
<team roster: one-paragraph blurb per other role; host is marked>
```

The team roster explicitly marks which role is host so non-host roles can
escalate back to the user via `@<host>` when they need direction.

`~/.coderoom/roles/` holds global role templates that a `cr role add backend`
can scaffold from. Project-level `.coderoom/roles/<role>.md` extends or
overrides the template.

### Patches

- Created via `/patch <role> <text>` in REPL — appended to the role's
  patches dir as a numbered file with a slug derived from the text.
- Loaded into the next `/refresh` (and at any subsequent role spawn).
- **Hard cap 50 per role.** On overflow, oldest patch is moved to `_archive/`
  and a CREP `RoleSpoke` warning is emitted.
- v0.2 adds `cr review` (cluster + propose promotion to base priors) and
  `cr patch promote/expire` lifecycle commands.

### Journals

- Written at session end by the role itself, via a one-shot prompt with a
  strict JSON schema (CC `--json-schema`, codex `--output-schema`, gemini
  response_schema). Schema enforces:
  - `decisions: [{description, transcript_anchor}]`
  - `learned: [{fact, evidence_path}]`
  - `open_questions: string[]`
- **Citation requirement.** Every `learned` entry must cite either a
  transcript line range or a repo file path. Wrapper rejects entries without
  citations and writes them to `_unverified.md` for the user to triage.
- Loaded as plain markdown into the next 7 days of role spawns.
- Hallucination defense: `cr verify` (v0.2) grep-checks claims against
  cited evidence; unverifiable claims demoted to "role believes…" tone.

### Transcripts

Raw CREP JSONL per role per session. Never auto-loaded — used for forensics,
`cr show`, and journal citations.

## Failure-mode mitigations baked into v0.1

| Failure                              | Mitigation                                                |
| ------------------------------------ | --------------------------------------------------------- |
| Brief loses thread context (HIPAA)   | Thread sticky: rolling 200-token constraint summary auto-prepended on every cross-role route. User-emphasized statements ("we use…", "must…") seed it. |
| Journal hallucination compounding    | JSON-schema-enforced citations on every `learned` entry; unverified entries quarantined |
| Patch directory bloat                | Hard 50-cap per role + FIFO archive at v0.1               |
| Routing loops (`@a` ↔ `@b` ↔ `@a`)   | Per-thread hop-depth counter. ≥3 cross-role hops triggers escalation message back to user |
| Permission gate fail-open            | Hook script defaults to deny on any error; wrapper supervises hook process and treats non-zero exit without decision-file as deny |
| Concurrency / SIGINT mid-tool        | Each role's tool calls wrapped in `.coderoom/locks/<role>.inflight`. On startup, stale inflight markers put the role in recovery mode (no new tool calls until user acknowledges) |
| Token cost runaway                   | `--max-budget-usd` ceiling per engine call. Wrapper-tracked daily aggregate per role with soft warning |
| Role identity drift over months      | v0.2 `cr review` diffs journal-self vs priors-self and surfaces contradictions |

## v0.1 scope

### CLI

```
cr init                          # initialize .coderoom/, prompt for first roles
                                 #   and which one is the host
cr role add <name> [--engine cc|codex|gemini] [--model <model>] [--host]
cr role list                     # marks the host with *
cr role rm <name>
cr role host <name>              # change which role is host
cr start                         # enter REPL; spawn all configured roles

# global commands (outside REPL)
cr show <session>                # raw transcript dump
cr cost                          # cost breakdown per role since YYYY-MM-DD
```

### REPL commands

```
<text>                           # bare text → host role (default routing)
@<role> <text>                   # send explicitly to one role
@all <text>                      # broadcast (still serial-rendered)

/patch <role> <text>             # save correction
/refresh <role>                  # tear down + respawn from priors+patches+journal
/stop <role>
/transcript <role>               # paginate latest archive
/host <role>                     # switch host for the current session only
/help
/exit
```

### Out of scope for v0.1

- `cr review` (patch clustering, promotion proposals)
- `cr verify` (journal fact-check)
- Auto routing (PM-as-router, content-based dispatch)
- Default-role for un-addressed messages
- Replay viewer (`▶ replay from start` chips in mockup)
- `cr inspect @role` per-role detail pane
- Plugin / community role marketplace
- Configuration UI (everything is `config.toml` + role markdown files)

## Open questions deferred to amendments

These are real questions but answering them now adds risk without value.

- **Codex permission proxy.** If MCP proxy interception of `tools/call` is
  too complex for v0.1, we ship Codex roles with on-request approval
  (Codex prompts directly, our wrapper observes). Decision deferred until
  first attempt at the codex adapter.
- **Per-role human owners (team mode).** When CodeRoom is used by a team,
  individual roles should have a human `owner` (e.g. the security engineer
  owns `@security`'s priors). Owner gates patch promotion and journal
  reconciliation for that role. This concretizes the Role Invariance
  Principle for multi-human use; v0.1 is single-user so it's deferred.
- **Cross-engine cache strategy.** CC's prompt cache is per-org-key. If
  the user runs CC + Codex + Gemini, we have no shared cache. Acceptable
  for v0.1; revisit if cost becomes the dominant complaint.
- **Host as proactive user-facing role.** Should the host periodically
  emit unsolicited summaries ("you've been quiet for 10 minutes; here's
  where each role is")? Worth dogfooding before committing.
- **ACP (`gemini --experimental-acp`).** Worth re-examining when adding a
  fourth engine.

## Implementation language

**Rust.** Tokio for the async subprocess + stream-json IO, `serde_json` for
event parsing, `clap` for CLI, `crossterm` for the colored REPL output. The
choice tracks four reasons:

- Single binary, no runtime, ~3–5 MB on disk, sub-10ms startup.
- Codex itself is Rust (`codex_cli_rs/0.128.0`); we sit in the same
  community + tooling ecosystem as the engines we wrap.
- Ownership/lifetimes match the system invariants we care about (one
  role-handle per spawned subprocess, exclusive write to the bus, etc.).
- I/O-bound work with a few MB working set means Rust vs Go performance
  is indistinguishable in practice — but the type system is worth the
  velocity cost on a project meant to last.

Python and Node are rejected (distribution story is worse than both
native options, and the engines we wrap are not Python/Node native).
