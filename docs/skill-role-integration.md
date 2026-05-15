# Skill-Role Layered Integration

This document describes how engine-native skills compose into CodeRoom's role
model without violating locked decision § 1 (wrapper, not runtime) or the
four guardrails defined in `docs/core-philosophy.md`.

This is a design contract. The amendment that locks the mechanism is A-013
in `docs/proposed-amendments.md`.

## Problem

Today CodeRoom spawns claude / codex / gemini subprocesses without any
isolation of the engine's skill discovery path. Every role inherits the
user's global skill pool (`~/.claude/skills/`) plus the project-local
`.claude/skills/`. Role differentiation is purely at the priors layer.

This is a hole. Skills are capabilities. `@frontend` having silent access to
a `db-migration` skill is not role partitioning; it is the same global
namespace pathology the priors layer was built to solve.

## What "role" and "skill" are

| Concept | Definition                                                 | Address       | Lifetime    |
| ------- | ---------------------------------------------------------- | ------------- | ----------- |
| Role    | Perspective. Priors plus viewpoint plus chat identity      | `@<role>`     | Session     |
| Skill   | Parameterized capability. Named callable procedure         | In-turn call  | Turn-scoped |

These are orthogonal axes. A role chooses which skills it surfaces, the same
way a Rust module declares which items it imports. The role does not
implement the skill; the skill does not have a viewpoint.

## Non-goals

- No CodeRoom-side skill runtime. Skill body execution stays inside the
  engine subprocess. Locked decision § 1.
- No cross-engine skill homogenization. If codex or gemini lack a native
  equivalent, that is surfaced as `—` (per A-002), not faked with
  prompt-injected pseudo-skills.
- No new priors layer. Skills compose along the existing kernel / shared /
  role lattice. No `skills.md` or analogous flat file.

## Layered pool

Skills live in a fixed three-layer directory tree, all under
`.coderoom/skills/`, mirroring the priors layering. Role-private skills
sit under `.coderoom/skills/roles/<role>/` rather than nested inside the
existing `.coderoom/roles/<role>.md` priors file — this preserves the
locked role-priors file layout from `architecture.md` § Knowledge model.

```
.coderoom/
├── skills/
│   ├── kernel/                # built-in. every role sees these.
│   │                          # CodeRoom protocol helpers
│   │                          # (e.g. journal-write).
│   ├── shared/                # project-wide pool. role allowlist gates
│   │                          # which subset a given role sees.
│   └── roles/
│       └── <role>/            # role-private. only this role sees these.
└── roles/
    └── <role>.md              # unchanged. existing priors file.
```

Each skill directory follows the engine's native skill format (currently a
`SKILL.md` with frontmatter for claude). CodeRoom does not invent a new
format.

## Allowlist mechanism

Each role's `.coderoom/roles/<role>.md` declares which skills it surfaces in
its frontmatter:

```yaml
---
skills:
  kernel: ["*"]                  # kernel is opt-out, not opt-in
  shared: [simplify, claude-api]
  deny: [db-migration]           # deny trumps any allowlist
---
```

Default behavior when a role file omits the `skills:` block:

- `kernel`: all visible. Kernel skills are CodeRoom protocol helpers; they
  exist precisely because every role needs them.
- `shared`: none visible. Shared skills must be explicitly opted into so a
  new role does not silently inherit project-wide capabilities.
- Role-private: all visible. Anything in `skills/roles/<role>/` is private to
  that role by construction.

This default makes capability inheritance an explicit user act, matching the
priors stance: priors are not inherited from shared without intent.

## Spawn-time materialization

The layered pool and the allowlist contract are locked by A-013. The
materialization mechanism described below is **non-locking** — it is the
engineering plan, not the constitutional commitment. Engines may expose
better surfaces (a `--skills-dir` flag, an env var) and the mechanism
adapts. The contract is "the role sees exactly the resolved skills",
not "the role's HOME is redirected".

On role spawn, the role manager:

