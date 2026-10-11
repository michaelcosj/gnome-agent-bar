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

"$DAEMON" > "$OUT/daemon.log" 2>&1 &
gnome-shell --headless --wayland --no-x11 --wayland-display=wayland-agentbar-test \
    --virtual-monitor 1600x900 > "$OUT/shell.log" 2>&1 &
SHELL_PID=$!
trap 'pkill -f "$DAEMON" || true; kill $SHELL_PID 2>/dev/null || true' EXIT
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
echo "panel: $(ev "$IND._label.text")"
shot panel
ev "$IND.menu.open(false)" >/dev/null; sleep 1; shot menu-1d
ev "$IND._settings.set_string('stats-range','7d')" >/dev/null; sleep 1; shot menu-7d
ev "$IND.menu.close(false)" >/dev/null

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
