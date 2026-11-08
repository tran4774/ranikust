# Development tasks for ranikust. Run `just` to list the recipes.
#
# The recipes that mirror CI run exactly the commands of the matching job in
# `.github/workflows/ci.yml`. Change both together. CI runs a job only when
# its lane needs it (ADR-0095, scripts/ci.py); `just ci` runs them all, as
# CI's full run on `main` does.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Recipe arguments reach the recipe as positional parameters. Use them as
# "$@" or "$1", never as `{{ arg }}`: just splices `{{ }}` into the shell
# line as text, so an argument such as 'x; cmd' would run cmd.
set positional-arguments

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
    cargo test --workspace --locked "$@"

# The musl recipes run through cargo-zigbuild (ADR-0020), which compiles every
# C dependency (libzstd, mimalloc, ring...) with `zig cc` for the target, so
# no musl cross compiler is needed. rustc still links x86_64 with the host
# `cc`: zig's linker would drop static-pie and the symbol table.

# Static x86_64 musl release binary, checked to need no loader or shared libraries (CI job `static-build`). Requires zig and cargo-zigbuild.
build-static:
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo zigbuild --release --locked --target {{ musl_target }} -p ranikust
    scripts/check-static.sh {{ static_bin }}

# Type-check for aarch64 musl so no architecture-specific code slips in; quicker than `test-aarch64`. Local only: CI's `test-aarch64` job builds and tests that target (ADR-0095). Requires zig and cargo-zigbuild.
check-aarch64:
    cargo-zigbuild check --workspace --locked --target aarch64-unknown-linux-musl

# aarch64 (ADR-0009): the static release binary, then the T0 tests, built with
# cargo-zigbuild. On an aarch64 host (CI's `ubuntu-24.04-arm` runner) they run
# natively and nothing is skipped. Elsewhere they run under qemu-user through
# cargo's target runner, QEMU 9.1 or later (older qemu-user has no openat2,
# which RootPath needs), and these are skipped, with the reason:
# - `open_gives_cloexec_path_fd_and_mount_id`,
#   `the_binarys_mount_is_measured_on_the_file`,
#   `a_file_in_the_root_is_weighed_by_the_roots_file_system`,
#   `a_mount_root_is_reached_at_its_mount_point`: qemu-user's
#   statx leaves `stx_mnt_id` at 0 (QEMU 10.0 and 10.2), so the mount id
#   they assert is not seen (a fallback through /proc/self/fdinfo is #95);
# - `without_root_the_set_up_refuses_before_touching_the_state_dir`,
#   `without_root_the_set_up_never_makes_the_default_state_dir`,
#   `kaniko_variables_are_not_read`, `logging_flags_change_what_is_written`,
#   `the_store_opens_inside_the_user_namespace_or_not_at_all`,
#   `red_team_the_build_user_namespace_is_whole_or_not_entered`,
#   `prune_all_empties_in_the_user_namespace_or_changes_nothing`,
#   `prune_all_as_a_user_empties_a_state_directory_or_changes_nothing`:
#   qemu-user runs a thread of its own, so the set-up, or
#   `enter_build_user_namespace`, stops at its single-thread check before
#   the privilege refusal or the user namespace the tests expect;
# - `the_filter_refuses_what_it_should_and_nothing_else` and the tests of every
#   `tests::kernel` module: a seccomp filter written for aarch64
#   syscall numbers cannot be installed in the x86_64 host kernel through
#   qemu-user;
# - without a binfmt_misc entry for aarch64, the tests that spawn an aarch64
#   binary themselves: ranikust's `cli` and `metrics`, rnk-store's
#   `processes`, and rnk-sandbox's
#   `a_copy_an_interpreter_must_reopen_is_refused_before_the_exec` and
#   `a_closures_child_writes_through_no_hook_or_subscriber_of_its_parent`.