1. Resolves the role's allowlist against current kernel and shared
   directory contents, then walks role-private skills.
2. Materializes a per-role skill view at
   `$XDG_RUNTIME_DIR/coderoom/<session>/<role>/skills/` containing
   symlinks to the resolved skills. On platforms where symlinks are
   unreliable, falls back to copies.
3. Points the engine subprocess at that view using the least-invasive
   engine-native mechanism available at implementation time.
4. Hashes the materialized tree; the digest flows into `priors_hash`
   (A-008).

Per-engine wiring (engineering plan, not locked):

- **claude**: prefer a native flag or environment variable that targets
  the materialized view directly. A `HOME` redirect is the fallback only
  if no narrower mechanism exists; if used, it shadows `~/.claude/skills/`
  for the subprocess without touching the user's real `~/.claude/`.
- **codex**: codex has no native skill concept comparable to claude's.
  Where codex exposes equivalent surfaces (prompts, commands, MCP
  servers), CodeRoom translates the allowlist into codex's native
  discovery. Where codex lacks an equivalent, that capability is rendered
  `—` in the capability matrix.
- **gemini**: gemini lacks a native skill mechanism today. Skills are
  reported as unsupported for gemini roles. CodeRoom does not fake the
  surface with priors injection.

On role stop, the materialized view is removed. On crash, `cr doctor`
reaps orphan session directories.

## Auditability

Every skill invocation visible to CodeRoom (claude stream-json, codex MCP
notification where applicable) becomes a first-class CREP event:

```
SkillInvoked { role, skill_name, skill_sha, priors_hash, turn_id, thread_id }
```

`priors_hash` is required per A-008 so skill invocations are anchored to
the priors composition that surfaced them.

This sits at the same level as `ToolCallProposed`. It is filtered into
`cr show` the same way as other tool events.

Engines that do not surface skill invocation events emit no `SkillInvoked`.
This is the same truth-in-advertising discipline as A-002: an event we
cannot observe does not get faked.

## Rejected architectures

Three alternatives were considered during the design synthesis and dropped.
Listed for paper trail.

### A. CodeRoom as skill broker

CodeRoom would parse `<skill name=...>` tags in role output, execute the
skill body itself, and feed results back to the role.

Rejected. Violates locked decision § 1 (wrapper, not runtime). This would
make CodeRoom a tool execution path independent of the engine's permission
contract, breaking the entire permission model.

### B. Per-role full sandbox without kernel layer

Each role gets its own complete skill directory by copy or symlink, with no
shared layer. Common skills duplicate per role.

Rejected. Loses the layered-priors symmetry. Common skills drift out of
sync across roles; users have to maintain N copies of `simplify`. There is
no mechanism to enforce "every role has access to `journal-write`" — the
kernel capability becomes optional.

### C. Soft prompt-level allowlist

Skills stay in the user's real `~/.claude/skills/`. CodeRoom only injects a
system prompt sentence saying "you may only call [list]".

Rejected. Soft isolation. A prompt injection across roles (the exact threat
A-007 defends against) can override the sentence. Also fails for gemini,
which has no skill system to gate. Violates the strong-guarantee posture of
the four guardrails.

## Migration impact

Existing projects without `.coderoom/skills/` continue to work: skills
resolve from the engine's native default discovery path. Adding the layered
tree is opt-in per project. `cr skill init` scaffolds the directory and
seeds role markdowns with empty `skills:` blocks.

`priors_hash` (A-008) only mixes in skill content when `.coderoom/skills/`
exists. Projects without skills see no change in `priors_hash` computation.

## Verification

Any PR landing this contract must include:

- Unit tests for allowlist resolution: kernel default-on, shared default-off,
  role default-on, deny overrides.
- Spawn-time materialization tests on Linux (symlink path) and macOS
  (HOME-shadow path).
- Capability matrix snapshot for the per-engine support level.
- A `cr doctor` test for orphan session directory cleanup.
- Negative test: a role without an explicit shared allowlist must not see
  any project-shared skill in the materialized view.
