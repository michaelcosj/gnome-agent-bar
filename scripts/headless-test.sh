#!/bin/bash
# Smoke-tests the extension in an isolated headless GNOME Shell: private session
# bus, in-memory GSettings, throwaway XDG_DATA_HOME. Leaves the real session alone.
# Screenshots and logs go to $OUT (default: ./target/headless).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$ROOT/target/headless}
DAEMON=$ROOT/daemon/target/debug/agent-bar-daemon
UUID=agent-bar@local

if [[ "${1:-}" != --inner ]]; then
    rm -rf "$OUT" && mkdir -p "$OUT/data/gnome-shell/extensions/unsafe@test"
    ln -s "$ROOT/extension/$UUID" "$OUT/data/gnome-shell/extensions/"
    # Test-only helper: unsafe mode allows Eval and Screenshot over D-Bus.
    echo '{"uuid":"unsafe@test","name":"unsafe (test)","description":"test only","shell-version":["50"]}' \
        > "$OUT/data/gnome-shell/extensions/unsafe@test/metadata.json"
    echo "import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
export default class extends Extension { enable() { global.context.unsafe_mode = true; } disable() {} }" \
        > "$OUT/data/gnome-shell/extensions/unsafe@test/extension.js"
    glib-compile-schemas "$ROOT/extension/$UUID/schemas"
    # Foreign (e.g. Nix) introspection paths crash the system gnome-shell.
    exec env -u GI_TYPELIB_PATH -u GIO_EXTRA_MODULES -u LD_LIBRARY_PATH \
        XDG_DATA_DIRS=/usr/local/share:/usr/share XDG_DATA_HOME="$OUT/data" GSETTINGS_BACKEND=memory \
        OUT="$OUT" timeout 120 dbus-run-session -- "$0" --inner
fi

# Offline daemon: fake Claude/Codex/pi/opencode data + a local server for both usage
# endpoints, so tests never hit (or get rate limited by) the real ones and the data
# is deterministic. The daemon also gets its own HOME so it never sees real logs.
FAKE=$OUT/fake-claude
python3 -I - "$FAKE" "$OUT" <<'PY'
import base64, json, os, sqlite3, sys, time, random
from datetime import datetime, timedelta, timezone
root, out = sys.argv[1], sys.argv[2]; now = datetime.now(timezone.utc); iso = lambda d: d.isoformat().replace("+00:00", "Z")
os.makedirs(f"{root}/projects/demo", exist_ok=True)
json.dump({"claudeAiOauth": {"accessToken": "test-token", "expiresAt": int(time.time() * 1000) + 86_400_000,
           "subscriptionType": "pro"}}, open(f"{root}/.credentials.json", "w"))
random.seed(7)
with open(f"{root}/projects/demo/session.jsonl", "w") as f:
    for i in range(160):
        ts = now - timedelta(minutes=random.randint(0, 23 * 60) + random.random())
        u = {"input_tokens": 5, "output_tokens": random.randint(50, 900), "cache_read_input_tokens": random.randint(20000, 90000),
             "cache_creation_input_tokens": 2000, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 2000}}
        f.write(json.dumps({"type": "assistant", "timestamp": iso(ts), "sessionId": f"s{i % 4}", "requestId": f"r{i}",
                            "message": {"id": f"m{i}", "model": "claude-opus-5-5", "usage": u}}) + "\n")
lim = lambda kind, group, pct, h: {"kind": kind, "group": group, "percent": pct, "severity": "normal",
                                   "resets_at": iso(now + timedelta(hours=h)), "is_active": True}
os.makedirs(f"{root}/srv", exist_ok=True)
json.dump({"limits": [lim("session", "session", 22, 3.7), lim("weekly_all", "weekly", 26, 143)],
           "seven_day_breakdown": {"rows": [{"display_name": "Claude Code", "percent": 82},
                                            {"display_name": "Cowork", "percent": 16}, {"display_name": "Chats", "percent": 2}]}},
          open(f"{root}/srv/usage", "w"))
