#!/usr/bin/env bash
# L3 — verify codex's mcp-server mode: speak JSON-RPC over stdio,
# initialize, and list tools. This is how CodeRoom's wrapper would
# drive Codex — same shape as we'd use for any other MCP-speaking
# engine.
#
# What it does:
#   1. Spawns `codex mcp-server`
#   2. Sends MCP initialize → expects InitializeResult
#   3. Sends notifications/initialized
#   4. Sends tools/list → expects array of tools
#
# Pass criteria:
#   - InitializeResult comes back with protocolVersion + serverInfo
#   - tools/list returns a non-empty array (codex exposes its tools as MCP tools)
#
# Fail criteria:
#   - codex mcp-server crashes / hangs
#   - Returned shapes don't match MCP 2024-11-05 spec
#   - tools/list is empty or errors
#
# Cost: $0 (no model calls — handshake only)

set -euo pipefail

SANDBOX="$(mktemp -d -t coderoom-L3-XXXXXX)"
trap 'echo "sandbox: $SANDBOX"' EXIT

INPUT="$SANDBOX/rpc-in.jsonl"
OUTPUT="$SANDBOX/rpc-out.jsonl"

cat > "$INPUT" <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"coderoom-spike","version":"0.0.1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
EOF

echo "===== L3: codex mcp-server JSON-RPC handshake ====="
echo "sandbox: $SANDBOX"
echo

# Run with a 10s timeout — handshake should be near-instant.
# Portable timeout: GNU coreutils' `timeout` ships on Linux but not on a
# default macOS install. `perl -e 'alarm shift; exec @ARGV'` is on every
# macOS/Linux box, and SIGALRM transfers to the exec'd child so the child
# is the one that dies on timeout. Exit 142 (128 + SIGALRM) = timed out.
perl -e 'alarm shift; exec @ARGV' 10 codex mcp-server < "$INPUT" > "$OUTPUT" 2> "$SANDBOX/stderr.log" || ec=$?
ec=${ec:-0}

echo "exit code: $ec"
echo "stderr:"
sed 's/^/  /' "$SANDBOX/stderr.log" | head -20
echo
echo "stdout (first 1KB):"
head -c 1024 "$OUTPUT" | sed 's/^/  /'
echo
echo

echo "===== verdict ====="

INIT_OK=$(jq -r 'select(.id==1) | .result.protocolVersion' "$OUTPUT" 2>/dev/null | head -1)
TOOLS_COUNT=$(jq -r 'select(.id==2) | .result.tools | length' "$OUTPUT" 2>/dev/null | head -1)

echo "initialize protocolVersion: ${INIT_OK:-<none>}"
echo "tools/list count:           ${TOOLS_COUNT:-<none>}"
echo

if [[ -n "$INIT_OK" && "${TOOLS_COUNT:-0}" -gt 0 ]]; then
  echo "✅ PASS: codex mcp-server speaks MCP, exposes $TOOLS_COUNT tool(s)."
  echo
  echo "tool names:"
  jq -r 'select(.id==2) | .result.tools[].name' "$OUTPUT" 2>/dev/null | sed 's/^/  /'
  exit 0
else
  echo "❌ FAIL or unclear. Inspect:"
  echo "  - $OUTPUT (raw stdout)"
  echo "  - $SANDBOX/stderr.log"
  exit 1
fi
