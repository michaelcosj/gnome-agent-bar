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

# Offline daemon: fake Claude dir + local usage server, so tests never hit (or get
# rate limited by) the real endpoint and the data is deterministic.
FAKE=$OUT/fake-claude
python3 -I - "$FAKE" <<'PY'
import json, os, sys, time, random
from datetime import datetime, timedelta, timezone
root = sys.argv[1]; now = datetime.now(timezone.utc); iso = lambda d: d.isoformat().replace("+00:00", "Z")
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
PY
PORT=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -I -m http.server "$PORT" --bind 127.0.0.1 --directory "$FAKE/srv" > "$OUT/usage-server.log" 2>&1 &
USAGE_SERVER=$!
for _ in $(seq 40); do
    python3 -I -c "import socket; socket.create_connection(('127.0.0.1', $PORT), 0.2)" 2>/dev/null && break
    sleep 0.1
done
export CLAUDE_CONFIG_DIR=$FAKE AGENT_BAR_USAGE_URL=http://127.0.0.1:$PORT/usage

"$DAEMON" > "$OUT/daemon.log" 2>&1 &
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

gnome-extensions enable unsafe@test
gnome-extensions enable $UUID
sleep 3
gnome-extensions info $UUID | grep State
# Fresh-session clutter: first-login welcome dialog, overview, unsafe-mode banner.
ev "Main.layoutManager.modalDialogGroup.get_children().forEach(d => d.close?.()); Main.overview.hide()" >/dev/null
ev "Main.messageTray.getSources().forEach(s => s.destroy())" >/dev/null
for _ in $(seq 30); do  # wait for the daemon's first snapshot
    [[ "$(ev "$IND._label.text")" == *%* ]] && break
    sleep 0.5
done
echo "panel: $(ev "$IND._label.text")"
echo "glyph: $(ev "[$IND._logo.visible ? 'logo' : 'ring', $IND._logo.get_style_class_name()].join(' ')")"
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
ev "$IND.menu.close(false)" >/dev/null

# Low-quota snapshot -> one notification from the "Agent Bar" source.
EXT="Main.extensionManager.lookup('$UUID').stateObj"
LOW='{"version":1,"providers":[{"id":"claude","name":"Claude","status":"ok","limits":[{"id":"session","label":"5-hour","used_pct":96,"resets_at":"2099-01-01T00:00:00Z"}],"stats":{}}]}'
ev "$EXT._onSnapshot('$LOW')" >/dev/null
ev "$EXT._onSnapshot('$LOW')" >/dev/null
echo "notifications: $(ev "JSON.stringify(Main.messageTray.getSources().filter(s => s.title === 'Agent Bar').flatMap(s => s.notifications.map(n => n.title)))")"
echo "panel (low): $(ev "$IND._label.text") / $(ev "$IND._logo.visible ? $IND._logo.get_style_class_name() : $IND._ring.get_style_class_name()")"
shot low

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
echo "panel (daemon down): $(ev "$IND._label.text")"
ev "$IND.menu.open(false)" >/dev/null; sleep 1; shot daemon-down
ev "$IND.menu.close(false)" >/dev/null

"$DAEMON" >> "$OUT/daemon.log" 2>&1 &
sleep 3
echo "panel (daemon back): $(ev "$IND._label.text")"
gnome-extensions disable $UUID; sleep 1; gnome-extensions enable $UUID; sleep 2
gnome-extensions info $UUID | grep State
# Shutdown with the daemon gone exercises late D-Bus callbacks on a destroyed indicator.
pkill -f "$DAEMON"; sleep 1
kill $SHELL_PID; wait $SHELL_PID 2>/dev/null || true

if grep -E 'JS ERROR|JS WARNING' "$OUT/shell.log" | grep -v unsafe@test; then
    echo "FAIL: JS errors in $OUT/shell.log"; exit 1
fi
echo "PASS"
