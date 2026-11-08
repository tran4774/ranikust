#!/usr/bin/env bash
# Runs in the T2 VM on AlmaLinux 9 (RANIKUST_VM_RELEASE=alma9): row 9e of
# ADR-0100's privilege matrix, kernel 5.14 with SELinux enforcing. Two legs
# build with --mode=auto in their default state directories, each asserting
# the mode it gets and the user namespace probe's line, every step with its
# result or errno:
#   root: overlay mode as root, the probe (reported only) passing;
#   rnk, a regular user with subordinate ranges: rootless overlay mode with a
#     new proc, the probe passing through newuidmap.
# Each builds one RUN per in-sandbox check of `just smoke`, the rootless
# case's `content` stage (one --reproducible digest for both legs: no
# SELinux label reaches a layer) and tests/smoke/e2e; Podman runs what they
# built. The checks build's last RUN must keep the SELinux context of the
# process that started ranikust. SELinux denials (AVC) logged while they build are printed, and
# none may be; Podman's bind of a home directory, which SELinux denies, is
# the control that the audit log shows them.
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
source guest/smoke-lib.bash

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
BASE="$REG/library/alpine:3.20"
RNK="$HOME/rnk/bin/ranikust"
OUT="$HOME/rnk/out/el9"
# Each leg's outputs, private to it; the state directories are the defaults.
W=/var/tmp/rnk-el9 U="$HOME/rnk-el9"
ROOT_STATE=/var/lib/ranikust USER_STATE="$HOME/.local/share/ranikust"
AUDIT=/var/log/audit/audit.log
# What each leg must log, measured on AlmaLinux 9.8 (kernel 5.14.0-687): its
# kernel takes overlay's lowerdir+ (a backport), refuses mknod and a sysfs of
# the namespace's own with EPERM, and has no AppArmor switch.
PROBE_STEPS="unshare(CLONE_NEWNS): ok; proc: new instance; sysfs: rbind of /sys (mount sysfs: EPERM); mknod: EPERM; overlay: ok, fsconfig(lowerdir+); kernel.apparmor_restrict_unprivileged_userns: absent"
ROOT_PROBE="user namespace probe (reported only): passes; unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok; $PROBE_STEPS"
ROOT_SETUP="set up for overlay mode as root, RUN enters its root with pivot_root, overlay mounts with fsconfig(lowerdir+)"
USER_PROBE="user namespace probe: passes; unshare(CLONE_NEWUSER): ok; map: newuidmap; chown: ok; $PROBE_STEPS"
USER_SETUP="ranikust: overlay mode, rootless, in the build's user namespace, RUN enters its root with pivot_root, proc: a new instance, sysfs: a read-only rbind (for a RUN with --network=none, a new instance where it mounts, else the rbind with no network device), overlay mounts with fsconfig(lowerdir+)"

fail() { echo "error: $*" >&2; exit 1; }

# --- The environment of row 9e -------------------------------------------------

grep -Eq '^ID_LIKE=.*\brhel\b' /etc/os-release || fail "not the RHEL family: $(grep '^ID_LIKE=' /etc/os-release)"
[ "$(getenforce)" = Enforcing ] || fail "SELinux is $(getenforce), not enforcing"
# Without it, a denial would go unseen and the check below prove nothing.
systemctl is-active -q auditd && sudo test -f "$AUDIT" || fail "auditd does not run: SELinux's denials would go unseen"
me="$(id -un)"
subuid="$(grep "^$me:" /etc/subuid || true)" subgid="$(grep "^$me:" /etc/subgid || true)"
[ -n "$subuid" ] && [ -n "$subgid" ] || fail "no subordinate ranges for $me in /etc/subuid and /etc/subgid"
echo "os: $(. /etc/os-release && echo "$PRETTY_NAME"); kernel $(uname -r)"
echo "selinux: $(sestatus | awk -F': *' '/^Current mode/ { m = $2 } /^Loaded policy name/ { p = $2 } END { print m ", policy " p }')"
echo "user: $me, $(id -Z), umask $(umask); subuid $subuid, subgid $subgid"
echo "newuidmap: $(stat -c '%A %U' "$(command -v newuidmap)"), $(sudo getcap "$(command -v newuidmap)" 2>/dev/null | cut -d' ' -f2-)"

! grep -qF " $W" /proc/self/mountinfo || fail "mounts left under $W by an earlier run"
sudo rm -rf --one-file-system "$W" && sudo mkdir -m 700 "$W"
rm -rf "$U" && mkdir -m 700 "$U"
mkdir -p "$OUT"
printf %s "$SMOKE_E2E_SECRET" | sudo tee "$W/token" >/dev/null
printf %s "$SMOKE_E2E_SECRET" >"$U/token"
chmod 600 "$U/token"

