# Wombat development tasks. Run `just` or `just --list` to see all of them.

# List available recipes.
default:
    @just --list

# Build the debug binary.
build:
    cargo build

# Run every test target.
test:
    cargo test --all-targets

# Format the whole tree.
fmt:
    cargo fmt --all

# Fail if anything is unformatted, without changing files.
fmt-check:
    cargo fmt --all -- --check

# Lint with warnings denied, matching CI.
clippy:
    cargo clippy --locked --all-targets --all-features -- -D warnings

# Build rustdoc with warnings denied, so broken doc links fail the build.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Check dependency licenses, bans, and advisories.
deny:
    cargo deny check

# Lint the POSIX installer.
shellcheck:
    shellcheck install.sh

# Refresh the CLI/Lua surface drift snapshot after an intentional change.
bless-surface:
    WOMBAT_BLESS_SURFACE=1 cargo test --bins documented_surface

# The full local set CONTRIBUTING.md asks for before submitting a change.
check: fmt-check clippy test doc

# check, plus the dependency and shell gates touched-dependency changes need.
check-all: check deny shellcheck

# Install this checkout's wombat over whatever is currently on PATH.
install:
    cargo install --path . --force

# Remove the installed wombat binary.
uninstall:
    cargo uninstall wombat

# Build and run wombat, forwarding arguments after `--`.
run *args:
    cargo run -- {{ args }}

# Remove build artifacts.
clean:
    cargo clean
