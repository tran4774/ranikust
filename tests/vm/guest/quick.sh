#!/usr/bin/env bash
# Runs in the T2 VM: the quick smoke cases (tests/smoke/cases) built by the
# ranikust binary as real root on the VM, Docker loading and running what
# they built. RANIKUST_QUICK_CASES names cases (space-separated); unset, all
# run. For a smoke test or a proof of concept that needs real root; the
# nextjs-kaniko workloads (`e2e`) are for large benchmarks only.
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
source guest/smoke-lib.bash
# shellcheck disable=SC1091
source guest/smoke-cases.bash
# shellcheck disable=SC1091
[ -f quick.env ] && source quick.env

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
BASE="$REG/library/alpine:3.20"
RNK="$HOME/rnk/bin/ranikust"
ENGINE="sudo docker"
CASES_OUT="$HOME/rnk/out/quick"
W=/var/tmp/rnk-quick
SECRET="$W/secret"

# $W itself 0700: the state directories and every build root below them
# stay out of other users' reach (ADR-0050).
# Never delete through a mount an earlier run left: refuse instead.
! grep -qF " $W" /proc/self/mountinfo ||
  { echo "error: mounts left under $W by an earlier run" >&2; exit 1; }
sudo rm -rf --one-file-system "$W" && sudo mkdir -m 700 "$W"
printf %s "$SMOKE_CASE_SECRET" | sudo tee "$SECRET" >/dev/null
mkdir -p "$CASES_OUT"

# RANIKUST_QUICK_MODE=kaniko: kaniko mode outside a container, so it builds
# in a root it makes in its state directory (ADR-0050); the VM's root may
# mount, so RUN gets the full sandbox (ADR-0043).
MODE_ARGS=(--state-dir "$W/state")
[ "${RANIKUST_QUICK_MODE:-}" = kaniko ] && MODE_ARGS=(--mode=kaniko --state-dir "$W/kstate")

rnk_run() {
  local ctx="$1" o="$2" rc=0
  shift 2
  sudo "$RNK" --context "$ctx" "${MODE_ARGS[@]}" --no-push \
    --oci-layout-path "$o/oci" --tar-path "$o/image.tar" --secret "id=tok,src=$SECRET" \
    "$@" || rc=$?
  sudo chown -R "$(id -u):$(id -g)" "$o"
  return "$rc"
}

# ranikust runs on the VM itself: the context is where it is.
rnk_ctx() { echo "$1"; }

# ADR-0054: kaniko mode refuses a container's `/` where root may mount
# (`--privileged`) or where /proc/sys is writable
# (`systempaths=unconfined`), before anything is pulled. Run with the
# kaniko-mode suite, unless the cases named leave kaniko-flags out.
adr0054_refusals() {
  local ctx="$W/adr0054" out
  sudo mkdir -p "$ctx" && printf 'FROM scratch\n' | sudo tee "$ctx/Dockerfile" >/dev/null
  for opt in "--privileged:where root may mount" \
    "--security-opt=systempaths=unconfined:writable /proc/sys"; do
    local flag="${opt%%:*}" want="${opt#*:}" rc=0
    out="$(sudo docker run --rm "$flag" -v "$RNK:/ranikust/bin/ranikust:ro" \
      -v "$ctx:/ranikust/ctx:ro" "$BASE" /ranikust/bin/ranikust --mode=kaniko \
      --context /ranikust/ctx --no-push 2>&1)" || rc=$?
    if [ "$rc" != 1 ] || ! grep -q -- "$want" <<<"$out"; then
      echo "error: kaniko mode in a docker run $flag: exit $rc, want 1 and \"$want\"" >&2
      echo "$out" >&2
      return 1
    fi
    echo "adr-0054: docker run $flag refused (exit 1)"
  done
}

