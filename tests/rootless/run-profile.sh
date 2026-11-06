#!/usr/bin/env bash
# Runs inside a T1 container (tests/rootless/lane.sh): the control checks of
# one profile, then every `#[ignore = "T1: <profile>: ..."]` test of the
# static binaries in /t1/bin, as root of the container, each exactly once. A
# failed control check fails the run: a test must never pass because the
# container lacks what it exercises, or has more than the profile allows.
set -euo pipefail
prof="$1"
cd /work
export TMPDIR=/work

fail() { echo "error: control check ($prof): $*" >&2; exit 3; }
check() { local what="$1"; shift; "$@" >/dev/null 2>&1 || fail "$what"; echo "control: $what"; }

# Every profile: root here, a user outside, with a range of ids.
[ "$(id -u)" = 0 ] || fail "not root"
outside="$(awk '$1 == 0 { print $2 }' /proc/self/uid_map)"
[ -n "$outside" ] && [ "$outside" != 0 ] || fail "root here is real root ($(paste -sd';' /proc/self/uid_map))"
echo "control: root here is uid $outside outside"
[ "$(wc -l </proc/self/uid_map)" -ge 2 ] && [ "$(wc -l </proc/self/gid_map)" -ge 2 ] ||
  fail "one id mapped: the user needs subordinate ids (/etc/subuid, /etc/subgid)"
echo "control: a range of ids mapped"
check "chown to another user" sh -c 'touch /work/.o && chown 1234:1234 /work/.o && rm /work/.o'

eff="$(sed -n 's/^CapEff:[[:space:]]*//p' /proc/self/status)"
case "$prof" in
  default)
    # Rootless Podman as it starts a container: no CAP_SYS_ADMIN, a seccomp
    # filter, and so no mount namespace without a user namespace.
    [ $((0x$eff >> 21 & 1)) = 0 ] || fail "CAP_SYS_ADMIN in $eff"
    echo "control: no CAP_SYS_ADMIN"
    [ "$(sed -n 's/^Seccomp:[[:space:]]*//p' /proc/self/status)" = 2 ] || fail "no seccomp filter"
    echo "control: a seccomp filter"
    if unshare -m true 2>/dev/null; then fail "a mount namespace could be made"; fi
    echo "control: no mount namespace"
    ;;
  privileged)
    check "tmpfs mount" sh -c 'mkdir /work/.t && mount -t tmpfs none /work/.t && umount /work/.t && rmdir /work/.t'
    # The root the mount tests take: a bind mount and a tmpfs inside it,
    # and its marker; gone with the container.
    export RNK_T1_ROOT=/work/t1-root
    mkdir -p /work/t1-src "$RNK_T1_ROOT/bind" "$RNK_T1_ROOT/tmpfs"
    touch "$RNK_T1_ROOT/.rnk-t1-root"
    mount --bind /work/t1-src "$RNK_T1_ROOT/bind"
    mount -t tmpfs none "$RNK_T1_ROOT/tmpfs"
    echo "control: RNK_T1_ROOT with a bind mount and a tmpfs"
    ;;
  *) fail "unknown profile" ;;
esac

mapfile -t names <"/t1/$prof.txt"
[ "${#names[@]}" -gt 0 ] || { echo "T1 $prof tests: none listed"; exit 0; }
declare -A want=()
for n in "${names[@]}"; do want[$n]=1; done
ran=0
failed=0
# Every binary runs, even after one fails, so one run shows every failure.
for bin in /t1/bin/*; do
  # Exact names only, as tests/container/run-profile.sh: a test whose name
  # merely contains a listed one never runs here.
  sel=()
  while IFS= read -r t; do
    [ -n "${want[${t##*::}]:-}" ] && sel+=("$t")
  done < <("$bin" --list --ignored 2>/dev/null | sed -n 's/: test$//p')
  [ "${#sel[@]}" -gt 0 ] || continue
  rc=0
  out="$("$bin" --ignored --exact --test-threads=1 "${sel[@]}" 2>&1)" || rc=$?
  n="$(sed -nE 's/^test result: [a-zA-Z]+\. ([0-9]+) passed.*/\1/p' <<<"$out" | awk '{ s += $1 } END { print s + 0 }')"
  if [ "$rc" != 0 ]; then
    echo "$out"
    echo "error: $(basename "$bin") failed" >&2
    failed=1
  elif [ "$n" -gt 0 ]; then
    echo "$out" | grep -E '^test .* \.\.\. |^measured: |^ok$'
  fi
  ran=$((ran + n))
done
echo "T1 $prof tests: $ran passed of ${#names[@]} listed"
[ "$failed" = 0 ] || exit 1
[ "$ran" = "${#names[@]}" ] || { echo "error: expected ${#names[@]} T1 $prof tests to run" >&2; exit 1; }