json.dump({"plan_type": "plus", "rate_limit": {"limit_reached": False,
           "primary_window": {"used_percent": 64, "limit_window_seconds": 18000, "reset_at": int(time.time()) + 5400},
           "secondary_window": {"used_percent": 31, "limit_window_seconds": 604800, "reset_at": int(time.time()) + 400000}}},
          open(f"{root}/srv/wham", "w"))

# Codex (signed in with ChatGPT): one rollout with two turns.
codex = f"{out}/home/.codex"; os.makedirs(f"{codex}/sessions/2026/10/11", exist_ok=True)
jwt = "e30." + base64.urlsafe_b64encode(json.dumps({"exp": int(time.time()) + 86400}).encode()).decode().rstrip("=") + ".sig"
json.dump({"auth_mode": "chatgpt", "tokens": {"access_token": jwt, "account_id": "acct-test"}}, open(f"{codex}/auth.json", "w"))
with open(f"{codex}/sessions/2026/10/11/rollout-test.jsonl", "w") as f:
    f.write(json.dumps({"timestamp": iso(now - timedelta(hours=2)), "type": "turn_context", "payload": {"model": "gpt-6.1-sol"}}) + "\n")
    for k, (inp, cached, outp) in enumerate([(40000, 30000, 900), (95000, 80000, 2100)]):
        f.write(json.dumps({"timestamp": iso(now - timedelta(hours=2 - k)), "type": "event_msg", "payload": {"type": "token_count",
                "info": {"total_token_usage": {"input_tokens": inp, "cached_input_tokens": cached, "output_tokens": outp,
                                               "total_tokens": inp + outp}}}}) + "\n")

# pi (OAuth to openai): one turn.
pi = f"{out}/home/.pi/agent"; os.makedirs(f"{pi}/sessions/--work--", exist_ok=True)
json.dump({"openai": {"type": "oauth"}}, open(f"{pi}/auth.json", "w"))
with open(f"{pi}/sessions/--work--/s.jsonl", "w") as f:
    f.write(json.dumps({"type": "message", "id": "p1", "timestamp": iso(now - timedelta(hours=5)), "message": {"role": "assistant",
            "provider": "openai", "model": "gpt-6.1-sol", "usage": {"input": 3000, "output": 400, "cacheRead": 20000, "cacheWrite": 0}}}) + "\n")

# opencode: subscription turns (cost 0), including a -fast model.
os.makedirs(f"{out}/data/opencode", exist_ok=True)
db = sqlite3.connect(f"{out}/data/opencode/opencode.db"); db.execute("PRAGMA journal_mode=WAL")
db.execute("CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, time_created INTEGER, time_updated INTEGER, data TEXT)")
for i in range(30):
    t = int((now - timedelta(minutes=random.randint(0, 20 * 60))).timestamp() * 1000)
    data = {"time": {"created": t}, "model": {"id": "gpt-6.1-sol-fast" if i % 5 == 0 else "gpt-6.1-sol", "providerID": "openai"},
            "cost": 0, "tokens": {"input": random.randint(500, 4000), "output": random.randint(100, 900), "reasoning": 50,
                                  "cache": {"read": random.randint(10000, 60000), "write": 0}}}
    db.execute("INSERT INTO session_message VALUES (?,?,?,?,?,?)", (f"msg{i}", f"ses{i % 3}", "assistant", t, t, json.dumps(data)))
db.commit(); db.close()
PY
PORT=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -I -m http.server "$PORT" --bind 127.0.0.1 --directory "$FAKE/srv" > "$OUT/usage-server.log" 2>&1 &
USAGE_SERVER=$!
for _ in $(seq 40); do
    python3 -I -c "import socket; socket.create_connection(('127.0.0.1', $PORT), 0.2)" 2>/dev/null && break
    sleep 0.1
done
export CLAUDE_CONFIG_DIR=$FAKE AGENT_BAR_USAGE_URL=http://127.0.0.1:$PORT/usage \
    AGENT_BAR_CHATGPT_USAGE_URL=http://127.0.0.1:$PORT/wham
daemon() { HOME=$OUT/home CODEX_HOME=$OUT/home/.codex PI_CODING_AGENT_DIR=$OUT/home/.pi/agent "$DAEMON"; }