# Static aarch64 musl binary and T0 tests, natively on aarch64 (CI job `test-aarch64`) or under qemu-user elsewhere. QEMU_AARCH64 names the emulator. Requires zig, cargo-zigbuild; on x86_64 also qemu-user 9.1 or later.
test-aarch64 *args:
    #!/usr/bin/env bash
    set -euo pipefail
    target=aarch64-unknown-linux-musl
    BIN=target/$target/release/ranikust
    # The linker, as for x86_64 (ADR-0020, Q-140): a gcc driver that targets
    # aarch64 gives static-pie with a symbol table, zig's linker gives
    # neither. Native `cc` on aarch64 (CI), a cross gcc on x86_64.
    if [ "$(uname -m)" = aarch64 ]; then
      linker=cc
    elif command -v aarch64-linux-gnu-gcc >/dev/null; then
      linker=aarch64-linux-gnu-gcc
    else
      linker=
      echo "warning: no aarch64-linux-gnu-gcc: linking with zig, so the binary is not static-pie and static-pie is not checked (install gcc-aarch64-linux-gnu)" >&2
    fi
    if [ -n "$linker" ]; then
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$linker"
      # rustc's aarch64-unknown-linux-musl target has no static-pie: it links
      # `-static -no-pie` with crt1.o. Ask for PIE code and link musl's own
      # self-relocating start file (rcrt1.o) and libc.a from rustc's
      # self-contained directory by hand, as x86_64-unknown-linux-musl does
      # by itself; the gcc driver only links, so its glibc is never used.
      sc="$(rustc --print sysroot)/lib/rustlib/$target/lib/self-contained"
      flags="-C relocation-model=pie -C link-self-contained=no"
      for a in -static-pie -nostdlib "-L$sc" "$sc/rcrt1.o" "$sc/crti.o" "$sc/crtbeginS.o" \
               "$sc/libc.a" -lgcc "$sc/crtendS.o" "$sc/crtn.o"; do
        flags+=" -C link-arg=$a"
      done
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="$flags"
    fi
    static_check() {
      file "$BIN"
      if readelf --program-headers --wide "$BIN" | grep -q 'program interpreter' ||
         readelf --dynamic --wide "$BIN" | grep -q '(NEEDED)'; then
        echo "error: $BIN is not static" >&2
        exit 1
      fi
      [ -n "$linker" ] || return 0
      if ! readelf --file-header --wide "$BIN" | grep -q 'Type: *DYN'; then
        echo "error: $BIN is not position-independent (static-pie)" >&2
        exit 1
      fi
      if ! readelf --section-headers --wide "$BIN" | grep -q '\.symtab'; then
        echo "error: $BIN has no symbol table" >&2
        exit 1
      fi
    }
    if [ "$(uname -m)" = aarch64 ]; then
      echo "native aarch64: no emulation, nothing skipped"
      cargo-zigbuild build --release --locked --target $target -p ranikust
      static_check
      "$BIN" version
      cargo-zigbuild test --workspace --locked --target $target "$@"
      exit 0
    fi
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
    cargo-zigbuild build --release --locked --target $target -p ranikust
    static_check
    "$qemu" "$BIN" version
    skip=(--skip root::tests::open_gives_cloexec_path_fd_and_mount_id
      --skip seal::tests::the_binarys_mount_is_measured_on_the_file
      --skip seal::tests::a_file_in_the_root_is_weighed_by_the_roots_file_system
      --skip seal::tests::a_mount_root_is_reached_at_its_mount_point
      --skip without_root_the_set_up_refuses_before_touching_the_state_dir
      --skip without_root_the_set_up_never_makes_the_default_state_dir
      --skip kaniko_variables_are_not_read
      --skip logging_flags_change_what_is_written
      --skip setup::tests::userns::the_store_opens_inside_the_user_namespace_or_not_at_all
      --skip userns::build::tests::red_team_the_build_user_namespace_is_whole_or_not_entered
      --skip setup::tests::userns::prune_all_empties_in_the_user_namespace_or_changes_nothing
      --skip prune_all_as_a_user_empties_a_state_directory_or_changes_nothing
      --skip seccomp::tests::the_filter_refuses_what_it_should_and_nothing_else
      --skip tests::kernel::)
    if grep -qs '^enabled' /proc/sys/fs/binfmt_misc/qemu-aarch64; then
      cargo-zigbuild test --workspace --locked --target $target "$@" -- "${skip[@]}"
    else
      echo "warning: no binfmt_misc entry for aarch64: skipping the tests that spawn a binary (ranikust's cli and metrics, rnk-store's processes, rnk-sandbox's sealed-copy interpreter test and closure's child test)" >&2
      skip+=(--skip concurrent_fills_from_processes_run_the_filler_once
        --skip a_build_killed_mid_fill_leaves_nothing_behind
        --skip seal::tests::a_copy_an_interpreter_must_reopen_is_refused_before_the_exec
        --skip userns::tests::a_closures_child_writes_through_no_hook_or_subscriber_of_its_parent)
      cargo-zigbuild test --workspace --exclude ranikust --locked --target $target "$@" -- "${skip[@]}"
      cargo-zigbuild test -p ranikust --bins --locked --target $target "$@" -- "${skip[@]}"
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
    git cliff --unreleased "$@"

