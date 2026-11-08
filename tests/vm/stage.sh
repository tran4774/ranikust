#!/usr/bin/env bash
# Stages what the T2 suites run: the guest scripts with the smoke files they
# read, quick.env, and the static musl binaries the named suites need (the
# root tests' binaries and their list for `unit`; the ranikust binary and the
# smoke-build example for the others, and for `net`, guest/net.sh's network
# check, which builds with ranikust); for `k8s`, also the k3s binary of
# guest/k8s/pin.env, checked against its pinned SHA-256. One tree for both
# places T2 runs: tests/vm/vm.sh copies it into its VM as ~/rnk,
# tests/vm/runner.sh puts it at ~/rnk on a GitHub-hosted runner (ADR-0090).
# Builds as the invoking user; nothing here needs root.
#
#   tests/vm/stage.sh <empty stage directory> [net] [suite]...
#
# RANIKUST_LOG_T0, the caller's seconds since its start, puts the `[stage
# +Ns]` lines on the caller's clock, so a run's log keeps one timeline.
# RANIKUST_K3S_CACHE, a directory of the caller's (vm.sh: the VM image
# cache), keeps the k3s download for the next run; this script deletes
# there a download that failed and, after one that did not, every other
# k3s-* file (older pins). Without it the download goes straight into the
# stage.
set -euo pipefail

