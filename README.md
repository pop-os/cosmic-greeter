# cosmic-greeter
libcosmic greeter for greetd, which can be run inside cosmic-comp

## Development

This project uses [just](https://github.com/casey/just) as a command runner.

### Available Commands

#### Building
- `just build-debug` - Compile with debug profile
- `just build-release` - Compile with release profile (default)
- `just build-vendored` - Compile release profile with vendored dependencies
    - Requires vendoring first, which can be done with `just vendor`

#### Testing & Development
- `just mock` - Run greeter in a windowed compositor for quick testing (builds and runs the mock server example)
- `just run` - Run with debug logs (`RUST_LOG=debug` and `RUST_BACKTRACE=full`)

#### Code Quality
- `just check` - Run clippy linter with pedantic warnings
- `just check-json` - Run clippy with JSON output format

#### Installation
- `just install` - Install all files (binary, daemon, D-Bus config, systemd files)
- `just install-debian` - Install only Debian package required files
- `just uninstall` - Remove all installed files

#### Cleanup
- `just clean` - Run `cargo clean`
- `just clean-dist` - Run `cargo clean` and remove vendored dependencies

#### Vendoring
- `just vendor` - Vendor dependencies locally and create vendor.tar
- `just vendor-extract` - Extract vendored dependencies from vendor.tar