# Criterion 8 (C): outside a container where root may mount but overlayfs
# refuses the state directory (here: one on overlayfs), `--mode=auto`
# builds in kaniko mode with a directory per stage instead of refusing.
# The overlay is detached on every exit path.
auto_routes_to_stages() {
  local ov="$W/ov" ctx="$W/auto-ctx" o="$W/auto-out" log rc=0
  sudo mkdir -p "$ov/l" "$ov/u" "$ov/w" "$ov/m" "$ctx" "$o"
  printf 'FROM scratch\nCOPY f /f\n' | sudo tee "$ctx/Dockerfile" >/dev/null
  echo routed | sudo tee "$ctx/f" >/dev/null
  sudo mount -t overlay overlay -o "lowerdir=$ov/l,upperdir=$ov/u,workdir=$ov/w" "$ov/m" || return 1
  # Detached on every exit path, even an interrupted one.
  trap 'sudo umount -l "'"$ov/m"'" 2>/dev/null || true' EXIT
  log="$(sudo "$RNK" --context "$ctx" --state-dir "$ov/m/state" --no-push \
    --oci-layout-path "$o/oci" 2>&1)" || rc=$?
  # A mount the build left below the overlay must fail the check, not be
  # hidden by a lazy unmount of the overlay.
  if grep -qF " $ov/m/" /proc/self/mountinfo; then
    echo "error: mounts left below $ov/m after the build" >&2
    rc=1
  fi
  if ! sudo umount "$ov/m"; then
    echo "error: $ov/m busy after the build; detached lazily" >&2
    sudo umount -l "$ov/m" || true
    rc=1
  fi
  trap - EXIT
  if [ "$rc" != 0 ] ||
    ! grep -q 'kaniko mode with a directory per stage, because overlay mode cannot run here' <<<"$log"; then
    echo "error: --mode=auto with a state directory on overlayfs: exit $rc" >&2
    echo "$log" >&2
    return 1
  fi
  echo "auto: a state directory on overlayfs built in kaniko mode with a directory per stage"
}

# Red team of `--mode=auto`'s routing (criterion 8, ADR-0054, ADR-0059,
# T4): the checks every attack ends with. No mount is left below $1, no
# ranikust process runs, and no build directory outlives the build in the
# state directory $2 (lock files may stay).
rt_nothing_left() {
  local below="$1" state="$2" what="$3"
  if grep -qF " $below/" /proc/self/mountinfo; then
    echo "error: red team: $what: mounts left below $below" >&2
    return 1
  fi
  if pgrep -x ranikust >/dev/null; then
    echo "error: red team: $what: ranikust processes left" >&2
    return 1
  fi
  if sudo test -d "$state/builds" &&
    [ -n "$(sudo find "$state/builds" "$state/tmp" -mindepth 1 -maxdepth 1 -type d 2>/dev/null)" ]; then
    echo "error: red team: $what: build directories left in $state" >&2
    sudo find "$state/builds" "$state/tmp" -mindepth 1 -maxdepth 1 >&2
    return 1
  fi
}

# The red team's context: one `COPY`, no `RUN`, so a build that went
# wrong could only write where the root is.
rt_ctx() {
  local ctx="$W/rt-ctx"
  sudo test -f "$ctx/Dockerfile" && return 0
  sudo mkdir -p "$ctx" &&
    printf 'FROM scratch\nCOPY f /f\n' | sudo tee "$ctx/Dockerfile" >/dev/null &&
    echo rt | sudo tee "$ctx/f" >/dev/null
}

# Red team: an environment that says "container" (`container=docker`, read
# by the detection) on this VM, where root may mount, never gets kaniko
# mode on `/`: `--mode=kaniko` is refused (ADR-0054), and `--mode=auto`
# with a state directory overlayfs refuses keeps the refusal instead of
# routing to kaniko mode. A sentinel in `/` stays.
rt_container_env_on_a_host() {
  local ov="$W/rt-ov" ctx="$W/rt-ctx" log rc=0 bad=0
  rt_ctx || return 1
  sudo mkdir -p "$ov/l" "$ov/u" "$ov/w" "$ov/m"
  echo keep | sudo tee /rnk-rt-sentinel >/dev/null
  log="$(sudo env container=docker "$RNK" --mode=kaniko --context "$ctx" \
    --state-dir "$W/rt-kstate" --no-push 2>&1)" || rc=$?
  if [ "$rc" != 1 ] || ! grep -q 'where root may mount' <<<"$log"; then
    echo "error: red team: container=docker --mode=kaniko on the VM: exit $rc, want 1" >&2
    echo "$log" >&2
    bad=1
  fi
  sudo mount -t overlay overlay -o "lowerdir=$ov/l,upperdir=$ov/u,workdir=$ov/w" "$ov/m" || return 1
  trap 'sudo umount -l "'"$ov/m"'" 2>/dev/null || true' EXIT
  rc=0
  log="$(sudo env container=docker "$RNK" --context "$ctx" --state-dir "$ov/m/state" \
    --no-push 2>&1)" || rc=$?
  if [ "$rc" != 1 ] || ! grep -q 'overlay mode only (ADR-0054)' <<<"$log"; then
    echo "error: red team: container=docker --mode=auto, state on overlayfs: exit $rc, want 1" >&2
    echo "$log" >&2
    bad=1
  fi
  rt_nothing_left "$ov/m" "$ov/m/state" "container=docker auto" || bad=1
  if ! sudo umount "$ov/m"; then
    sudo umount -l "$ov/m" || true
    bad=1
  fi
  trap - EXIT
  if [ "$(cat /rnk-rt-sentinel 2>/dev/null)" != keep ]; then
    echo "error: red team: /rnk-rt-sentinel changed: kaniko mode touched /" >&2
    bad=1
  fi
  sudo rm -f /rnk-rt-sentinel
  [ "$bad" = 0 ] && echo "red team: container=docker on the VM never builds on / (exit 1 twice)"
  return "$bad"
}

