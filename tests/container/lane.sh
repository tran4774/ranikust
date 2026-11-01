#!/usr/bin/env bash
# The T2c lane (ADR-0063): runs the `#[ignore = "T2c: <profile>: ..."]`
# tests as root in throwaway rootful Docker containers, one per profile,
# on a disposable test machine. Never on a developer's machine: it refuses
# unless RANIKUST_CONTAINER_LANE=1 (tests/container/remote.sh sets it on
# the test machine). Containers are never --privileged, mount no host
# directory (the test binaries are copied into an image), have no network,
# and are removed when they exit; the results are their output, written
# here by the shell.
#
#   RANIKUST_CONTAINER_LANE=1 tests/container/lane.sh [unit-c]
#
# Profiles:
#   default   docker run as is (Docker's default capabilities, seccomp, AppArmor)
#   sysadmin  + --cap-add SYS_ADMIN --security-opt apparmor=unconfined
#   netadmin  as sysadmin, + --cap-add NET_ADMIN
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
log() { echo "[lane] $*"; }

[ "${RANIKUST_CONTAINER_LANE:-}" = 1 ] ||
  die "the T2c lane runs only on a disposable test machine with rootful Docker (ADR-0063); set RANIKUST_CONTAINER_LANE=1 there, never on your own machine"
[ "$(id -u)" != 0 ] || die "run as a user in the docker group, not as root"
for tool in docker jq cargo-zigbuild; do
  command -v "$tool" >/dev/null || die "$tool is missing"
done
# Rootful Docker only: rootless Docker or Podman maps root to a user, and
# the tests would no longer mean what they say.
sec="$(docker info --format '{{json .SecurityOptions}}' 2>/dev/null)" || die "docker info failed"
case "$sec" in *rootless*) die "Docker here is rootless; T2c needs rootful Docker" ;; esac
docker version --format '{{.Server.Os}}' 2>/dev/null | grep -qx linux || die "no Docker Engine server here"
docker version 2>/dev/null | grep -qi podman && die "this is Podman; T2c needs rootful Docker"

suites=("$@")
[ "${#suites[@]}" -gt 0 ] || suites=(unit-c)
for s in "${suites[@]}"; do
  case "$s" in unit-c) ;; *) die "unknown suite $s (known: unit-c)" ;; esac
done

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
OUT="$TARGET/container"
STAGE="$OUT/stage"
RESULTS="$OUT/results"
MUSL=x86_64-unknown-linux-musl
PROFILES=(default sysadmin netadmin)

rm -rf "$STAGE" "$RESULTS"
mkdir -p "$STAGE/bin" "$RESULTS"

# The tests of each profile: the fn under each marker.
for p in "${PROFILES[@]}"; do
  (cd "$REPO" && grep -rn --include='*.rs' -A3 "#\[ignore = \"T2c: $p:" crates || true) |
    sed -nE 's/^.*fn ([A-Za-z0-9_]+).*$/\1/p' | sort -u >"$STAGE/$p.txt"
done
mapfile -t crates < <(cd "$REPO" && grep -rl --include='*.rs' '#\[ignore = "T2c: ' crates | cut -d/ -f2 | sort -u)
[ "${#crates[@]}" -gt 0 ] || die "no T2c tests found"
total=$(cat "$STAGE"/*.txt | wc -l)
log "building the test binaries of ${crates[*]} ($total T2c tests)"
pkgs=(); for c in "${crates[@]}"; do pkgs+=(-p "$c"); done
(cd "$REPO" && CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc \
  cargo-zigbuild test --no-run --locked --target "$MUSL" --message-format=json "${pkgs[@]}") |
  jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null) | .executable' |
  while read -r exe; do cp "$exe" "$STAGE/bin/"; done
[ -n "$(ls -A "$STAGE/bin")" ] || die "no test binary was built"
cp "$REPO/tests/container/Dockerfile" "$REPO/tests/container/run-profile.sh" "$STAGE/"

tag="rnk-t2c:$(cd "$STAGE" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)"
log "building $tag"
docker build -q -t "$tag" "$STAGE" >/dev/null
trap 'docker image rm -f "$tag" >/dev/null 2>&1 || true' EXIT

status=0
for p in "${PROFILES[@]}"; do
  n=$(wc -l <"$STAGE/$p.txt")
  [ "$n" -gt 0 ] || continue
  flags=()
  case "$p" in
    sysadmin) flags=(--cap-add SYS_ADMIN --security-opt apparmor=unconfined) ;;
    netadmin) flags=(--cap-add SYS_ADMIN --cap-add NET_ADMIN --security-opt apparmor=unconfined) ;;
  esac
  log "profile $p: $n tests"
  t0=$(date +%s)
  rc=0
  docker run --rm --network none --name "rnk-t2c-$p-$$" \
    --tmpfs /work:rw,exec,suid,dev,mode=1777,size=4g \
    --tmpfs /var/tmp:rw,exec,suid,dev,mode=1777,size=4g \
    --memory 6g --pids-limit 8192 --ulimit nofile=65536:65536 \
    "${flags[@]}" "$tag" /t2c/run-profile.sh "$p" >"$RESULTS/$p.log" 2>&1 || rc=$?
  grep -E '^(control:|T2c |error)' "$RESULTS/$p.log" || true
  log "profile $p: exit $rc after $(($(date +%s) - t0)) s"
  if [ "$rc" != 0 ]; then
    tail -40 "$RESULTS/$p.log"
    status=1
  fi
done
log "results in $RESULTS"
[ "$status" = 0 ] && echo "unit-c: ok" || echo "unit-c: FAILED"
exit "$status"
