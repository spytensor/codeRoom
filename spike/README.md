# CodeRoom — feasibility spike scripts

Three live tests that the architecture in `docs/spike-2026-05-09.md` depends on.
Run them in order. Each is self-contained and uses a `mktemp` sandbox so it
won't touch your home config.

| # | What it tests | Cost | Time |
|---|---|---|---|
| L1 | PreToolUse hook deny under `--dangerously-skip-permissions` | ~$0.05–0.15 | ~30s |
| L2 | Long-lived `claude` session via stream-json over stdin | ~$0.10–0.30 | ~30s |
| L3 | `codex mcp-server` JSON-RPC handshake | $0 | ~5s |

Total: under $0.50, under 2 minutes wall-clock.

## How to run

```bash
cd /home/chaojiezhu/codes/codeRoom/spike

bash L1-permission-deny.sh ; echo "L1 exit: $?"
bash L2-stream-injection.sh ; echo "L2 exit: $?"
bash L3-codex-mcp.sh ; echo "L3 exit: $?"
```

Each script prints a clear `✅ PASS` / `❌ FAIL` / `⚠️ AMBIGUOUS` verdict at the
end. Exit code 0 = pass, 1 = fail, 2 = ambiguous (read the output).

## What I'm hoping you see

**L1**:
```
permission_denials length: 1
✅ PASS: permission_denials populated, no proof-file written.
```

**L2**:
```
unique sessions:  1
cache stats per turn (cache_read / cache_creation):
  turn: read=0     creation=20000  cost=0.06
  turn: read=20000 creation=0      cost=0.001
  turn: read=20020 creation=0      cost=0.001
✅ PASS: single session, 2 cache hits across 3 turns.
```

**L3**:
```
initialize protocolVersion: 2024-11-05
tools/list count:           5
✅ PASS: codex mcp-server speaks MCP, exposes 5 tool(s).
```

## If anything fails

- **L1 fail** (`permission_denials` empty AND `PROOF_RAN_DESPITE_DENY` exists)
  → permission gate design dies. We'd have to either drop `--dangerously-skip-permissions`
  and forward CC's native prompts, or never let CC run a tool we don't want
  (i.e. limit `--allowedTools`). Architectural change required.

- **L2 fail** (multiple session_ids OR no cache hits)
  → "persistent role identity = one long-lived process" doesn't pay off. Roles
  become "spawn-per-turn" instead. Brief routing still works but we lose the
  cache-warm optimization. Bigger token bills, otherwise fine.

- **L3 fail** (no MCP handshake, or no tools listed)
  → Codex adapter has to drive `codex exec` via subprocess + parse output
  instead of clean RPC. Doable but messier. Multi-engine ETA pushes out.

Paste the script output here when you're done — I'll fold the results into the
spike doc and we can move to `docs/architecture.md`.