# Red team: `--mode=auto` with `--ignore-path` where overlay mode runs is
# refused (exit 2) only after overlay mode's set-up: the store it opened is
# closed (no build directory, mount or process left), and the next build
# in that state directory works. Routed to kaniko mode (a state directory
# on overlayfs), the same flag is honoured.
rt_auto_ignore_path() {
  local ctx="$W/rt-ctx" st="$W/rt-state" ov="$W/rt-ov2" log rc=0 bad=0
  rt_ctx || return 1
  log="$(sudo "$RNK" --context "$ctx" --state-dir "$st" --no-push --ignore-path=/x 2>&1)" || rc=$?
  if [ "$rc" != 2 ] || ! grep -q 'kaniko mode only' <<<"$log"; then
    echo "error: red team: auto --ignore-path with overlay mode: exit $rc, want 2" >&2
    echo "$log" >&2
    bad=1
  fi
  rt_nothing_left "$st" "$st" "auto --ignore-path refused" || bad=1
  rc=0
  log="$(sudo "$RNK" --context "$ctx" --state-dir "$st" --no-push 2>&1)" || rc=$?
  if [ "$rc" != 0 ]; then
    echo "error: red team: the build after a refused one: exit $rc" >&2
    echo "$log" >&2
    bad=1
  fi
  rt_nothing_left "$st" "$st" "the build after a refused one" || bad=1
  sudo mkdir -p "$ov/l" "$ov/u" "$ov/w" "$ov/m"
  sudo mount -t overlay overlay -o "lowerdir=$ov/l,upperdir=$ov/u,workdir=$ov/w" "$ov/m" || return 1
  trap 'sudo umount -l "'"$ov/m"'" 2>/dev/null || true' EXIT
  rc=0
  log="$(sudo "$RNK" --context "$ctx" --state-dir "$ov/m/state" --no-push \
    --ignore-path=/x 2>&1)" || rc=$?
  if [ "$rc" != 0 ] || ! grep -q 'directory per stage, because overlay mode cannot run here' <<<"$log"; then
    echo "error: red team: auto --ignore-path routed to kaniko mode: exit $rc, want 0" >&2
    echo "$log" >&2
    bad=1
  fi
  rt_nothing_left "$ov/m" "$ov/m/state" "auto --ignore-path routed" || bad=1
  if ! sudo umount "$ov/m"; then
    sudo umount -l "$ov/m" || true
    bad=1
  fi
  trap - EXIT
  [ "$bad" = 0 ] && echo "red team: auto --ignore-path refused after the set-up with nothing left; honoured when routed"
  return "$bad"
}

