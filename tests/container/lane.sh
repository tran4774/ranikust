#!/usr/bin/env bash
# The T2c lane (ADR-0063): runs the `#[ignore = "T2c: <profile>: ..."]`
# tests as root in throwaway rootful Docker containers, one per profile,
# on a disposable test machine. Never on a developer's machine: it refuses
# unless the machine carries the root-owned marker /etc/ranikust/throwaway-
# test-host (made once, by hand, only on the test machine: docs/testing.md)
# and RANIKUST_CONTAINER_LANE=1 (tests/container/remote.sh sets it there).
# The sysadmin and netadmin profiles are as good as root on the machine's
# kernel, so the marker is what keeps them off any other machine.
# Containers are never --privileged, mount no host directory (the test
# binaries are copied into an image), have no network, and are removed when
# they exit; the results are their output, written here by the shell.
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

# First of all, before any other check or command: this machine must say
# it is a throwaway test host, in a file only its root could have written.
MARKER=/etc/ranikust/throwaway-test-host
refuse() { die "not a throwaway test host ($MARKER: $1); the T2c lane never runs elsewhere (ADR-0063)"; }
[ -e "$MARKER" ] || refuse "missing"
[ ! -L "$MARKER" ] && [ -f "$MARKER" ] || refuse "not a regular file"
[ ! -L "${MARKER%/*}" ] || refuse "its directory is a link"
for f in "${MARKER%/*}" "$MARKER"; do
  read -r owner mode < <(stat -c '%u %a' "$f")
  [ "$owner" = 0 ] || refuse "$f not owned by root"
  # No write bit for group or others (the last two octal digits even and
  # without 2: 0, 1, 4 or 5).
  case "${mode: -2}" in [0145][0145]) ;; *) refuse "$f writable by group or others (mode $mode)" ;; esac
done
host_name="$(head -c 256 "$MARKER" | tr -cd '[:alnum:]._-')"
[ -n "$host_name" ] || refuse "empty"

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

log "throwaway test host: $host_name"
# One lane run at a time on this machine: the clean-up below removes every
# labelled container. The lock is in the user's own runtime directory.
command -v flock >/dev/null || die "flock is missing"
exec 8>>"${XDG_RUNTIME_DIR:-$HOME}/.ranikust-t2c.lock"
flock -w 3600 8 || die "another T2c lane run held the lock for an hour"
# Leftovers of an interrupted run (every container and image of the lane is
# labelled), so none outlives the run that made it.
LABEL=org.ranikust.lane=t2c
docker ps -aq --filter "label=$LABEL" | xargs -r docker rm -f >/dev/null
docker image ls -q --filter "label=$LABEL" | sort -u | xargs -r docker image rm -f >/dev/null 2>&1 || true

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
# The trap before the build: an interrupted build leaves no image either.
trap 'docker ps -aq --filter "label=$LABEL" | xargs -r docker rm -f >/dev/null 2>&1; docker image rm -f "$tag" >/dev/null 2>&1 || true' EXIT
docker build -q --label "$LABEL" -t "$tag" "$STAGE" >/dev/null

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
  docker run --rm --network none --name "rnk-t2c-$p-$$" --label "$LABEL" \
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
