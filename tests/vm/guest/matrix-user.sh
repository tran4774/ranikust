#!/usr/bin/env bash
# Runs as the runner's own user on a GitHub-hosted runner (tests/vm/runner.sh,
# ADR-0090): rows 6 and 6b of ADR-0100's privilege matrix, a regular user on
# Ubuntu 24.04, where kernel.apparmor_restrict_unprivileged_userns is 1, with
# uidmap and subordinate ranges. The environment is checked against the rows
# first. Each leg builds with --mode=auto as this user and asserts the mode
# or the refusal with its step and errno:
#   6, the user's own copy of ranikust: refused at the mount namespace
#      (EPERM), before anything is written, with sudo, the AppArmor profile
#      after an install as /usr/local/bin/ranikust, then a container;
#   6, root's copy at /usr/local/bin/ranikust: refused the same way, with
#      the profile as shipped (contrib/apparmor/ranikust);
#   6b, that copy with the profile loaded: rootless overlay mode with a new
#      proc, the probe passing through newuidmap; its RUN sees its own PID
#      namespace alone and sets an owner of the user's ranges, which the
#      image keeps.
# What the rows do not decide (sysfs, mknod, the overlay API) is printed as
# `measured:`. Where /usr/local/bin is not root's alone (GitHub's image makes
# it 0777), it is made root's 0755, as Ubuntu ships it, for the legs; the
# teardown restores it and removes the profile and root's copy.
#
# Writes out/matrix-user/: env.txt and each leg's log.
set -euo pipefail
cd "$HOME/rnk"

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
BASE="$REG/library/alpine:3.20"
RNK="$HOME/rnk/bin/ranikust"
CTX="$HOME/rnk/guest/cases/matrix-podman"
OUT="$HOME/rnk/out/matrix-user"
# The legs' state directories and outputs, this user's alone.
U="$HOME/rnk/matrix-user"
LOCAL_BIN=/usr/local/bin
AA_BINARY="$LOCAL_BIN/ranikust"
AA_PROFILE=/etc/apparmor.d/ranikust
SWITCH=/proc/sys/kernel/apparmor_restrict_unprivileged_userns

die() { echo "error: $*" >&2; exit 1; }
FAILED=()
fail() { echo "FAIL: $*" >&2; FAILED+=("$*"); }

# --- The environment of rows 6 and 6b -------------------------------------------

# shellcheck disable=SC1091
. /etc/os-release
[ "${ID:-}" = ubuntu ] && [ "${VERSION_ID:-}" = 24.04 ] || die "not Ubuntu 24.04: ${PRETTY_NAME:-?}"
[ "$(cat "$SWITCH" 2>/dev/null)" = 1 ] || die "kernel.apparmor_restrict_unprivileged_userns is not 1"
[ "$(id -u)" != 0 ] || die "run as a regular user"
me="$(id -un)"
label="$(tr -d '\000' </proc/self/attr/current)"
# ranikust offers the profile only to an unconfined process in the host's
# user namespace.
[ "$label" = unconfined ] || die "this shell's AppArmor label is '$label', not unconfined"
[ "$(awk '{ printf "%s %s %s;", $1, $2, $3 }' /proc/self/uid_map)" = "0 0 4294967295;" ] ||
  die "not the host's user namespace"
subuid="$(grep "^$me:" /etc/subuid || true)" subgid="$(grep "^$me:" /etc/subgid || true)"
[ -n "$subuid" ] && [ -n "$subgid" ] || die "no subordinate ranges for $me in /etc/subuid and /etc/subgid"
newuidmap="$(command -v newuidmap)" || die "no newuidmap: install uidmap"
[ ! -e "$AA_BINARY" ] && [ ! -e "$AA_PROFILE" ] || die "$AA_BINARY or $AA_PROFILE is there already"
! sudo grep -q '^ranikust ' /sys/kernel/security/apparmor/profiles || die "a ranikust profile is loaded already"
mkdir -p "$OUT"
bin_was="$(stat -c '%u:%g %a' "$LOCAL_BIN")"
{
  echo "os: $PRETTY_NAME; kernel $(uname -r)"
  echo "user: $me, AppArmor label $label, umask $(umask); subuid $subuid, subgid $subgid"
  echo "newuidmap: $(stat -c '%A %U' "$newuidmap") $(sudo getcap "$newuidmap" 2>/dev/null | cut -d' ' -f2-)"
  echo "kernel.apparmor_restrict_unprivileged_userns: $(cat "$SWITCH")"
  echo "kernel.apparmor_restrict_unprivileged_unconfined: $(cat /proc/sys/kernel/apparmor_restrict_unprivileged_unconfined 2>/dev/null || echo absent)"
  echo "$LOCAL_BIN before: uid:gid mode $bin_was"
} | tee "$OUT/env.txt"