# Red team: a state directory on ramfs (no extended attributes), where the
# kernel's overlay probe fails its own way: `--mode=auto` either refuses
# (exit 1) or builds in kaniko mode with a directory per stage (exit 0),
# never on `/`, leaving no mount or process. A refused set-up may leave its
# build directory to the next start (`rnk_store::Store`'s rule): the next
# build removes it, and the third finds only its own. Which way it goes is
# printed.
rt_auto_on_ramfs() {
  local ctx="$W/rt-ctx" fs="$W/rt-ramfs" log rc bad=0 i before="" now
  rt_ctx || return 1
  sudo mkdir -p "$fs"
  sudo mount -t ramfs -o mode=0700 ramfs "$fs" || return 1
  trap 'sudo umount -l "'"$fs"'" 2>/dev/null || true' EXIT
  for i in 1 2 3; do
    rc=0
    log="$(sudo "$RNK" --context "$ctx" --state-dir "$fs/state" --no-push 2>&1)" || rc=$?
    if grep -q 'kaniko mode on /' <<<"$log"; then
      echo "error: red team: ramfs state directory: kaniko mode on /" >&2
      bad=1
    fi
    case "$rc" in
      0) grep -q 'directory per stage' <<<"$log" || grep -q 'set up for overlay' <<<"$log" ||
        { echo "error: red team: ramfs build $i: built, but in which mode?" >&2; bad=1; } ;;
      1) ;;
      *) echo "error: red team: ramfs build $i: exit $rc" >&2; bad=1 ;;
    esac
    echo "red team: ramfs state directory, build $i: exit $rc: $(grep -Eo 'directory per stage|overlay mode cannot run here: [^;]{0,120}|set up for [a-z ]+' <<<"$log" | head -2 | tr '\n' ' ')"
    rt_nothing_left "$fs" "/nonexistent" "ramfs build $i" || bad=1
    now="$(sudo find "$fs/state/builds" "$fs/state/tmp" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort)"
    local d
    for d in $before; do
      if grep -qxF "$d" <<<"$now"; then
        echo "error: red team: ramfs build $i: an earlier build's $d is still there" >&2
        bad=1
      fi
    done
    if [ "$(grep -c . <<<"$now")" -gt 2 ]; then
      echo "error: red team: ramfs build $i: more than one build's directories:" >&2
      echo "$now" >&2
      bad=1
    fi
    before="$now"
  done
  if ! sudo umount "$fs"; then
    sudo umount -l "$fs" || true
    bad=1
  fi
  trap - EXIT
  return "$bad"
}

# Red team: a container signal (`container=docker`) on this VM, run as root
# without CAP_SYS_ADMIN (root that may not mount, so `--mode=auto` picks
# kaniko mode and believes it is on a container's `/`): the writable
# /proc/sys of a real host refuses it before anything is written; without
# the signal kaniko mode off `/` without mounts is refused (Q-330). Neither
# creates the state directory, and a sentinel in `/` stays.
rt_no_mount_root_with_a_container_signal() {
  local ctx="$W/rt-ctx" st="$W/rt-nomount" log rc bad=0 mode sig want
  rt_ctx || return 1
  echo keep | sudo tee /rnk-rt-sentinel >/dev/null
  for mode in --mode=auto --mode=kaniko; do
    for sig in docker none; do
      rc=0
      if [ "$sig" = docker ]; then
        want='writable /proc/sys'
        log="$(sudo env container=docker setpriv --bounding-set=-sys_admin -- "$RNK" "$mode" \
          --context "$ctx" --state-dir "$st" --no-push 2>&1)" || rc=$?
      else
        want='where root may not mount'
        log="$(sudo env -u container setpriv --bounding-set=-sys_admin -- "$RNK" "$mode" \
          --context "$ctx" --state-dir "$st" --no-push 2>&1)" || rc=$?
      fi
      if [ "$rc" != 1 ] || ! grep -q "$want" <<<"$log"; then
        echo "error: red team: no CAP_SYS_ADMIN, $mode, signal $sig: exit $rc, want 1 and \"$want\"" >&2
        echo "$log" >&2
        bad=1
      fi
      if sudo test -e "$st"; then
        echo "error: red team: no CAP_SYS_ADMIN, $mode, signal $sig: $st was created" >&2
        sudo rm -rf --one-file-system "$st"
        bad=1
      fi
    done
  done
  if [ "$(cat /rnk-rt-sentinel 2>/dev/null)" != keep ]; then
    echo "error: red team: /rnk-rt-sentinel changed: kaniko mode touched /" >&2
    bad=1
  fi
  sudo rm -f /rnk-rt-sentinel
  [ "$bad" = 0 ] && echo "red team: root without CAP_SYS_ADMIN and a container signal never builds on the VM's / (exit 1, nothing created)"
  return "$bad"
}

