#!/bin/bash
# TUI Compare: runs both Codex and Astra TUI in tmux, sends messages, captures output for diff.
# Usage: bash scripts/dev/tui-compare.sh

set -e

ASTRA_BIN="${ASTRA_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/rust/target/release/astra}"
CODEX_BIN=$(which codex 2>/dev/null || echo "")
WIDTH=100
HEIGHT=30
WAIT_STARTUP=5
WAIT_RESPONSE=10

# Messages to test
MESSAGES=(
  "hi"
  "what is 2+2"
  "explain what rust is in 3 sentences"
)

capture_session() {
  local name=$1
  local bin=$2
  local env_prefix=$3
  local outdir="/tmp/tui-compare/${name}"
  mkdir -p "$outdir"

  echo "=== Starting $name ==="
  tmux kill-session -t "$name" 2>/dev/null || true
  tmux new-session -d -s "$name" -x $WIDTH -y $HEIGHT "$env_prefix $bin"
  sleep $WAIT_STARTUP

  # Capture idle state
  tmux capture-pane -t "$name" -p > "$outdir/0-idle.txt"

  for i in "${!MESSAGES[@]}"; do
    local msg="${MESSAGES[$i]}"
    local idx=$((i+1))

    # Send message
    tmux send-keys -t "$name" "$msg" Enter

    # Capture working state (after 2s)
    sleep 2
    tmux capture-pane -t "$name" -p > "$outdir/${idx}a-working.txt"

    # Wait for response
    sleep $WAIT_RESPONSE
    tmux capture-pane -t "$name" -p > "$outdir/${idx}b-response.txt"
  done

  tmux kill-session -t "$name" 2>/dev/null || true
  echo "=== $name captures in $outdir ==="
}

# Clean
rm -rf /tmp/tui-compare
mkdir -p /tmp/tui-compare

# Run Astra
ASTRA_DIR=$(dirname "$ASTRA_BIN")
capture_session "astra" "$ASTRA_BIN --tui --yes" \
  "PATH=\"${ASTRA_DIR}:\$PATH\" NO_PROXY=\"*\" ASTRA_API_URL=http://localhost:28000"

# Run Codex (if available)
if [ -n "$CODEX_BIN" ]; then
  capture_session "codex" "$CODEX_BIN" ""
else
  echo "Codex not installed, skipping. Install with: npm i -g @openai/codex"
  echo "Only Astra captures generated."
fi

# Show Astra results
echo ""
echo "============================================"
echo "  ASTRA TUI CAPTURES"
echo "============================================"
for f in /tmp/tui-compare/astra/*.txt; do
  echo ""
  echo "--- $(basename $f) ---"
  cat -n "$f" | tail -15
done

# Show diff if both exist
if [ -d "/tmp/tui-compare/codex" ]; then
  echo ""
  echo "============================================"
  echo "  SIDE-BY-SIDE DIFF (last 10 lines)"
  echo "============================================"
  for f in /tmp/tui-compare/astra/*.txt; do
    base=$(basename "$f")
    if [ -f "/tmp/tui-compare/codex/$base" ]; then
      echo ""
      echo "--- $base ---"
      diff -y --width=160 \
        <(tail -10 "/tmp/tui-compare/astra/$base") \
        <(tail -10 "/tmp/tui-compare/codex/$base") || true
    fi
  done
fi