# Prepare a release (ADR-0089; CONTRIBUTING.md, Releasing): set the workspace version, refresh Cargo.lock offline, and draft the CHANGELOG.md section from the `## Changelog` sections of the pull requests merged since the last tag. Edits files only: never commits, tags or pushes. Requires python3, cargo and gh.
release-prepare version:
    python3 -I scripts/release.py prepare "$1"

# THIRD-PARTY-LICENSES as the release ships it (ADR-0094), from the same steps as the release workflow's `notices` job: cargo-about over Cargo.lock for x86_64 musl, offline after `cargo fetch`, failing on a licence deny.toml does not allow, then the toolchain's notices from licenses/; writes target/THIRD-PARTY-LICENSES. Requires cargo-about 0.9.2 (`cargo install --locked cargo-about@0.9.2`) and python3.
third-party-licenses:
    cargo fetch --locked
    cargo about --version
    cargo about generate --frozen --fail -m crates/ranikust/Cargo.toml -c about.toml --target {{ musl_target }} -o target/cargo-about.txt scripts/third-party-licenses.hbs
    python3 -I scripts/release.py notices target/cargo-about.txt target/THIRD-PARTY-LICENSES

# Tests of the release scripts (CI: the `Release` workflow's build job): scripts/check-static.sh with a fake readelf, and scripts/release.py with fake gh and git remotes; no network, no privileges. Requires bash, python3 3.11 or later and git.
test-release:
    bash tests/release/check-static-test.sh
    python3 -I tests/release/test_release.py

# Mechanical checks of the decision records over the files git ls-files lists (CI job `docs-lint`, which sets DOCS_LINT_REQUIRE_GITHUB=1): every ADR indexed with its status, no duplicate question in docs/open-questions.md, no old `[open: Q-NNN]` marker outside ADR-0001 to ADR-0089, every `[open: #N]` naming an open issue labelled question (any question issue inside an ADR), asked of GitHub through gh; without gh, a logged-in gh or network that check is skipped and the verdict says so; every relative Markdown link, link to this repository on github.com (blob/main, tree/main, the front page) and Markdown path named in a code span or in code resolving to a listed file and anchor; book links staying inside docs/book/src. Requires python3; gh for the issue check.
docs-lint:
    python3 -I scripts/docs-lint.py

# Tests of docs-lint's link check on a throwaway tree (CI: the `docs-lint` job, before docs-lint). No network. Requires python3.
test-docs-lint:
    python3 -I tests/docs/test_links.py

# Tests of CI's lane detection and cache pruning, scripts/ci.py, and of how the workflows wire them (ADR-0095; CI: the `changes` job). Fails on a tracked file no lane rule names. No network, no privileges. Requires python3 3.11 or later and git.
test-ci:
    python3 -I tests/ci/test_ci.py

# Local only: print the brief the `host-safety` subagent needs for a branch against main (changed files by crate, diff stat, `graphify affected`, threat-model rows, design sections, crate boundaries). Requires python3; graphify optional.
review-pack branch *args:
    python3 -I scripts/review-pack.py "$@"

# Local only: remove the agent worktrees under .claude/worktrees/ that hold nothing to lose, with their local branch and build output (ADR-0096): a pull request merged into main with the branch's commit as head, or no commit of its own and idle a day; nothing changed or untracked, nothing ignored outside target/, graphify-out/ and __pycache__/; no live VM run; not locked, detached, on main or the caller's. `--dry-run` only lists (it still fetches origin/main). Requires git and a logged-in gh.
prune-worktrees *args:
    bash scripts/prune-worktrees.sh "$@"

# Local only: tests scripts/prune-worktrees.sh in a throwaway repository with a fake gh; no network.
test-prune-worktrees:
    bash tests/agents/prune-worktrees-test.sh