# Red team: in a `--privileged` container (root may mount) `--mode=auto`
# with the default state directory on the container's overlayfs root keeps
# the refusal (ADR-0054), also with /.dockerenv removed first (the overlay
# root still tells; pid 1 runs a script whose name holds no runtime's), and
# the container's `/` is left whole.
rt_auto_privileged_container() {
  local ctx="$W/rt-ctx" sh="$W/rt-sh" out rc bad=0 pre
  rt_ctx || return 1
  sudo mkdir -p "$sh"
  for pre in keep remove; do
    {
      echo 'set -u'
      [ "$pre" = remove ] && echo 'rm -f /.dockerenv; test ! -e /.dockerenv || exit 9'
      echo '/ranikust/bin/ranikust --context /ranikust/ctx --no-push; rc=$?'
      echo 'test -e /etc/alpine-release && test -x /bin/busybox && echo ROOT-INTACT'
      echo 'exit $rc'
    } | sudo tee "$sh/run.sh" >/dev/null
    rc=0
    out="$(sudo docker run --rm --privileged -v "$RNK:/ranikust/bin/ranikust:ro" \
      -v "$ctx:/ranikust/ctx:ro" -v "$sh:/rt:ro" --entrypoint sh "$BASE" /rt/run.sh 2>&1)" || rc=$?
    if [ "$rc" != 1 ] || ! grep -q 'overlay mode only (ADR-0054)' <<<"$out" ||
      ! grep -q ROOT-INTACT <<<"$out"; then
      echo "error: red team: --privileged auto (/.dockerenv: $pre): exit $rc, want 1 and the refusal" >&2
      echo "$out" >&2
      bad=1
    fi
  done
  [ "$bad" = 0 ] && echo "red team: --privileged container, auto: refused (ADR-0054), with or without /.dockerenv"
  return "$bad"
}

# Phase 2 acceptance criterion 5: a second kaniko-mode build on `/` in one
# default container, started while the first one runs, is refused by the
# flock on `/` (`lock_slash`) before it writes anything (its state
# directory, another one than the first's, is never made), and the first
# build ends as if nothing happened. FROM scratch: the RUN runs the
# container's own sleep on its `/`, as kaniko's RUNs do; nothing is pulled.
slash_lock_refuses_a_second_build() {
  local c="rnk-slash-lock-$$" rc=0
  slash_lock_in "$c" || rc=1
  # Removed on every path, with whatever still runs in it.
  sudo docker rm -f "$c" >/dev/null 2>&1 || true
  return "$rc"
}

slash_lock_in() {
  local c="$1" ctx="$W/slash-lock" rc=0 out i first=
  sudo mkdir -p "$ctx" &&
    printf 'FROM scratch\nRUN sleep 15\n' | sudo tee "$ctx/Dockerfile" >/dev/null || return 1
  sudo docker run -d --name "$c" -v "$RNK:/ranikust/bin/ranikust:ro" -v "$ctx:/ranikust/ctx:ro" \
    "$BASE" sleep 300 >/dev/null || return 1
  sudo docker exec -d "$c" sh -c '/ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx \
    --no-push >/ranikust/first.log 2>&1; echo $? >/ranikust/first.rc' || return 1
  # The first build holds the lock from its set-up to its end: wait until
  # its RUN runs.
  for i in $(seq 120); do
    sudo docker exec "$c" pgrep -f '^sleep 15$' >/dev/null 2>&1 && break
    if [ "$i" = 120 ]; then
      echo "error: slash lock: the first build's RUN never started" >&2
      sudo docker exec "$c" cat /ranikust/first.log >&2 || true
      return 1
    fi
    sleep 0.5
  done
  out="$(sudo docker exec "$c" /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx \
    --no-push --state-dir /var/lib/rnk-second 2>&1)" || rc=$?
  if [ "$rc" != 1 ] || ! grep -qF 'another ranikust build is using / in this container' <<<"$out"; then
    echo "error: slash lock: a second build on / during the first: exit $rc, want 1 and the lock's refusal" >&2
    echo "$out" >&2
    return 1
  fi
  if sudo docker exec "$c" test -e /var/lib/rnk-second; then
    echo "error: slash lock: the refused build made its state directory /var/lib/rnk-second" >&2
    return 1
  fi
  for i in $(seq 180); do
    first="$(sudo docker exec "$c" cat /ranikust/first.rc 2>/dev/null || true)"
    [ -n "$first" ] && break
    sleep 0.5
  done
  if [ "$first" != 0 ]; then
    echo "error: slash lock: the first build ended with '${first:-nothing}' after the refusal, want 0" >&2
    sudo docker exec "$c" cat /ranikust/first.log >&2 || true
    return 1
  fi
  echo "slash lock: a second kaniko-mode build on / refused by the flock (exit 1, nothing written); the first built (exit 0)"
}

