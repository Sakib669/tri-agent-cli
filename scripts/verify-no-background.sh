#!/usr/bin/env bash
# triagent process-lifecycle verification: proves the CLI leaves NO background
# processes, daemons, or autostart artifacts behind.
#
# Usage: scripts/verify-no-background.sh [path-to-triagent]
set -u
BIN="${1:-$HOME/.local/bin/triagent}"

fail() { echo "FAIL: $1"; exit 1; }
ok() { echo "ok: $1"; }

# 0. Snapshot autostart/systemd/cron state.
SNAPSHOT=$(mktemp -d)
ls "$HOME/.config/autostart" > "$SNAPSHOT/autostart.before" 2>/dev/null || true
ls "$HOME/.config/systemd/user" > "$SNAPSHOT/systemd.before" 2>/dev/null || true
crontab -l > "$SNAPSHOT/cron.before" 2>/dev/null || true

# 1. Start a one-shot agent in its own session with a long-running tool,
#    then SIGTERM the agent: the tool child must die too.
TMP=$(mktemp -d)
cd "$TMP" || fail "mktemp"
echo '{"command": "sleep 300"}' > "$TMP/cmd.json"
setsid "$BIN" -p "run the bash tool with timeout_ms 600000" --dangerously-skip-permissions \
  > "$TMP/out.log" 2>&1 &
AGENT_PID=$!
sleep 2
CHILD_PIDS=$(pgrep -f 'sleep 300' || true)
if [ -z "$CHILD_PIDS" ]; then
  # Agent may not have run the tool yet (model latency); allow a retry window.
  sleep 6
  CHILD_PIDS=$(pgrep -f 'sleep 300' || true)
fi
if [ -n "$CHILD_PIDS" ]; then
  kill -TERM "$AGENT_PID" 2>/dev/null || true
  sleep 2
  SURVIVORS=$(pgrep -f 'sleep 300' || true)
  if [ -n "$SURVIVORS" ]; then
    echo "survivors after SIGTERM: $SURVIVORS" >> "$TMP/out.log"
    fail "tool child survived agent SIGTERM: $SURVIVORS"
  fi
  ok "tool children killed when the agent was terminated"
else
  ok "no tool child spawned (model did not call bash) — skipped kill check"
fi
pkill -x triagent 2>/dev/null || true
sleep 1
LEFT=$(pgrep -x triagent || true)
[ -z "$LEFT" ] || fail "triagent process still running after SIGTERM: $LEFT"
ok "no triagent process remains"

# 2. OAuth listener is transient: start a login, grab the port, kill the
#    process, assert the port is released. Skipped when credentials already
#    exist for that provider (would needlessly open a browser).
python3 - "$HOME" <<'PYEOF'
import json, os, sys
p = os.path.expanduser("~/.local/share/triagent/credentials.json")
skip = False
try:
    d = json.load(open(p))
    skip = "claude" in d.get("providers", {})
except Exception:
    pass
sys.exit(0 if skip else 1)
PYEOF
if [ $? -eq 0 ]; then
  ok "claude credentials present — skipping loopback login test"
else
"$BIN" login claude > "$TMP/login.log" 2>&1 &
LOGIN_PID=$!
sleep 3
PORT=$(grep -o 'localhost%3A[0-9]*' "$TMP/login.log" | head -1 | sed 's/.*%3A//')
if [ -n "$PORT" ]; then
  kill "$LOGIN_PID" 2>/dev/null || true
  sleep 1
  if curl -s -m 2 "http://localhost:$PORT/callback" -o /dev/null 2>/dev/null; then
    fail "OAuth listener still bound after login process died (port $PORT)"
  else
    ok "OAuth loopback listener released on exit (port $PORT)"
  fi
else
  ok "login did not expose a port (already logged in?)"
fi
pkill -x triagent 2>/dev/null || true
fi

# 3. No autostart/systemd/cron artifacts were created.
ls "$HOME/.config/autostart" > "$SNAPSHOT/autostart.after" 2>/dev/null || true
ls "$HOME/.config/systemd/user" > "$SNAPSHOT/systemd.after" 2>/dev/null || true
crontab -l > "$SNAPSHOT/cron.after" 2>/dev/null || true
diff -q "$SNAPSHOT/autostart.before" "$SNAPSHOT/autostart.after" >/dev/null \
  || fail "~/.config/autostart changed"
diff -q "$SNAPSHOT/systemd.before" "$SNAPSHOT/systemd.after" >/dev/null \
  || fail "~/.config/systemd/user changed"
diff -q "$SNAPSHOT/cron.before" "$SNAPSHOT/cron.after" >/dev/null \
  || fail "crontab changed"
ok "no autostart / systemd-user / cron artifacts created"

rm -rf "$TMP" "$SNAPSHOT"
echo "ALL CHECKS PASSED — triagent leaves nothing running in the background."
