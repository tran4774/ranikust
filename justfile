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

# The musl recipes run through cargo-zigbuild (ADR-0020), which compiles every
# C dependency (libzstd, mimalloc, ring...) with `zig cc` for the target, so
# no musl cross compiler is needed. rustc still links x86_64 with the host
# `cc`: zig's linker would drop static-pie and the symbol table.

# Static x86_64 musl release binary, checked to need no loader or shared libraries (CI job `static-build`). Requires zig and cargo-zigbuild.
build-static:
    #!/usr/bin/env bash
    set -euo pipefail
    BIN={{ static_bin }}
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo zigbuild --release --locked --target {{ musl_target }} -p ranikust
    file "$BIN"
    if readelf --program-headers --wide "$BIN" | grep -q 'program interpreter'; then
      echo "error: $BIN requests a dynamic loader" >&2
      exit 1
    fi
    if readelf --dynamic --wide "$BIN" | grep -q '(NEEDED)'; then
      echo "error: $BIN depends on shared libraries" >&2
      exit 1
    fi
    if ! readelf --file-header --wide "$BIN" | grep -q 'Type: *DYN'; then
      echo "error: $BIN is not position-independent (static-pie)" >&2
      exit 1
    fi
    if ! readelf --section-headers --wide "$BIN" | grep -q '\.symtab'; then
      echo "error: $BIN has no symbol table" >&2
      exit 1
    fi
    ls -l "$BIN"
    "$BIN" version

# Type-check for aarch64 musl so no architecture-specific code slips in (CI job `check-aarch64`). Requires zig and cargo-zigbuild.
check-aarch64:
    cargo-zigbuild check --workspace --locked --target aarch64-unknown-linux-musl

# aarch64 runs under qemu-user (ADR-0009): the static release binary, then the
# T0 tests, built with cargo-zigbuild and run through cargo's target runner.
# QEMU 9.1 or later: older qemu-user has no openat2, which RootPath needs.
# Skipped there, with the reason:
# - `open_gives_cloexec_path_fd_and_mount_id`: qemu-user's statx leaves
#   `stx_mnt_id` at 0 (QEMU 10.0 and 10.2), so the mount id it asserts is not seen;
# - without a binfmt_misc entry for aarch64, ranikust's `cli` and `metrics`
#   tests, which spawn the binary themselves (CI registers one and requires it).

# Static aarch64 musl binary and T0 tests under qemu-user (CI job `test-aarch64`). QEMU_AARCH64 names the emulator. Requires zig, cargo-zigbuild, qemu-user 9.1 or later.
test-aarch64 *args:
    #!/usr/bin/env bash
    set -euo pipefail
    target=aarch64-unknown-linux-musl
    qemu="${QEMU_AARCH64:-$(command -v qemu-aarch64-static || command -v qemu-aarch64 || true)}"
    if [ ! -x "$qemu" ]; then
      echo "error: no qemu-aarch64: install qemu-user 9.1 or later, or set QEMU_AARCH64" >&2
      exit 1
    fi
    version="$("$qemu" --version | sed -nE '1s/.*version ([0-9]+\.[0-9]+).*/\1/p')"
    if [ -z "$version" ] || [ "$(printf '%s\n' 9.1 "$version" | sort -V | head -1)" != 9.1 ]; then
      echo "error: $qemu is QEMU ${version:-of unknown version}; 9.1 or later is needed for openat2" >&2
      exit 1
    fi
    echo "qemu: $qemu ($version)"
    if [ -e /proc/sys/fs/binfmt_misc/qemu-aarch64 ]; then
      echo "binfmt: $(grep '^interpreter' /proc/sys/fs/binfmt_misc/qemu-aarch64)"
    fi
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER="$qemu"
    BIN=target/$target/release/ranikust
    cargo-zigbuild build --release --locked --target $target -p ranikust
    file "$BIN"
    if readelf --program-headers --wide "$BIN" | grep -q 'program interpreter' ||
       readelf --dynamic --wide "$BIN" | grep -q '(NEEDED)'; then
      echo "error: $BIN is not static" >&2
      exit 1
    fi
    "$qemu" "$BIN" version
    skip=(--skip root::tests::open_gives_cloexec_path_fd_and_mount_id)
    if grep -qs '^enabled' /proc/sys/fs/binfmt_misc/qemu-aarch64; then
      cargo-zigbuild test --workspace --locked --target $target {{ args }} -- "${skip[@]}"
    else
      echo "warning: no binfmt_misc entry for aarch64: skipping ranikust's cli and metrics tests" >&2
      cargo-zigbuild test --workspace --exclude ranikust --locked --target $target {{ args }} -- "${skip[@]}"
      cargo-zigbuild test -p ranikust --bins --locked --target $target {{ args }} -- "${skip[@]}"
    fi

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
ci: fmt-check lint test build-static check-aarch64 test-aarch64 deny doc book