# Everything ci.yml runs on a full run (a push to `main`), in CI's order; on a pull request CI skips what the change does not need (ADR-0095).
ci: test-ci fmt-check lint test build-static test-aarch64 deny doc book test-docs-lint docs-lint

# Until `rnk-builder` exists, `smoke` strings the finished crates together:
# the `smoke-build` example pulls an image, extracts it, runs `RUN` commands
# in the sandbox inside rootless `podman run --privileged` (T1: root there is
# the user's uid), writes a one-layer OCI layout, and podman runs the result.

# Smoke test of pull, extract, sandboxed RUN and a one-layer image (T1). Requires zig, cargo-zigbuild, rootless podman.
smoke image="docker.io/library/alpine:3.20":
    #!/usr/bin/env bash
    set -euo pipefail
    smoke_image="$1"
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
      "$smoke_image" /smoke-build --image "$smoke_image" --out /out --tag "$NAME" "${runs[@]}"
    smoke_check_no_stub_leak "$OUT"
    # `podman load` reads the layout as an OCI archive and names the image
    # after its `ref.name` annotation.
    tar -C "$OUT" -cf "$OUT.tar" .
    podman load --quiet -i "$OUT.tar"
    podman run --rm "$NAME" sh -c 'cat /hello && jq --version'
    # Root of a default `podman run`: the build's user namespace, with an
    # rbind of the container's /proc and /sys (#78), so without the check
    # of the RUN's own PID 1; nor the host name's: Podman's seccomp profile
    # refuses `sethostname` without CAP_SYS_ADMIN.
    OUT_U="$TARGET/smoke/oci-userns" NAME_U=localhost/ranikust-smoke-userns:latest
    rm -rf "$OUT_U" "$OUT_U.tar" && mkdir -p "$OUT_U"
    # Unasked, the rbind of /proc is refused (ADR-0126): the probe stops at
    # the new proc, before anything is pulled.
    if podman run --rm -v "$BIN:/smoke-build:ro" "$smoke_image" \
      /smoke-build --userns --image "$smoke_image" --out /out >"$OUT_U.refused.log" 2>&1; then
      echo "error: --userns without --allow proc.rbind: expected a refusal" >&2; exit 1
    fi
    grep -q '^smoke-build: RUN sandbox: mount proc: Operation not permitted' "$OUT_U.refused.log" ||
      { echo "error: --userns without --allow proc.rbind: not refused at the new proc:" >&2;
        cat "$OUT_U.refused.log" >&2; exit 1; }
    runs=(--run 'echo "hello from ranikust" > /hello' --run 'apk add --no-cache jq')
    for c in "${smoke_checks[@]:smoke_checks_own_proc}"; do
      [[ $c == *'$(hostname)'* ]] || runs+=(--run "$c")
    done
    podman run --rm -v "$BIN:/smoke-build:ro" -v "$OUT_U:/out" \
      "$smoke_image" /smoke-build --userns --allow proc.rbind --image "$smoke_image" --out /out \
      --tag "$NAME_U" "${runs[@]}" | tee "$OUT_U.log"
    grep -qx 'probe: Caps { root_entry: PivotRoot, proc: Rbind, sysfs: Rbind }' "$OUT_U.log" ||
      { echo "error: --userns: expected an rbind of /proc and of /sys" >&2; exit 1; }
    smoke_check_no_stub_leak "$OUT_U"
    tar -C "$OUT_U" -cf "$OUT_U.tar" .
    podman load --quiet -i "$OUT_U.tar"
    podman run --rm "$NAME_U" sh -c 'cat /hello && jq --version'
    # The ranikust binary builds tests/smoke/e2e. The workdir is a volume:
    # overlay refuses an upper directory on the container's own overlay
    # root (that environment needs kaniko mode).
    E="$TARGET/smoke/e2e" && rm -rf "$E" && mkdir -m 700 -p "$E/w" "$E/out"
    printf %s "$SMOKE_E2E_SECRET" >"$E/token"
    smoke_e2e_args /ctx /tmp/w /out /token
    e2e() {
      local rc=0 log
      log="$(podman run --rm --privileged -v "$RNK:/ranikust:ro" -v "$PWD/tests/smoke/e2e:/ctx:ro" \
        -v "$E/w:/tmp/w" -v "$E/out:/out" -v "$E/token:/token:ro" "$smoke_image" \
        /ranikust "${e2e_args[@]}" "$@" 2>&1)" || rc=$?
      smoke_check_build "$log" pivot_root "$rc" >&2 || return 1
      smoke_digest "$log"
    }
    e2e >/dev/null
    smoke_check_no_stub_leak "$E/out/oci"
    smoke_check_no_secret "$E/out/oci" "$SMOKE_E2E_SECRET"
    smoke_check_no_empty_layer "$E/out/oci"
    podman load --quiet -i "$E/out/image.tar"
    got="$(podman run --rm localhost/rnk-e2e:latest)"
    [ "$got" = "$SMOKE_E2E_OUTPUT" ] || { echo "error: the e2e image printed: $got" >&2; exit 1; }
    [ "$(podman run --rm localhost/rnk-e2e:latest id -un)" = "$SMOKE_E2E_USER" ]
    # --reproducible: one digest, whatever --jobs.
    a="$(e2e --reproducible)" && b="$(e2e --reproducible --jobs 1)"
    [ "$a" = "$b" ] || { echo "error: --reproducible digests differ: $a, $b" >&2; exit 1; }
    echo "smoke: ok"

