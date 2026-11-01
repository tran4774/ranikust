#!/usr/bin/env bash
# Runs inside a throwaway T2c container (ADR-0063): the control checks of
# one profile, then every `#[ignore = "T2c: <profile>: ..."]` test of the
# static binaries in /t2c/bin, as root, each exactly once. A failed control
# check fails the run: a test must never pass because the container lacks
# what it exercises (or has more than the profile allows).
set -euo pipefail
prof="$1"
cd /work
export TMPDIR=/work

fail() { echo "error: control check ($prof): $*" >&2; exit 3; }
check() { local what="$1"; shift; "$@" >/dev/null 2>&1 || fail "$what"; echo "control: $what"; }

# What every profile needs: root, and a TMPDIR that keeps device nodes
# usable (Docker's --tmpfs is nodev by default; /dev/zero's node, 1:5, is
# one the device cgroup lets a container open).
[ "$(id -u)" = 0 ] || fail "not root"
check "device node in TMPDIR opens (1:5)" sh -c 'mknod /work/.c c 1 5 && head -c1 /work/.c && rm /work/.c'
check "block node made (8:0)" sh -c 'mknod /work/.b b 8 0 && rm /work/.b'
check "chown to another user" sh -c 'touch /work/.o && chown 1234:1234 /work/.o && rm /work/.o'
check "setgroups" setpriv --reuid=1000 --regid=1000 --groups=4,5 true

bnd="$(sed -n 's/^CapBnd:[[:space:]]*//p' /proc/self/status)"
case "$prof" in
  default)
    # Exactly Docker's default capability set, and no mount namespace: a
    # default test must not lean on anything more.
    [ "$bnd" = 00000000a80425fb ] || fail "bounding set $bnd is not Docker's default (00000000a80425fb)"
    echo "control: Docker's default bounding set"
    if unshare -m true 2>/dev/null; then fail "a mount namespace could be made (SYS_ADMIN?)"; fi
    echo "control: no mount namespace"
    ;;
  sysadmin | netadmin)
    check "private mount namespace with a tmpfs" unshare -m sh -c 'mkdir -p /work/.t && mount -t tmpfs none /work/.t'
    check "overlay mount" sh -c 'mkdir -p /work/.l /work/.u /work/.w /work/.m && echo x >/work/.l/a &&
      unshare -m sh -c "mount -t overlay overlay -o lowerdir=/work/.l,upperdir=/work/.u,workdir=/work/.w /work/.m && grep -qx x /work/.m/a"'
    check "user namespace" unshare -U true
    rm -rf /work/.t /work/.l /work/.u /work/.w /work/.m
    if [ "$prof" = netadmin ]; then
      check "loopback up in a new network namespace" unshare -n sh -c 'ip link set lo up && ip -o link show lo | grep -q "UP"'
    elif unshare -n ip link set lo up 2>/dev/null; then
      fail "NET_ADMIN present in the sysadmin profile"
    else
      echo "control: no NET_ADMIN"
    fi
    ;;
  *) fail "unknown profile" ;;
esac

mapfile -t names <"/t2c/$prof.txt"
[ "${#names[@]}" -gt 0 ] || { echo "T2c $prof tests: none listed"; exit 0; }
ran=0
for bin in /t2c/bin/*; do
  # libtest takes several filters; --ignored runs only the ignored tests.
  out="$("$bin" --ignored --test-threads=1 "${names[@]}" 2>&1)" || {
    echo "$out"
    echo "error: $(basename "$bin") failed" >&2
    exit 1
  }
  n="$(sed -nE 's/^test result: ok\. ([0-9]+) passed.*/\1/p' <<<"$out" | awk '{ s += $1 } END { print s + 0 }')"
  if [ "$n" -gt 0 ]; then
    echo "$out" | grep -E '^test .* \.\.\. '
    ran=$((ran + n))
  fi
done
echo "T2c $prof tests: $ran passed of ${#names[@]} listed"
[ "$ran" = "${#names[@]}" ] || { echo "error: expected ${#names[@]} T2c $prof tests to run" >&2; exit 1; }