daemon > "$OUT/daemon.log" 2>&1 &
gnome-shell --headless --wayland --no-x11 --wayland-display=wayland-agentbar-test \
    --virtual-monitor 1600x900 > "$OUT/shell.log" 2>&1 &
SHELL_PID=$!
trap 'pkill -f "$DAEMON" || true; kill $SHELL_PID $USAGE_SERVER 2>/dev/null || true' EXIT
for _ in $(seq 60); do
    gdbus introspect --session --dest org.gnome.Shell --object-path /org/gnome/Shell >/dev/null 2>&1 && break
    sleep 0.5
done

ev() { gdbus call --session --dest org.gnome.Shell --object-path /org/gnome/Shell --method org.gnome.Shell.Eval "$1" | cut -c1-120; }
shot() { gdbus call --session --dest org.gnome.Shell.Screenshot --object-path /org/gnome/Shell/Screenshot \
    --method org.gnome.Shell.Screenshot.Screenshot false false "$OUT/$1.png" >/dev/null && echo "screenshot: $OUT/$1.png"; }
IND="Main.panel.statusArea['$UUID']"
# Every label text in the open menu, joined.
MENU_TEXT="(function walk(a) { return [a.text ?? '', ...a.get_children().map(walk)].join(' '); })($IND.menu.box)"

gnome-extensions enable unsafe@test
gnome-extensions enable $UUID
sleep 3
gnome-extensions info $UUID | grep State
# Fresh-session clutter: first-login welcome dialog, overview, unsafe-mode banner.
ev "Main.layoutManager.modalDialogGroup.get_children().forEach(d => d.close?.()); Main.overview.hide()" >/dev/null
ev "Main.messageTray.getSources().forEach(s => s.destroy())" >/dev/null
for _ in $(seq 30); do  # wait for the daemon's first snapshot
    [[ "$(ev "$IND.panelSummary")" == *openai*%* ]] && break
    sleep 0.5
done
PANEL=$(ev "$IND.panelSummary"); echo "panel: $PANEL"
# Both accounts; the countdown only on ChatGPT (36% left vs Claude's 74%).
[[ "$PANEL" == *claude*%\ \|* && "$PANEL" == *openai*%\ ·\ * ]] || { echo "FAIL: panel should show both accounts, countdown on ChatGPT"; exit 1; }
shot panel
ev "$IND.menu.open(false)" >/dev/null; sleep 1; shot menu-1d
# Chart hover: stage x of the 22nd bar maps to slot 21, and hovering it captions that hour.
FIND_CHART="(function find(a) { if (a.constructor.name.includes('HourlyChart')) return a; for (const c of a.get_children()) { const r = find(c); if (r) return r; } return null; })($IND.menu.box)"
CAPTION="ch.get_parent().get_children()[1].get_children().map(l => l.text).join(' | ')"
echo "chart slot for bar 22: $(ev "const ch = $FIND_CHART; const [x] = ch.get_transformed_position(); ch._slotAt(x + ch.width * 21.5 / 24)")"
echo "chart hover caption: $(ev "const ch = $FIND_CHART; ch._hover = 21; ch._onHover(21); ch.queue_repaint(); $CAPTION")"
sleep 0.5; shot chart-hover
echo "chart caption after leave: $(ev "const ch = $FIND_CHART; ch._hover = -1; ch._onHover(-1); ch.queue_repaint(); $CAPTION")"
ev "$IND._settings.set_string('stats-range','7d')" >/dev/null; sleep 1; shot menu-7d
# ChatGPT tab: usage from Codex, pi and opencode, so a By tool section.
ev "$IND._settings.set_string('menu-account','openai')" >/dev/null; sleep 1; shot menu-chatgpt
MENU=$(gdbus call --session --dest org.gnome.Shell --object-path /org/gnome/Shell --method org.gnome.Shell.Eval "$MENU_TEXT")
for want in 'ChatGPT · Plus' 'By tool' opencode Codex pi '6.1-sol-fast' 'Claude  74%' 'ChatGPT  36%'; do
    [[ "$MENU" == *"$want"* ]] || { echo "FAIL: ChatGPT tab lacks '$want'"; exit 1; }
