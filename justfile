name := 'cosmic-greeter'
export APPID := 'com.system76.CosmicGreeter'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))

export INSTALL_DIR := base-dir / 'share'

cargo-target-dir := env('CARGO_TARGET_DIR', 'target')
bin-src := cargo-target-dir / 'release' / name
bin-dst := base-dir / 'bin' / name

# Systemd sysusers/tmpfiles components directories
lib-dir := base-dir / 'lib'

# sysusers.d
sysusers-src := 'debian' / name + '.sysusers'
sysusers-dst := lib-dir / 'sysusers.d' / name + '.conf'
# tmpfiles.d
tmpfiles-src := 'debian' / name + '.tmpfiles'
tmpfiles-dst := lib-dir / 'tmpfiles.d' / name + '.conf'

daemon-src := cargo-target-dir / 'release' / name + '-daemon'
daemon-dst := base-dir / 'bin' / name + '-daemon'

start-src := name + '-start.sh'
start-dst := base-dir / 'bin' / name + '-start'

dbus-src := 'dbus' / APPID + '.conf'
dbus-dst := base-dir / 'share' / 'dbus-1' / 'system.d' / APPID + '.conf'

# Default recipe which runs `just build-release`
default: build-release

# Runs `cargo clean`
clean:
    cargo clean

# `cargo clean` and removes vendored dependencies
clean-dist: clean
    rm -rf .cargo vendor vendor.tar

# Compiles with debug profile
build-debug *args:
    cargo build --all {{args}}

# Compiles with release profile
build-release *args: (build-debug '--release' args)

# Compiles release profile with vendored dependencies
build-vendored *args: vendor-extract (build-release '--frozen --offline' args)

# Runs a clippy check
check *args:
    cargo clippy --all-features {{args}} -- -W clippy::pedantic

# Runs a clippy check with JSON message format
check-json: (check '--message-format=json')

mock:
    cargo build --release --example server
    cosmic-comp {{cargo-target-dir}}/release/examples/server

# Run with debug logs
run *args:
    env RUST_LOG=debug RUST_BACKTRACE=full cargo run --release {{args}}

# Install only debian package required files
# The sysusers and tmpfiles files are automatically added
install-debian:
    install -Dm0755 {{bin-src}} {{bin-dst}}
    install -Dm0755 {{start-src}} {{start-dst}}
    install -Dm0755 {{daemon-src}} {{daemon-dst}}
    install -Dm0755 {{dbus-src}} {{dbus-dst}}

# Installs files
install: install-debian
    install -Dm0644 {{sysusers-src}} {{sysusers-dst}}
    install -Dm0644 {{tmpfiles-src}} {{tmpfiles-dst}}

# Uninstalls installed files
uninstall:
    rm {{start-dst}} {{bin-dst}} {{daemon-dst}} {{dbus-dst}} {{sysusers-dst}} {{tmpfiles-dst}}

# Vendor dependencies locally
vendor:
    #!/usr/bin/env sh
    mkdir -p .cargo
    cargo vendor --sync Cargo.toml | head -n -1 > .cargo/config.toml
    echo 'directory = "vendor"' >> .cargo/config.toml
    echo >> .cargo/config.toml
    echo '[env]' >> .cargo/config.toml
    if [ -n "${SOURCE_DATE_EPOCH}" ]
    then
        source_date="$(date -d "@${SOURCE_DATE_EPOCH}" "+%Y-%m-%d")"
        echo "VERGEN_GIT_COMMIT_DATE = \"${source_date}\"" >> .cargo/config.toml
    fi
    if [ -n "${SOURCE_GIT_HASH}" ]
    then
        echo "VERGEN_GIT_SHA = \"${SOURCE_GIT_HASH}\"" >> .cargo/config.toml
    fi
    tar pcf vendor.tar .cargo vendor
    rm -rf .cargo vendor

# Extracts vendored dependencies
vendor-extract:
    rm -rf vendor
    tar pxf vendor.tar
