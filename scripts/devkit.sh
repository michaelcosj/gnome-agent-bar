#!/bin/bash
# Opens a nested GNOME Shell (devkit window) with the extension enabled and the
# daemon on the same private bus. Uses a throwaway data dir and in-memory
# GSettings, so the real session's extensions and settings are untouched.
# Close the devkit window to stop.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$ROOT/target/devkit}
DAEMON=$ROOT/daemon/target/debug/agent-bar-daemon
UUID=agent-bar@local

if [[ "${1:-}" != --inner ]]; then
    rm -rf "$OUT" && mkdir -p "$OUT/data/gnome-shell/extensions"
    ln -s "$ROOT/extension/$UUID" "$OUT/data/gnome-shell/extensions/"
    glib-compile-schemas "$ROOT/extension/$UUID/schemas"
    # Foreign (e.g. Nix) introspection paths crash the system gnome-shell.
    exec env -u GI_TYPELIB_PATH -u GIO_EXTRA_MODULES -u LD_LIBRARY_PATH \
        XDG_DATA_DIRS=/usr/local/share:/usr/share XDG_DATA_HOME="$OUT/data" GSETTINGS_BACKEND=memory \
        OUT="$OUT" dbus-run-session -- "$0" --inner
fi

"$DAEMON" > "$OUT/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
gnome-shell --devkit --wayland > "$OUT/shell.log" 2>&1 &
SHELL_PID=$!

for _ in $(seq 60); do
    gdbus introspect --session --dest org.gnome.Shell --object-path /org/gnome/Shell >/dev/null 2>&1 && break
    sleep 0.5
done
gnome-extensions enable $UUID
sleep 1
echo "$UUID: $(gnome-extensions info $UUID | sed -n 's/^ *State: //p')  (logs: $OUT/shell.log, $OUT/daemon.log)"
wait $SHELL_PID || true