done
echo "chatgpt tab: ok"
ev "$IND._settings.set_strv('hidden-accounts',['claude'])" >/dev/null; sleep 1
echo "panel (claude hidden): $(ev "$IND.panelSummary")"
ev "$IND._settings.set_strv('hidden-accounts',[])" >/dev/null
ev "$IND._settings.set_string('menu-account','claude')" >/dev/null
ev "$IND.menu.close(false)" >/dev/null

# Low-quota snapshot -> one notification from the "Agent Bar" source.
EXT="Main.extensionManager.lookup('$UUID').stateObj"
LOW='{"version":1,"providers":[{"id":"claude","name":"Claude","status":"ok","limit_reached":true,"limits":[{"id":"session","label":"5-hour","used_pct":96,"resets_at":"2099-01-01T00:00:00Z"}],"stats":{}}]}'
ev "$EXT._onSnapshot('$LOW')" >/dev/null
ev "$EXT._onSnapshot('$LOW')" >/dev/null
echo "notifications: $(ev "JSON.stringify(Main.messageTray.getSources().filter(s => s.title === 'Agent Bar').flatMap(s => s.notifications.map(n => n.title)))")"
echo "panel (low): $(ev "$IND.panelSummary")"
ev "$IND.menu.open(false)" >/dev/null; sleep 1
[[ "$(ev "$MENU_TEXT")" == *"limit reached"* ]] || { echo "FAIL: no limit reached badge"; exit 1; }
shot low
ev "$IND.menu.close(false)" >/dev/null

# A long error must wrap, not widen the menu.
ev "$IND.menu.open(false)" >/dev/null; sleep 1
W_OK=$(ev "Math.round($IND.menu.actor.width)" | grep -oE '[0-9]+' | tail -1)
LONG_ERR=$(printf 'request failed: error sending request for url: dns error: failed to lookup address information: Temporary failure in name resolution %.0s' 1 2)
STALE="{\"version\":1,\"providers\":[{\"id\":\"claude\",\"name\":\"Claude\",\"status\":\"stale\",\"error\":\"$LONG_ERR\",\"limits\":[{\"id\":\"session\",\"label\":\"5-hour\",\"used_pct\":40,\"resets_at\":null}],\"stats\":{},\"hourly\":[]}]}"
ev "$EXT._onSnapshot('$STALE')" >/dev/null; sleep 1
W_ERR=$(ev "Math.round($IND.menu.actor.width)" | grep -oE '[0-9]+' | tail -1)
echo "menu width: normal=$W_OK long-error=$W_ERR"
shot long-error
ev "$IND.menu.close(false)" >/dev/null
[ "$W_ERR" -le $((W_OK + 2)) ] || { echo "FAIL: long error widened the menu"; exit 1; }

gnome-extensions prefs $UUID; sleep 3
echo "prefs window: $(ev "global.get_window_actors().map(a => a.meta_window.get_title()).filter(t => t?.includes('Agent Bar')).length > 0")"
shot prefs

pkill -f "$DAEMON"; sleep 1
echo "panel (daemon down): $(ev "$IND.panelSummary")"
ev "$IND.menu.open(false)" >/dev/null; sleep 1; shot daemon-down
ev "$IND.menu.close(false)" >/dev/null

daemon >> "$OUT/daemon.log" 2>&1 &
sleep 3
echo "panel (daemon back): $(ev "$IND.panelSummary")"
gnome-extensions disable $UUID; sleep 1; gnome-extensions enable $UUID; sleep 2
gnome-extensions info $UUID | grep State
# Shutdown with the daemon gone exercises late D-Bus callbacks on a destroyed indicator.
pkill -f "$DAEMON"; sleep 1
kill $SHELL_PID; wait $SHELL_PID 2>/dev/null || true

if grep -E 'JS ERROR|JS WARNING' "$OUT/shell.log" | grep -v unsafe@test; then
    echo "FAIL: JS errors in $OUT/shell.log"; exit 1
fi
echo "PASS"