# The in-sandbox checks of `just smoke`, one RUN each so that a failure
# names it, and the RUN's own SELinux context, printed.
CHECKS="$U/checks"
mkdir -p "$CHECKS"
{
  printf 'FROM %s\n' "$BASE"
  for c in "${smoke_checks[@]}"; do printf 'RUN %s\n' "$c"; done
  cat <<'EOF'
RUN echo "RUN selinux context: $(tr -d '\000' </proc/self/attr/current)"
EOF
} >"$CHECKS/Dockerfile"

# avc_since <epoch>: the SELinux records of the audit log from then on, the
# logs auditd rotated in the meantime included.
avc_since() {
  sudo find "${AUDIT%/*}" -maxdepth 1 -type f -name "${AUDIT##*/}*" -exec awk -v t0="$1" \
    '/^type=(AVC|USER_AVC|SELINUX_ERR) / && match($0, /msg=audit\([0-9.]+/) {
      if (substr($0, RSTART + 10, RLENGTH - 10) + 0 >= t0 + 0) print
    }' {} +
}
# run_context <leg>: the SELinux context the checks' RUN printed.
run_context() { sed -n 's/^RUN selinux context: //p' "$OUT/$1-checks.log"; }

# --- The legs --------------------------------------------------------------------

# leg_build <leg> <name> <ranikust arguments...>: builds as root (sudo) or
# as this user, keeps the log in out/el9/<leg>-<name>.log, prints the
# digest; fails unless the build passed in the leg's mode with the user
# namespace probe's line expected there.
leg_build() {
  local leg=$1 name=$2 log rc=0 line probe setup
  shift 2
  if [ "$leg" = root ]; then
    log="$(sudo "$RNK" "$@" 2>&1)" || rc=$?
  else
    log="$("$RNK" "$@" 2>&1)" || rc=$?
  fi
  printf '%s\n' "$log" >"$OUT/$leg-$name.log"
  line="$(grep -Eo 'user namespace probe( \(reported only\))?: .*' <<<"$log" || true)"
  echo "$leg: $name: ${line:-no user namespace probe line}" >&2
  grep -Eo 'set up for .*|^RUN selinux context: .*' <<<"$log" | sed "s/^/$leg: $name: /" >&2 || true
  [ "$rc" = 0 ] || { echo "$log" >&2; fail "$leg: $name: the build failed ($rc)"; }
  [ "$(grep -c . <<<"$line")" = 1 ] || { echo "$log" >&2; fail "$leg: $name: want one user namespace probe line"; }
  case "$leg" in
    root) probe=$ROOT_PROBE setup=$ROOT_SETUP ;;
    user) probe=$USER_PROBE setup=$USER_SETUP ;;
  esac
  [ "$line" = "$probe" ] || { echo "$log" >&2; fail "$leg: $name: want the probe line: $probe"; }
  grep -qF "$setup" <<<"$log" || { echo "$log" >&2; fail "$leg: $name: want the mode: $setup"; }
  smoke_digest "$log" || { echo "$log" >&2; fail "$leg: $name: no digest in the log"; }
}

content_args=(--context "$HOME/rnk/guest/cases/rootless" --no-push --target content --reproducible
  --build-arg "BASE=$BASE" --destination localhost/rnk-el9-content:latest)

t0="$(date +%s.%N)"
echo "=== root: overlay mode as root expected"
leg_build root checks --context "$CHECKS" --no-push >/dev/null
# ranikust sets no SELinux context: the RUN keeps its caller's.
[ "$(run_context root)" = "$(sudo id -Z)" ] ||
  fail "root: the RUN's SELinux context $(run_context root), not its caller's $(sudo id -Z)"
root_digest="$(leg_build root content "${content_args[@]}" --tar-path "$W/content.tar")"
sudo mkdir -m 700 "$W/e2e"
smoke_e2e_args "$HOME/rnk/guest/smoke-e2e" "$ROOT_STATE" "$W/e2e" "$W/token"
leg_build root e2e "${e2e_args[@]}" --build-arg "BASE=$BASE" >/dev/null
sudo bash -c "$(declare -f smoke_check_no_stub_leak smoke_check_no_secret smoke_check_no_empty_layer); \
  smoke_check_no_stub_leak '$W/e2e/oci' && smoke_check_no_secret '$W/e2e/oci' '$SMOKE_E2E_SECRET' && \
  smoke_check_no_empty_layer '$W/e2e/oci'"
echo "root: ok"

echo "=== user $me: rootless overlay mode expected"
leg_build user checks --context "$CHECKS" --no-push >/dev/null
[ "$(run_context user)" = "$(id -Z)" ] ||
  fail "user: the RUN's SELinux context $(run_context user), not its caller's $(id -Z)"
