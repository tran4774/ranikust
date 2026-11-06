#!/usr/bin/env bash
# The T1 lane: runs the `#[ignore = "T1: <profile>: ..."]` tests as root of
# rootless Podman containers, one per profile. Root there is the user who
# runs this, with the user's subordinate ids: nothing runs as real root, so
# it runs on a developer's machine. The static test binaries are mounted
# read-only; the tests write only in the container's /work, a tmpfs gone
# with it. Containers have no network and are removed when they exit.
#
#   tests/rootless/lane.sh        (just test-rootless)
#
# Profiles:
#   default     podman run as is (rootless Podman's capabilities and seccomp)
#   privileged  + --privileged, as `just smoke`: mounts in the container
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
log() { echo "[t1] $*"; }

[ "$(id -u)" != 0 ] || die "run as a regular user with rootless podman (T1); real root belongs in the VM (T2)"
for tool in podman jq cargo-zigbuild; do
  command -v "$tool" >/dev/null || die "$tool is missing"
done
export SUPPRESS_BOLTDB_WARNING=1
[ "$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null)" = true ] ||
  die "podman here is not rootless; T1 needs rootless Podman"

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
OUT="$TARGET/rootless"
STAGE="$OUT/stage"
MUSL=x86_64-unknown-linux-musl
# Ubuntu 24.04, as the T2c image and the VM.
IMAGE=docker.io/library/ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
PROFILES=(default privileged)

rm -rf "$OUT"
mkdir -p "$STAGE/bin"

# The fn under each marker.
fns() {
  (cd "$REPO" && grep -rn --include='*.rs' -A3 "$1" crates || true) |
    sed -nE 's/^.*fn ([A-Za-z0-9_]+).*$/\1/p' | sort -u
}
total=0
for p in "${PROFILES[@]}"; do
  fns "#\[ignore = \"T1: $p:" >"$STAGE/$p.txt"
  total=$((total + $(wc -l <"$STAGE/$p.txt")))
done
# Selection is by fn name: every T1 test names a known profile, and no name
# is in two lists.
all="$(fns '#\[ignore = "T1: ' | wc -l)"
[ "$all" = "$total" ] || die "$all T1 tests, $total of them in a profile (${PROFILES[*]}): mark each \"T1: <profile>: ...\""
both="$(comm -12 "$STAGE/default.txt" "$STAGE/privileged.txt")"
[ -z "$both" ] || die "test names in both profiles: $(echo "$both" | paste -sd' ' -)"
mapfile -t crates < <(cd "$REPO" && grep -rl --include='*.rs' '#\[ignore = "T1: ' crates | cut -d/ -f2 | sort -u)
[ "${#crates[@]}" -gt 0 ] || die "no T1 tests found"

log "building the test binaries of ${crates[*]} ($total T1 tests)"
pkgs=(); for c in "${crates[@]}"; do pkgs+=(-p "$c"); done
(cd "$REPO" && CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc \
  cargo-zigbuild test --no-run --locked --target "$MUSL" --message-format=json "${pkgs[@]}") |
  jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null) | .executable' |
  while read -r exe; do cp "$exe" "$STAGE/bin/"; done
[ -n "$(ls -A "$STAGE/bin")" ] || die "no test binary was built"
cp "$REPO/tests/rootless/run-profile.sh" "$STAGE/"

status=0
for p in "${PROFILES[@]}"; do
  n=$(wc -l <"$STAGE/$p.txt")
  [ "$n" -gt 0 ] || continue
  flags=()
  [ "$p" = privileged ] && flags=(--privileged)
  log "profile $p: $n tests"
  rc=0
  podman run --rm --network none --name "rnk-t1-$p-$$" \
    --tmpfs /work:rw,exec,mode=1777,size=2g \
    -v "$STAGE:/t1:ro" "${flags[@]}" "$IMAGE" /t1/run-profile.sh "$p" >"$OUT/$p.log" 2>&1 || rc=$?
  grep -E '^(control:|test .* \.\.\. |measured: |ok$|T1 |error)' "$OUT/$p.log" || true
  log "profile $p: exit $rc"
  if [ "$rc" != 0 ]; then
    tail -40 "$OUT/$p.log"
    status=1
  fi
done
log "logs in $OUT"
[ "$status" = 0 ] && echo "test-rootless: ok" || echo "test-rootless: FAILED"
exit "$status"
