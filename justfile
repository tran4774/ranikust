# Development tasks for ranikust. Run `just` to list the recipes.
#
# The recipes that mirror CI run exactly the commands of the matching job in
# `.github/workflows/ci.yml`. Change both together.

set shell := ["bash", "-euo", "pipefail", "-c"]

musl_target := "x86_64-unknown-linux-musl"
static_bin := "target/" + musl_target + "/release/ranikust"

# List the recipes.
default:
    @just --list --unsorted

# Format check, clippy and unprivileged tests: run before calling a change done.
check: fmt-check lint test

# Format every crate in place.
fmt:
    cargo fmt --all

# Fail if any file is not formatted (CI job `fmt`).
fmt-check:
    cargo fmt --all --check

# Clippy on all targets, warnings are errors (CI job `clippy`).
lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Tier T0 tests (CI job `test`). Extra arguments go to `cargo test`, e.g. a name filter.
test *args:
    cargo test --workspace --locked {{ args }}

# Static x86_64 musl release binary, checked to need no loader or shared libraries (CI job `static-build`).
build-static:
    #!/usr/bin/env bash
    set -euo pipefail
    BIN={{ static_bin }}
    cargo build --release --locked --target {{ musl_target }} -p ranikust
    file "$BIN"
    if readelf --program-headers --wide "$BIN" | grep -q 'program interpreter'; then
      echo "error: $BIN requests a dynamic loader" >&2
      exit 1
    fi
    if readelf --dynamic --wide "$BIN" | grep -q '(NEEDED)'; then
      echo "error: $BIN depends on shared libraries" >&2
      exit 1
    fi
    ls -l "$BIN"
    "$BIN" version

# Type-check for aarch64 musl so no architecture-specific code slips in (CI job `check-aarch64`).
check-aarch64:
    cargo check --workspace --locked --target aarch64-unknown-linux-musl

# Advisories, licenses, bans and sources with cargo-deny (CI job `deny`).
deny:
    cargo deny --locked check

# API docs with rustdoc, warnings are errors (CI job `docs`).
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Build the user guide into target/book (CI job `docs`). Requires mdbook.
book:
    mdbook build docs/book

# The draft goes to stdout; edit it into CHANGELOG.md by hand.
# Draft release notes for unreleased commits; requires git-cliff (`cargo install --locked git-cliff`).
changelog *args:
    git cliff --unreleased {{ args }}

# Everything CI runs, in CI's order.
ci: fmt-check lint test build-static check-aarch64 deny doc book
