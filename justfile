uuid := "agent-bar@local"
ext_src := "extension/" + uuid
ext_dst := env_var("HOME") / ".local/share/gnome-shell/extensions" / uuid
cargo := env_var_or_default("CARGO", env_var("HOME") / ".cargo/bin/cargo")

default:
    @just --list

# Release build of the daemon
build:
    cd daemon && {{cargo}} build --release

# Rust tests + headless GNOME Shell smoke test
test:
    cd daemon && {{cargo}} test
    gjs -m extension/tests/unit.js
    cd daemon && {{cargo}} build
    scripts/headless-test.sh

schemas:
    glib-compile-schemas {{ext_src}}/schemas

# Dev install: symlink the extension so edits apply on the next Shell start
link-extension: schemas
    mkdir -p "$(dirname {{ext_dst}})"
    ln -sfn "{{justfile_directory()}}/{{ext_src}}" "{{ext_dst}}"

# Nested GNOME Shell window, extension enabled, daemon on the same private bus
devkit:
    cd daemon && {{cargo}} build
    scripts/devkit.sh

# Run the daemon in the foreground (dev; the installed service runs at login)
run-daemon:
    cd daemon && {{cargo}} run --release

# Zip for extensions.gnome.org
pack:
    gnome-extensions pack --force --extra-source=format.js --extra-source=indicator.js --extra-source=alerts.js --extra-source=notifier.js --extra-source=icons --out-dir=target {{ext_src}}

clean:
    rm -rf target {{ext_src}}/schemas/gschemas.compiled
    cd daemon && {{cargo}} clean

unit_dir := env_var("HOME") / ".config/systemd/user"
bin_dir := env_var("HOME") / ".local/bin"

# Install daemon (systemd user service, starts at login) and extension
install: pack
    @if pgrep -f 'target/(debug|release)/agent-bar-daemon' >/dev/null; then \
        echo "Stop the dev daemon (just run-daemon / devkit) first: it holds the bus name."; exit 1; fi
    {{cargo}} install --quiet --locked --path daemon --root "$HOME/.local"
    install -Dm644 packaging/agent-bar.service "{{unit_dir}}/agent-bar.service"
    @# Example config only if none exists, so edits survive reinstalls.
    @[ -e "$HOME/.config/agent-bar/config.toml" ] || install -Dm644 packaging/config.example.toml "$HOME/.config/agent-bar/config.toml"
    systemctl --user daemon-reload
    systemctl --user enable agent-bar.service
    systemctl --user restart agent-bar.service
    @# Replace a dev symlink without following it into the source tree.
    @if [ -L "{{ext_dst}}" ]; then rm "{{ext_dst}}"; fi
    gnome-extensions install --force target/{{uuid}}.shell-extension.zip
    @gnome-extensions enable {{uuid}} 2>/dev/null && echo "Extension enabled." \
        || echo "Extension installed. Log out and back in once, then: gnome-extensions enable {{uuid}}"

# Remove the service, binary and extension
uninstall:
    -systemctl --user disable --now agent-bar.service
    rm -f "{{unit_dir}}/agent-bar.service" "{{bin_dir}}/agent-bar-daemon"
    systemctl --user daemon-reload
    -gnome-extensions uninstall {{uuid}}

# Service status and recent logs
status:
    -systemctl --user --no-pager status agent-bar.service
    -gnome-extensions info {{uuid}}

logs:
    journalctl --user -u agent-bar.service -f