teardown() {
  set +e
  if [ -e "$AA_PROFILE" ]; then
    sudo apparmor_parser -R "$AA_PROFILE" >/dev/null 2>&1
    sudo rm -f "$AA_PROFILE"
  fi
  sudo rm -f "$AA_BINARY"
  sudo chown "${bin_was%% *}" "$LOCAL_BIN"
  sudo chmod "${bin_was##* }" "$LOCAL_BIN"
}
trap teardown EXIT

# Ubuntu's /usr/local/bin is root's 0755: the profile the refusal offers
# hangs on it.
sudo chown root:root "$LOCAL_BIN" && sudo chmod 0755 "$LOCAL_BIN" || die "cannot make $LOCAL_BIN root's"
d="$LOCAL_BIN"
while :; do
  [ ! -L "$d" ] && [ "$(stat -c %u "$d")" = 0 ] && [ $((8#$(stat -c %a "$d") & 8#022)) = 0 ] ||
    die "$d is not root's alone: $(stat -c '%U %a' "$d")"
  [ "$d" = / ] && break
  d="$(dirname "$d")"
done

! grep -qF " $U" /proc/self/mountinfo || die "mounts left under $U by an earlier run"
sudo rm -rf --one-file-system "$U" && mkdir -m 700 "$U"

# --- The legs ----------------------------------------------------------------------

# leg <name> <binary> <ranikust arguments...>: runs the build as this user,
# keeps its log in out/matrix-user/<name>.log and prints it; sets LOG, RC and
# LINE, the user namespace probe's line (empty unless the log has exactly
# one).
leg() {
  local name=$1 bin=$2 n
  shift 2
  RC=0
  LOG="$("$bin" "$@" 2>&1)" || RC=$?
  printf '%s\n' "$LOG" >"$OUT/$name.log"
  echo "--- $name: exit $RC"
  printf '%s\n' "$LOG"
  n="$(grep -Ec 'user namespace probe( \(reported only\))?: ' <<<"$LOG" || true)"
  LINE=
  if [ "$n" = 1 ]; then
    LINE="$(grep -Eo 'user namespace probe( \(reported only\))?: .*' <<<"$LOG")"
  else
    fail "$name: $n user namespace probe lines, want 1"
  fi
}

# Whether each argument after the first comes in $1, after the one before.
in_order() {
  local rest="$1" part
  shift
  for part in "$@"; do
    [[ "$rest" == *"$part"* ]] || return 1
    rest="${rest#*"$part"}"
  done
}

# The advice line of a refusal: ranikust indents it.
advice() { grep '^  ' <<<"$LOG" || true; }

args=(--context "$CTX" --build-arg "BASE=$BASE" --no-push)
REFUSED_LINE="user namespace probe: stops at mount namespace, nothing written; unshare(CLONE_NEWUSER): ok; unshare(CLONE_NEWNS): EPERM; kernel.apparmor_restrict_unprivileged_userns: 1"
REFUSED="rootless overlay mode cannot run here: the user namespace probe stops at mount namespace"
WHY="where kernel.apparmor_restrict_unprivileged_userns is 1, as here, only a program an AppArmor profile allows gets a usable one: "
PROFILE="ranikust's AppArmor profile (contrib/apparmor/ranikust in its repository; the book's \"Choosing where to run it\" shows it)"

echo "== 6: no AppArmor profile, the user's own copy of ranikust"
leg 6-own "$RNK" "${args[@]}" --state-dir "$U/state-6-own"
[ "$RC" = 1 ] || fail "6, own copy: exit $RC, want 1"
[ "$LINE" = "$REFUSED_LINE" ] || fail "6, own copy: the probe line '$LINE', want '$REFUSED_LINE'"
grep -qF "$REFUSED" <<<"$LOG" || fail "6, own copy: no '$REFUSED'"
in_order "$(advice)" "$WHY" 'run ranikust as root, with sudo; ' \
  "or, as root, install ranikust as $AA_BINARY first, " \
  "then run ranikust from $AA_BINARY: $(realpath "$RNK") is not a path only root can change (" \
  '; or run ranikust in a container' ||
  fail "6, own copy: want the advice sudo, the profile after an install as $AA_BINARY, a container"
[ ! -e "$U/state-6-own" ] || fail "6, own copy: the state directory was made"

echo "== 6: no AppArmor profile, root's copy at $AA_BINARY"
sudo install -m 0755 -o root -g root "$RNK" "$AA_BINARY"
leg 6-root "$AA_BINARY" "${args[@]}" --state-dir "$U/state-6-root"
[ "$RC" = 1 ] || fail "6, root's copy: exit $RC, want 1"
[ "$LINE" = "$REFUSED_LINE" ] || fail "6, root's copy: the probe line '$LINE', want '$REFUSED_LINE'"
grep -qF "$REFUSED" <<<"$LOG" || fail "6, root's copy: no '$REFUSED'"
in_order "$(advice)" "$WHY" 'run ranikust as root, with sudo; ' \
  "or, as root, copy $PROFILE to $AA_PROFILE and run apparmor_parser -r $AA_PROFILE; " \
  'or run ranikust in a container' && ! grep -qF ' first' <<<"$(advice)" ||
  fail "6, root's copy: want the advice sudo, the profile as shipped, a container"
[ ! -e "$U/state-6-root" ] || fail "6, root's copy: the state directory was made"

echo "== 6b: root's copy at $AA_BINARY with ranikust's AppArmor profile"
sudo cp guest/apparmor-ranikust "$AA_PROFILE"
sudo apparmor_parser -r "$AA_PROFILE"
leg 6b "$AA_BINARY" "${args[@]}" --state-dir "$U/state-6b" --destination localhost/rnk-matrix:6b \
  --tar-path "$U/6b.tar"
[ "$RC" = 0 ] || fail "6b: exit $RC, want 0"
echo "measured: 6b: $LINE"
[[ "$LINE" == "user namespace probe: passes; unshare(CLONE_NEWUSER): ok; map: newuidmap; chown: ok; unshare(CLONE_NEWNS): ok; proc: new instance; sysfs: "* &&
  "$LINE" == *"; overlay: ok, "* && "$LINE" == *"; kernel.apparmor_restrict_unprivileged_userns: 1" ]] ||
  fail "6b: the probe should pass through newuidmap with a new proc: '$LINE'"
grep -qF "ranikust: overlay mode, rootless, in the build's user namespace, RUN enters its root with pivot_root, proc: a new instance, " <<<"$LOG" ||
  fail "6b: not rootless overlay mode with a new proc"
# A new proc shows the RUN's own PID namespace alone, under ranikust's init.
seen="$(sed -n 's/^proc: //p' <<<"$LOG" | head -1)"
[ "$seen" = "pid 1 ranikust-init, NSpid levels 1" ] || fail "6b: the RUN's proc is not a new one: '$seen'"
if [ "$RC" = 0 ]; then
  got="the image did not load"
  if img="$(sudo docker load -i "$U/6b.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)" && [ -n "$img" ]; then
    got="$(sudo docker run --rm "$img" stat -c '%u:%g' /d)" || got="the image did not run"
    sudo docker rmi -f "$img" >/dev/null || true
  fi
  [ "$got" = 1234:1234 ] || fail "6b: the owner the RUN set is '$got', want 1234:1234"
fi

echo "== after the legs"
! grep -qF " $U" /proc/self/mountinfo || fail "mounts left under $U"
# A sandbox process carries its spawner thread's name until it execs or
# renames itself (rnk-sandbox-spawner, cut to 15 bytes).
for p in ranikust ranikust-init rnk-sandbox-spa; do
  rc=0
  pgrep -x "$p" >/dev/null || rc=$?
  [ "$rc" = 1 ] || fail "$p processes left, or pgrep failed ($rc)"
done

if [ "${#FAILED[@]}" -gt 0 ]; then
  echo "matrix-user: ${#FAILED[@]} failure(s):"
  printf '  %s\n' "${FAILED[@]}"
  exit 1
fi
echo "matrix-user: rows 6 and 6b as expected"