# Until `rnk-builder` exists, `smoke` strings the finished crates together:
# the `smoke-build` example pulls an image, extracts it, runs `RUN` commands
# in the sandbox inside rootless `podman run --privileged` (T1: root there is
# the user's uid), writes a one-layer OCI layout, and podman runs the result.

# Smoke test of pull, extract, sandboxed RUN and a one-layer image (T1). Requires zig, cargo-zigbuild, rootless podman.
smoke image="docker.io/library/alpine:3.20":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(id -u)" = 0 ] || [ "$(podman info --format '{{{{.Host.Security.Rootless}}')" != true ]; then
      echo "error: run as a normal user with rootless podman (T1); real root belongs in the VM (T2)" >&2
      exit 1
    fi
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo zigbuild --release --locked --target {{ musl_target }} -p ranikust --example smoke-build --bin ranikust
    TARGET="${CARGO_TARGET_DIR:-$PWD/target}"
    BIN="$TARGET/{{ musl_target }}/release/examples/smoke-build"
    RNK="$TARGET/{{ musl_target }}/release/ranikust"
    OUT="$TARGET/smoke/oci"
    NAME=localhost/ranikust-smoke:latest
    rm -rf "$OUT" "$OUT.tar" && mkdir -p "$OUT"
    source tests/smoke/lib.bash
    runs=(--run 'echo "hello from ranikust" > /hello' --run 'apk add --no-cache jq')
    for c in "${smoke_checks[@]}"; do runs+=(--run "$c"); done
    podman run --rm --privileged \
      -v "$BIN:/smoke-build:ro" -v "$OUT:/out" \
      "{{ image }}" /smoke-build --image "{{ image }}" --out /out --tag "$NAME" "${runs[@]}"
    smoke_check_no_stub_leak "$OUT"
    # `podman load` reads the layout as an OCI archive and names the image
    # after its `ref.name` annotation.
    tar -C "$OUT" -cf "$OUT.tar" .
    podman load --quiet -i "$OUT.tar"
    podman run --rm "$NAME" sh -c 'cat /hello && jq --version'
    # The ranikust binary builds tests/smoke/e2e. The workdir is a volume:
    # overlay refuses an upper directory on the container's own overlay
    # root (that environment needs kaniko mode).
    E="$TARGET/smoke/e2e" && rm -rf "$E" && mkdir -m 700 -p "$E/w" "$E/out"
    printf %s "$SMOKE_E2E_SECRET" >"$E/token"
    smoke_e2e_args /ctx /tmp/w /out /token
    e2e() {
      local rc=0 log
      log="$(podman run --rm --privileged -v "$RNK:/ranikust:ro" -v "$PWD/tests/smoke/e2e:/ctx:ro" \
        -v "$E/w:/tmp/w" -v "$E/out:/out" -v "$E/token:/token:ro" "{{ image }}" \
        /ranikust "${e2e_args[@]}" "$@" 2>&1)" || rc=$?
      smoke_check_build "$log" pivot_root "$rc" >&2
      smoke_digest "$log"
    }
    e2e >/dev/null
    smoke_check_no_stub_leak "$E/out/oci"
    smoke_check_no_secret "$E/out/oci" "$SMOKE_E2E_SECRET"
    podman load --quiet -i "$E/out/image.tar"
    got="$(podman run --rm localhost/rnk-e2e:latest)"
    [ "$got" = "$SMOKE_E2E_OUTPUT" ] || { echo "error: the e2e image printed: $got" >&2; exit 1; }
    [ "$(podman run --rm localhost/rnk-e2e:latest id -un)" = "$SMOKE_E2E_USER" ]
    # --reproducible: one digest, whatever --jobs.
    a="$(e2e --reproducible)" && b="$(e2e --reproducible --jobs 1)"
    [ "$a" = "$b" ] || { echo "error: --reproducible digests differ: $a, $b" >&2; exit 1; }
    echo "smoke: ok"

# Tier T2 suites (`unit`: the `#[ignore = "T2: ..."]` tests; `smoke`: smoke-build as real root and in rootful Docker) in a disposable QEMU/KVM VM, never on the host. RANIKUST_VM_RELEASE=noble|jammy picks the kernel. Requires /dev/kvm access, qemu, xorriso, zig, cargo-zigbuild.
test-vm *suites:
    tests/vm/vm.sh {{ suites }}