T0="${RANIKUST_LOG_T0:-0}"
[[ "$T0" =~ ^(0|[1-9][0-9]{0,8})$ ]] || T0=0
log() { printf '[stage +%ss] %s\n' "$((T0 + SECONDS))" "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ $# -ge 1 ] || die "usage: tests/vm/stage.sh <empty stage directory> [net] [suite]..."
STAGE="$1"
shift
SUITES=("$@")
# The caller makes it, and empties it: this script deletes nothing in it.
case "$STAGE" in /*) ;; *) die "the stage directory must be an absolute path (got '$STAGE')" ;; esac
[ -d "$STAGE" ] && [ ! -L "$STAGE" ] || die "$STAGE is not a directory"
[ -z "$(ls -A "$STAGE")" ] || die "$STAGE is not empty"
for s in ${SUITES[@]+"${SUITES[@]}"}; do
  case "$s" in net | unit | smoke | quick | e2e | push | secrets | ultra | k8s | el9) ;; *) die "unknown suite '$s'" ;; esac
done
wants() { local s; for s in ${SUITES[@]+"${SUITES[@]}"}; do [ "$s" = "$1" ] && return 0; done; return 1; }

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$REPO/tests/vm"
TARGET="$(realpath -m -- "${CARGO_TARGET_DIR:-$REPO/target}")"
MUSL=x86_64-unknown-linux-musl

mkdir -p "$STAGE/bin/tests"
cp -r "$HERE/guest" "$STAGE/guest"
cp "$REPO/tests/smoke/lib.bash" "$STAGE/guest/smoke-lib.bash"
# The smoke suite's build context; not `guest/e2e`, which holds the e2e
# suite's scripts.
cp -r "$REPO/tests/smoke/e2e" "$STAGE/guest/smoke-e2e"
# The checks of a RUN in a network namespace a user namespace owns.
cp "$REPO/tests/smoke/netns-check.py" "$STAGE/guest/netns-check.py"
# The quick smoke cases and their runner.
cp "$REPO/tests/smoke/cases.bash" "$STAGE/guest/smoke-cases.bash"
cp "$REPO/tests/smoke/oci-config.py" "$STAGE/guest/oci-config.py"
cp -r "$REPO/tests/smoke/cases" "$STAGE/guest/cases"
# Quoted: `RANIKUST_QUICK_CASES="run copy"` names two cases.
[[ ${RANIKUST_QUICK_BENCH:-} =~ ^1?$ ]] || die "RANIKUST_QUICK_BENCH must be 1 or unset"
[[ ${RANIKUST_QUICK_BENCH_ROUNDS:-1} =~ ^[1-9][0-9]?$ ]] || die "RANIKUST_QUICK_BENCH_ROUNDS must be 1-99"
: >"$STAGE/quick.env"
for v in RANIKUST_QUICK_CASES RANIKUST_QUICK_MODE RANIKUST_QUICK_BENCH RANIKUST_QUICK_BENCH_ROUNDS; do
  [ -z "${!v:-}" ] || printf 'export %s=%q\n' "$v" "${!v}" >>"$STAGE/quick.env"
done
# Without vm.sh's lock descriptor: a build server or podman's pause process
# left running must not hold the one-VM lock after the run.
zigbuild() { CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo-zigbuild "$1" --locked --target "$MUSL" "${@:2}" 9>&-; }

if wants unit; then
  # Root tests: every `#[ignore = "T2: ..."]` and `"T2c: ..."` (the VM is the
  # reference for the container lane too, ADR-0063) and the fn under it.
  (cd "$REPO" && grep -rn --include='*.rs' -A3 '#\[ignore = "T2' crates) |
    sed -nE 's/^.*fn ([A-Za-z0-9_]+).*$/\1/p' | sort -u >"$STAGE/t2-tests.txt"
  # The crates holding them.
  mapfile -t crates < <(cd "$REPO" && grep -rl --include='*.rs' '#\[ignore = "T2' crates |
    cut -d/ -f2 | sort -u)
  [ -s "$STAGE/t2-tests.txt" ] || die "no T2 tests found"
  log "building the test binaries of ${crates[*]} ($(wc -l <"$STAGE/t2-tests.txt") T2 tests)"
  pkgs=(); for c in "${crates[@]}"; do pkgs+=(-p "$c"); done
  (cd "$REPO" && zigbuild test --no-run --message-format=json "${pkgs[@]}") |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null) | .executable' |
    while read -r exe; do cp "$exe" "$STAGE/bin/tests/"; done
fi
if wants net || wants smoke || wants quick || wants e2e || wants ultra || wants push || wants secrets || wants k8s || wants el9; then
  log "building the smoke-build example and the ranikust binary"
  (cd "$REPO" && zigbuild zigbuild --release -p ranikust --example smoke-build --bin ranikust)
  cp "$TARGET/$MUSL/release/examples/smoke-build" "$TARGET/$MUSL/release/ranikust" "$STAGE/bin/"
fi
if wants k8s; then
  # shellcheck source=guest/k8s/pin.env
  source "$HERE/guest/k8s/pin.env"
  url="https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION//+/%2B}/k3s"
  sha() { sha256sum "$1" | cut -d' ' -f1; }
  if [ -n "${RANIKUST_K3S_CACHE:-}" ]; then
    cached="$RANIKUST_K3S_CACHE/k3s-$K3S_VERSION"
    if [ ! -f "$cached" ] || [ "$(sha "$cached")" != "$K3S_SHA256" ]; then
      log "downloading k3s $K3S_VERSION into $RANIKUST_K3S_CACHE"
      mkdir -p "$RANIKUST_K3S_CACHE"
      curl -fsSL --retry 3 -o "$cached.part" "$url" || { rm -f "$cached.part"; die "downloading k3s $K3S_VERSION failed"; }
      [ "$(sha "$cached.part")" = "$K3S_SHA256" ] || { rm -f "$cached.part"; die "SHA-256 mismatch for k3s $K3S_VERSION"; }
      mv "$cached.part" "$cached"
      # One release kept: the pins before this one are not used again.
      find "$RANIKUST_K3S_CACHE" -maxdepth 1 -type f -name 'k3s-*' ! -name "k3s-$K3S_VERSION" -delete
    fi
    cp "$cached" "$STAGE/bin/k3s"
  else
    log "downloading k3s $K3S_VERSION"
    curl -fsSL --retry 3 -o "$STAGE/bin/k3s" "$url"
  fi
  [ "$(sha "$STAGE/bin/k3s")" = "$K3S_SHA256" ] || die "SHA-256 mismatch for k3s $K3S_VERSION"
  log "k3s $K3S_VERSION sha256 $K3S_SHA256 (verified)"
fi