# The quick smoke cases of tests/smoke/cases (T1, rootless podman): small Dockerfiles, one concern each, a few seconds per build. Names pick cases; none runs them all. Requires zig, cargo-zigbuild, rootless podman, python3.
smoke-case *names:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(id -u)" = 0 ] || [ "$(podman info --format '{{{{.Host.Security.Rootless}}')" != true ]; then
      echo "error: run as a normal user with rootless podman (T1); real root belongs in the VM (T2)" >&2
      exit 1
    fi
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo zigbuild --release --locked --target {{ musl_target }} -p ranikust --bin ranikust
    tests/smoke/run-cases.sh "${CARGO_TARGET_DIR:-$PWD/target}/{{ musl_target }}/release/ranikust" "$@"

# Tier T2 suites (`unit`: every `#[ignore = "T2: ..."]` and `"T2c: ..."` test; `smoke`: smoke-build as real root and in rootful Docker; `quick`: the quick smoke cases as real root; `e2e`, only when named: the light W0 workload against kaniko and BuildKit; `ultra`, only when named and asked for: the W1-W3 benchmark; `push`, only when named: pushes to a loopback and the host's test registries, needs `just registry-up`) in a disposable QEMU/KVM VM, never on the host. RANIKUST_VM_RELEASE=noble|jammy picks the kernel; alma9, AlmaLinux 9 with SELinux enforcing, runs only `el9`, the privilege matrix's root and regular-user legs there. Requires /dev/kvm access, qemu, xorriso, zig, cargo-zigbuild.
test-vm *suites:
    tests/vm/vm.sh "$@"

# Tier T2c (ADR-0063): the `#[ignore = "T2c: <profile>: ..."]` tests as root in throwaway rootful Docker containers (default, +SYS_ADMIN, +NET_ADMIN; never --privileged, no host volume) on the disposable test machine, through the lane helper (RANIKUST_LANE, default rnk-vps-lane). Sends the current branch as committed. Suites: `unit-c`.
test-container *suites:
    bash tests/container/remote.sh "$@"

# Tier T1: the `#[ignore = "T1: <profile>: ..."]` tests as root of rootless Podman containers (default; --privileged), on this machine as a regular user: static test binaries mounted read-only, the tests writing only in the container's tmpfs, no network. Requires zig, cargo-zigbuild, jq, rootless podman and subordinate ids for the user.
test-rootless:
    bash tests/rootless/lane.sh

# Start the test registries (tests/registry/compose.yaml): plain HTTP on 127.0.0.1:5000 and HTTPS with a self-signed CA and basic auth on 127.0.0.1:5443, data in tests/registry/_data; then push and read back an image in each. Requires podman (compose), openssl, python3-bcrypt, skopeo.
registry-up:
    tests/registry/setup.sh
    SUPPRESS_BOLTDB_WARNING=1 podman compose -f tests/registry/compose.yaml up -d
    tests/registry/check.sh

# Stop the test registries; their data stays in tests/registry/_data.
registry-down:
    SUPPRESS_BOLTDB_WARNING=1 podman compose -f tests/registry/compose.yaml down