user_digest="$(leg_build user content "${content_args[@]}" --tar-path "$U/content.tar")"
[ "$user_digest" = "$root_digest" ] ||
  fail "user: the content stage's --reproducible digest $user_digest, root's $root_digest"
echo "user: the content stage's --reproducible digest is root's, $user_digest"
mkdir -m 700 "$U/e2e"
smoke_e2e_args "$HOME/rnk/guest/smoke-e2e" "$USER_STATE" "$U/e2e" "$U/token"
leg_build user e2e "${e2e_args[@]}" --build-arg "BASE=$BASE" >/dev/null
smoke_check_no_stub_leak "$U/e2e/oci"
smoke_check_no_secret "$U/e2e/oci" "$SMOKE_E2E_SECRET"
smoke_check_no_empty_layer "$U/e2e/oci"
echo "user: ok"

smoke_check_no_secret_in "$SMOKE_E2E_SECRET" "$OUT/root-e2e.log" "$OUT/user-e2e.log"

# The types the policy gives the default state directories, where the
# overlays' upper and work directories are.
echo "=== state directories"
sudo ls -Zd "$ROOT_STATE" "$USER_STATE"
[[ "$(sudo stat -c %C "$ROOT_STATE")" == *:var_lib_t:* ]] || fail "$ROOT_STATE is not var_lib_t"
[[ "$(stat -c %C "$USER_STATE")" == *:data_home_t:* ]] || fail "$USER_STATE is not data_home_t"

# The audit log's records reach the file a little after the event.
sleep 2
echo "=== SELinux denials while the legs built"
avcs="$(avc_since "$t0")"
if [ -n "$avcs" ]; then
  echo "$avcs"
  fail "SELinux denied something while the legs built ($(grep -c . <<<"$avcs") records)"
fi
echo "selinux: no denial logged while the legs built"

# --- What they built, run by Podman --------------------------------------------

loaded() { sed -nE 's/^Loaded image(\(s\))?( ID)?: //p' | tail -1; }
img="$(sudo podman load -i "$U/content.tar" | loaded)"
got="$(sudo podman run --rm "$img" sh -c 'stat -c "%u:%g %a" /home/u/f /d && readlink /home/u/l')"
[ "$got" = "$(printf '1234:1234 644\n0:0 2750\nf')" ] ||
  fail "the content stage's ownership, setgid directory or symlink: $got"
sudo podman rmi -f "$img" >/dev/null
for leg in "$W" "$U"; do
  img="$(sudo podman load -i "$leg/e2e/image.tar" | loaded)"
  [ "$(sudo podman run --rm "$img")" = "$SMOKE_E2E_OUTPUT" ] || fail "$leg: the e2e image does not print what it should"
  [ "$(sudo podman run --rm "$img" id -un)" = "$SMOKE_E2E_USER" ] || fail "$leg: the e2e image does not run as $SMOKE_E2E_USER"
  sudo podman rmi -f "$img" >/dev/null
done
echo "podman: the images run as built"

# --- The control ------------------------------------------------------------------

# A container (container_t) may not read a home directory's file
# (user_home_t) bound without relabelling: SELinux denies it, and the
# audit log must show the denial.
echo "=== control: Podman binds a home directory without relabelling it"
mkdir -p "$U/control" && echo control >"$U/control/f"
t1="$(date +%s.%N)"
rc=0
ctl="$(sudo podman run --rm -v "$U/control:/c:ro" "$BASE" cat /c/f 2>&1)" || rc=$?
[ "$rc" != 0 ] && grep -q 'Permission denied' <<<"$ctl" ||
  fail "control: want cat refused with EACCES in the container, got $rc: $ctl"
for _ in $(seq 10); do
  ctl="$(avc_since "$t1" | grep 'comm="cat"' | grep 'user_home_t' || true)"
  [ -z "$ctl" ] || break
  sleep 1
done
[ -n "$ctl" ] || fail "control: the denial is not in $AUDIT: the check above would not see one"
printf '%s\n' "${ctl%%$'\n'*}"
echo "control: the audit log shows SELinux's denial"

echo "=== the VM after the legs"
! grep -qE " ($W|$ROOT_STATE|$USER_STATE)" /proc/self/mountinfo || fail "mounts left by a build"
# The sandbox's processes carry its spawner thread's name until they exec or
# rename themselves.
! pgrep -x 'ranikust|ranikust-init|rnk-sandbox-spa' >/dev/null || fail "ranikust processes left"
sudo rm -rf --one-file-system "$W"
rm -rf "$U"
echo "el9: ok"