# ADR-0103, ADR-0109: kaniko mode on a default container's `/`, where `RUN`
# runs `Direct`, refuses every `--seccomp` (exit 2) before anything is
# pulled, and a `RUN` there runs under the container's own profile: as many
# filters as a plain `docker run`. Run with the kaniko-mode suite, unless
# the cases named leave seccomp out.
seccomp_on_slash() {
  local ctx="$W/seccomp-slash" out v rc own got
  local in=(-v "$RNK:/ranikust/bin/ranikust:ro" -v "$ctx:/ranikust/ctx:ro")
  sudo mkdir -p "$ctx" || return 1
  printf 'FROM %s\nRUN echo "filters=$(sed -n "s/^Seccomp_filters:[[:space:]]*//p" /proc/self/status)"\n' \
    "$BASE" | sudo tee "$ctx/Dockerfile" >/dev/null || return 1
  echo '{"defaultAction": "SCMP_ACT_ALLOW"}' | sudo tee "$ctx/profile.json" >/dev/null || return 1
  for v in default unconfined /ranikust/ctx/profile.json; do
    rc=0
    out="$(sudo docker run --rm "${in[@]}" "$BASE" /ranikust/bin/ranikust --mode=kaniko \
      --context /ranikust/ctx --no-push "--seccomp=$v" 2>&1)" || rc=$?
    if [ "$rc" != 2 ] || ! grep -qF 'kaniko mode on / runs RUN directly' <<<"$out"; then
      echo "error: --seccomp=$v in kaniko mode on /: exit $rc, want 2 and the refusal" >&2
      echo "$out" >&2
      return 1
    fi
  done
  own="$(sudo docker run --rm "$BASE" sed -n 's/^Seccomp_filters:[[:space:]]*//p' /proc/self/status)"
  rc=0
  out="$(sudo docker run --rm "${in[@]}" "$BASE" /ranikust/bin/ranikust --mode=kaniko \
    --context /ranikust/ctx --no-push 2>&1)" || rc=$?
  got="$(sed -n 's/^filters=\([0-9][0-9]*\)$/\1/p' <<<"$out" | head -1)"
  if [ "$rc" != 0 ] || [ -z "$own" ] || [ "$got" != "$own" ]; then
    echo "error: kaniko mode on /: a RUN under $got seccomp filters, a plain container $own (exit $rc)" >&2
    echo "$out" >&2
    return 1
  fi
  echo "seccomp on /: every --seccomp refused (exit 2); a RUN under the container's own profile ($own filters)"
}

# A plain command: under `||` errexit would be off inside the cases.
set +e
# shellcheck disable=SC2086
smoke_cases ${RANIKUST_QUICK_CASES:-}
rc=$?
if [ "${RANIKUST_QUICK_MODE:-}" = kaniko ] &&
  [[ -z "${RANIKUST_QUICK_CASES:-}" || " ${RANIKUST_QUICK_CASES} " == *" kaniko-flags "* ]]; then
  adr0054_refusals || rc=1
  auto_routes_to_stages || rc=1
  rt_container_env_on_a_host || rc=1
  rt_auto_ignore_path || rc=1
  rt_auto_on_ramfs || rc=1
  rt_no_mount_root_with_a_container_signal || rc=1
  rt_auto_privileged_container || rc=1
  slash_lock_refuses_a_second_build || rc=1
fi
if [ "${RANIKUST_QUICK_MODE:-}" = kaniko ] &&
  [[ -z "${RANIKUST_QUICK_CASES:-}" || " ${RANIKUST_QUICK_CASES} " == *" seccomp "* ]]; then
  seccomp_on_slash || rc=1
fi
set -e

# No image at all when only cases whose builds must fail ran (self-exe).
sudo docker images --format '{{.Repository}}:{{.Tag}}' | { grep '^localhost/rnk-case:' || true; } |
  xargs -r sudo docker rmi -f >/dev/null
! grep -qF " $W" /proc/self/mountinfo || { echo "error: mounts left under $W" >&2; exit 1; }
! pgrep -x ranikust >/dev/null || { echo "error: ranikust processes left" >&2; exit 1; }
sudo rm -rf --one-file-system "$W"
[ "$rc" = 0 ] && echo "quick: ok"
exit "$rc"
