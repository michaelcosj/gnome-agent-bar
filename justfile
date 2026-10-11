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

# Run the daemon in the foreground (until the systemd service exists)
run-daemon:
    cd daemon && {{cargo}} run --release

# Zip for extensions.gnome.org
pack:
    gnome-extensions pack --force --extra-source=format.js --extra-source=indicator.js --out-dir=target {{ext_src}}

clean:
    rm -rf target {{ext_src}}/schemas/gschemas.compiled
    cd daemon && {{cargo}} clean
