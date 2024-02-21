name := 'cosmic-greeter'
export APPID := 'com.system76.CosmicGreeter'

rootdir := ''
prefix := '/usr'

base-dir := absolute_path(clean(rootdir / prefix))

export INSTALL_DIR := base-dir / 'share'

bin-src := 'target' / 'release' / name
bin-dst := base-dir / 'bin' / name

# Systemd sysusers/tmpfiles components directories
lib-dir := base-dir / 'lib'

# sysusers.d
sysusers-src := 'debian' / name + '.sysusers'
sysusers-dst := lib-dir / 'sysusers.d' / name + '.conf'
# tmpfiles.d
tmpfiles-src := 'debian' / name + '.tmpfiles'
tmpfiles-dst := lib-dir / 'tmpfiles.d' / name + '.conf'

daemon-src := 'target' / 'release' / name + '-daemon'
daemon-dst := base-dir / 'bin' / name + '-daemon'

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

# Run with debug logs
run *args:
    env RUST_LOG=debug RUST_BACKTRACE=full cargo run --release {{args}}

# Install only debian package required files
# The sysusers and tmpfiles files are automatically added
install-debian:
    install -Dm0755 {{bin-src}} {{bin-dst}}
    install -Dm0755 {{daemon-src}} {{daemon-dst}}
    install -Dm0755 {{dbus-src}} {{dbus-dst}}

# Installs files
install: install-debian
    install -Dm0644 {{sysusers-src}} {{sysusers-dst}}
    install -Dm0644 {{tmpfiles-src}} {{tmpfiles-dst}}

# Uninstalls installed files
uninstall:
    rm {{bin-dst}} {{daemon-dst}} {{dbus-dst}} {{sysusers-dst}} {{tmpfiles-dst}}

# Vendor dependencies locally
vendor:
    mkdir -p .cargo
    cargo vendor --sync Cargo.toml \
        | head -n -1 > .cargo/config
    echo 'directory = "vendor"' >> .cargo/config
    tar pcf vendor.tar vendor
    rm -rf vendor

# Extracts vendored dependencies
vendor-extract:
    rm -rf vendor
    tar pxf vendor.tar
