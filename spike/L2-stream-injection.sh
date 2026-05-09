#!/usr/bin/env bash
# L2 — verify that a long-lived `claude` subprocess accepts multiple
# user prompts on stdin via --input-format=stream-json, keeps a single
# session_id across them, and reuses prompt cache between turns.
#
# v3 fixes:
#   1. Use `cat $INPUT |` instead of `< $INPUT`. Bash file-redirect closes
#      stdin too quickly; claude's stream-json reader sees EOF before
#      processing messages. Pipe keeps stdin open during run.
#   2. Tolerate turn merging. If we send N messages back-to-back without
#      reading the result event between them, claude may process them as
#      M ≤ N turns. The wrapper will pace writes properly; for spike, we
#      just want to confirm same session + cache reuse, not strict 1:1.
#
# Pass criteria (relaxed to match real semantics):
#   - ≥2 result events (something completed)
#   - 1 unique session_id across all events
#   - cache_read_input_tokens grows from turn 1 to turn 2 (cache reuse)
#
# Cost: ~$0.10–0.30

set -euo pipefail

SANDBOX="$(mktemp -d -t coderoom-L2-XXXXXX)"
trap 'echo "sandbox: $SANDBOX"' EXIT

INPUT="$SANDBOX/input.jsonl"
OUTPUT="$SANDBOX/output.jsonl"

cat > "$INPUT" <<'EOF'
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly: ALPHA"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly: BETA"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Reply with exactly: GAMMA"}]}}
EOF

echo "===== L2: long-lived stream-json session ====="
echo "sandbox: $SANDBOX"
echo

# IMPORTANT: pipe via cat, not < redirect. < closes stdin before claude
# has finished reading messages.
cat "$INPUT" | claude \
  --print \
  --input-format=stream-json \
  --output-format=stream-json \
  --verbose \
  --max-budget-usd 0.50 \
  --add-dir "$SANDBOX" \
  > "$OUTPUT" || {
    echo "❌ FAIL: claude exited non-zero. stderr-on-stdout (last 30 lines):"
    tail -30 "$OUTPUT"
    exit 1
  }

echo "===== output sample ====="
echo "(first 2 lines)"
head -2 "$OUTPUT" | cut -c1-200
echo "(last 2 lines)"
tail -2 "$OUTPUT" | cut -c1-200
echo

echo "===== verdict ====="
SESSIONS=$(jq -r 'select(.type=="result") | .session_id' "$OUTPUT" 2>/dev/null | sort -u)
SESSION_COUNT=$(echo -n "$SESSIONS" | grep -c . || true)
TURN_COUNT=$(jq -rc 'select(.type=="result")' "$OUTPUT" 2>/dev/null | wc -l)

echo "result events:    $TURN_COUNT"
echo "unique sessions:  $SESSION_COUNT"
echo "session id(s):"
echo "$SESSIONS" | sed 's/^/  /'
echo

echo "per-turn replies + cache stats:"
jq -r 'select(.type=="result") |
  "  reply=\((.result | tostring | gsub("\n";"\\n"))[0:40])  cache_read=\(.usage.cache_read_input_tokens)  cache_create=\(.usage.cache_creation_input_tokens)  cost=\(.total_cost_usd)"' \
  "$OUTPUT" 2>/dev/null
echo

CACHE_READS=$(jq -rs '[.[] | select(.type=="result") | .usage.cache_read_input_tokens]' "$OUTPUT" 2>/dev/null)
echo "cache_read sequence: $CACHE_READS"
echo

if [[ "$SESSION_COUNT" -eq 1 && "$TURN_COUNT" -ge 2 ]]; then
  GREW=$(echo "$CACHE_READS" | jq 'if length >= 2 then (.[1] > .[0]) else false end')
  if [[ "$GREW" == "true" ]]; then
    echo "✅ PASS: same session, ≥2 turns, cache_read grew turn1 → turn2."
    echo "   (Long-lived session + brief routing pattern is feasible.)"
    exit 0
  else
    echo "⚠️  PARTIAL: same session and ≥2 turns, but cache_read did not grow."
    echo "   Cost asymmetry concern — long sessions may not save tokens."
    exit 2
  fi
fi

if [[ "$SESSION_COUNT" -eq 0 || "$TURN_COUNT" -eq 0 ]]; then
  echo "❌ FAIL: no result events in $TURN_COUNT turns / $SESSION_COUNT sessions."
  echo "First 10 lines of output:"
  head -10 "$OUTPUT"
  echo
  echo "type histogram:"
  jq -r '.type' "$OUTPUT" 2>/dev/null | sort | uniq -c
  exit 1
fi

echo "⚠️  PARTIAL: $SESSION_COUNT session(s), $TURN_COUNT turns — investigate."
exit 2
