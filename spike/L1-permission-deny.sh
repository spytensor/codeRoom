#!/usr/bin/env bash
# L1 — verify that a PreToolUse hook returning deny actually blocks
# tool execution under --dangerously-skip-permissions.
#
# v2: use a fully benign prompt so the model doesn't self-police before
#     the hook even gets a chance to run. v1's prompt mentioned tokens like
#     "DESPITE_DENY" / "PROOF_RAN_DESPITE_DENY" which spooked the model into
#     refusing all Bash use — the hook never fired and we couldn't test.
#
# What it does:
#   1. Creates a sandbox with a hook script that denies every Bash call
#   2. Asks claude to run a totally innocuous bash command (pwd)
#   3. Inspects whether the hook was invoked AND whether the call was blocked
#
# Pass:    permission_denials non-empty AND `pwd`-output absent from result
# Fail:    permission_denials empty AND `pwd`-output present in result
# Hook-OK: permission_denials non-empty (deny mechanism works)
#
# Cost: ~$0.05–0.15

set -euo pipefail

SANDBOX="$(mktemp -d -t coderoom-L1-XXXXXX)"
trap 'echo "sandbox: $SANDBOX"' EXIT

cat > "$SANDBOX/deny-bash.sh" <<EOF
#!/usr/bin/env bash
input=\$(cat)
echo "\$input" > "$SANDBOX/last-hook-input.json"
cat <<'JSON'
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "deny",
    "permissionDecisionReason": "L1 spike (do not actually run any bash)"
  }
}
JSON
EOF
chmod +x "$SANDBOX/deny-bash.sh"

cat > "$SANDBOX/settings.json" <<EOF
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          { "type": "command", "command": "$SANDBOX/deny-bash.sh" }
        ]
      }
    ]
  }
}
EOF

echo "===== L1: permission deny under --dangerously-skip-permissions ====="
echo "sandbox: $SANDBOX"
echo

# Benign, no test-vocabulary. Model will happily try Bash for this.
PROMPT='Use the Bash tool to run the command `pwd` and tell me the working directory.'

OUT="$SANDBOX/result.json"
claude \
  --dangerously-skip-permissions \
  --settings "$SANDBOX/settings.json" \
  --add-dir "$SANDBOX" \
  --print \
  --output-format=json \
  --max-budget-usd 0.50 \
  "$PROMPT" > "$OUT"

echo "===== verdict ====="
DENIALS=$(jq -r '.permission_denials | length' "$OUT")
RESULT_TEXT=$(jq -r '.result' "$OUT")

echo "permission_denials length: $DENIALS"
echo "session_id:                $(jq -r .session_id "$OUT")"
echo "total_cost_usd:            $(jq -r .total_cost_usd "$OUT")"
echo "hook fired (input file exists)?: $([[ -f "$SANDBOX/last-hook-input.json" ]] && echo YES || echo NO)"
echo

# Heuristic: the cwd path leaks into the response only if pwd actually ran.
# Match for /tmp/, /home/, or path-like substring as evidence the tool ran.
LEAKED=0
if echo "$RESULT_TEXT" | grep -qE '(/home/|/tmp/coderoom)'; then
  LEAKED=1
fi

if [[ "$DENIALS" -gt 0 ]]; then
  echo "✅ PASS: permission_denials populated."
  echo
  echo "first denial entry:"
  jq '.permission_denials[0]' "$OUT"
  echo
  if [[ "$LEAKED" -eq 1 ]]; then
    echo "⚠️  WARNING: pwd-style output appears in reply — check whether the"
    echo "    tool ran anyway. (Could also be claude quoting the prompt.)"
  fi
  exit 0
fi

if [[ "$LEAKED" -eq 1 ]]; then
  echo "❌ FAIL: no denial recorded AND pwd output leaked into reply."
  echo "    Skip-permissions ignored our hook deny. Permission gate design dies."
  echo
  echo "claude's reply:"
  echo "$RESULT_TEXT" | head -10
  exit 1
fi

echo "⚠️  AMBIGUOUS: no denial recorded, no obvious tool output leak."
echo "    Was the hook invoked at all? Check $SANDBOX/last-hook-input.json"
echo
echo "claude's reply:"
echo "$RESULT_TEXT" | head -10
exit 2
