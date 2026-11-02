#!/usr/bin/env bash
# Runs in the T2 VM (ADR-0051): ranikust builds in kaniko mode on the `/`
# of a default Docker container (no --privileged, no --cap-add), with RUN
# steps that attack it (guest/secrets/attack.sh). Every vector must be
# blocked, except the two ranikust can only warn about:
#
#   V1  anything in the state directory /var/lib/ranikust
#   V2  ranikust's descriptors: /proc/<ranikust>/fd, and the marker of an
#       earlier layer through any /proc/*/fd/*
#   V3  ranikust's environment: the tokens in /proc/<ranikust>/environ
#   V4  the RUN's own environment
#   V5  processes a RUN left (`&`, `setsid`, double fork) alive at the next
#   V6  V2 and V3 again from `unshare -Ur`
#   V7  the tokens (given in the environment) in any /proc/*/cmdline
#   V8  a --build-arg value in ranikust's cmdline: readable (expected),
#       ranikust warns
#   V9  the tokens in another process's environment: blocked with ranikust
#       as PID 1; readable (expected) through the shell that started it
#       in the `shell` scenario, ranikust warns
#   V10 ranikust's memory: /proc/<ranikust>/mem
#   V11 capabilities ranikust holds beyond the RUN's (ADR-0078): none,
#       with --cap-add SYS_PTRACE too
#   V12 a --secret file a RUN swapped for a symlink to /proc/self/comm: a
#       later RUN still gets the bytes read at start-up (ADR-0078)
#
# Scenarios: `pid1` (ranikust is the container's PID 1), `ptrace` (the same
# with --cap-add SYS_PTRACE: the RUN must not get it), `shell` (started by
# `sh -c`, as a CI script would). Also measures whether the container's
# overlayfs takes O_TMPFILE, and how ranikust made its state files
# (O_TMPFILE, a name unlinked at once, or memfd).
#
# Then `stopsig` (ADR-0077): a RUN that forges a store on the state volume
# and signals ranikust cannot stop the build, and what it wrote goes.
#
# Then `memlimit` (ADR-0056): a build whose state (a 512 MiB layer) exceeds
# the container's memory limit (--memory 256m, no swap) must succeed: the
# state files are on disk, never in memfd.
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
[ -f vm.env ] && source vm.env

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
IMAGE="$REG/library/ubuntu:24.04"
RNK="$HOME/rnk/bin/ranikust"
OUT="$HOME/rnk/out/secrets"
W=/var/tmp/rnk-secrets
AUTH='{"auths":{"registry.invalid":{"auth":"dXNlcjp0b2stUk5LLTQyNDI="}}}'

sudo rm -rf "$W" && sudo mkdir -m 755 "$W" "$W/ctx"
rm -rf "$OUT" && mkdir -p "$OUT"
sudo cp guest/secrets/attack.sh "$W/ctx/attack.sh"
sudo tee "$W/ctx/Dockerfile" >/dev/null <<EOF
FROM $IMAGE
ARG NPM_TOKEN
COPY attack.sh /attack.sh
RUN bash /attack.sh seed
RUN bash /attack.sh unseed
RUN for v in V1 V2 V3 V4 V6 V7 V8 V9 V10 V11; do bash /attack.sh \$v; done
RUN bash /attack.sh swap
RUN --mount=type=secret,id=s,env=S bash /attack.sh V12
RUN bash /attack.sh spawn
RUN bash /attack.sh V5
EOF

for i in 1 2 3 4 5; do
  sudo docker pull -q "$IMAGE" >/dev/null && break
  [ "$i" = 5 ] && { echo "error: cannot pull $IMAGE" >&2; exit 1; }
  sleep $((i * 5))
done

# Does overlayfs, the container's root, take O_TMPFILE on this kernel?
probe='use Fcntl; if (sysopen(my $f, "/var/lib", O_RDWR | 0x410000, 0600)) { print "ok\n" } else { print "refused: $!\n" }'
tmpfile="$(sudo docker run --rm "$IMAGE" perl -e "$probe" 2>&1 || true)"
echo "kernel $(uname -r): O_TMPFILE on the container's overlayfs root: $tmpfile" | tee "$OUT/o_tmpfile.txt"

ARGS=(--mode=kaniko --context /ranikust/ctx --no-push --oci-layout-path /ranikust/out/oci
  --build-arg NPM_TOKEN=argv-secret-4242 --secret id=s,src=/ranikust/out/sec/s)

# scenario NAME [docker run options...]: builds, keeps the log, the exit.
scenario() {
  local name="$1" o="$W/out-$1" rc=0
  shift
  sudo rm -rf "$o" && sudo mkdir -m 755 "$o" "$o/sec"
  # A file secret on a volume a RUN may write (V12).
  printf file-secret-RNK-5151 | sudo tee "$o/sec/s" >/dev/null
  # ubuntu:24.04 has no CA certificates: the VM's, read-only.
  local common=(--rm -v "$RNK:/ranikust/bin/ranikust:ro" -v "$W/ctx:/ranikust/ctx:ro"
    -v "$o:/ranikust/out" -v /etc/ssl/certs:/etc/ssl/certs:ro
    -e REGISTRY_TOKEN=tok-RNK-4242 -e "DOCKER_AUTH_CONFIG=$AUTH")
  case "$name" in
    shell)
      sudo docker run "${common[@]}" "$@" "$IMAGE" \
        sh -c '/ranikust/bin/ranikust "$@"; rc=$?; exit $rc' sh "${ARGS[@]}" \
        >"$OUT/$name.log" 2>&1 || rc=$?
      ;;
    *)
      sudo docker run "${common[@]}" "$@" "$IMAGE" /ranikust/bin/ranikust "${ARGS[@]}" \
        >"$OUT/$name.log" 2>&1 || rc=$?
      ;;
  esac
  echo "$rc" >"$OUT/$name.rc"
}

scenario pid1
scenario ptrace --cap-add SYS_PTRACE
scenario shell

# The expectations: blocked everywhere but V8 (always, warned) and V9 in
# `shell` (warned).
expect() {
  case "$1:$2" in
    *:V8 | shell:V9) echo LEAK ;;
    *) echo blocked ;;
  esac
}

status=0
VECTORS=(V1 V2 V3 V4 V5 V6 V7 V8 V9 V10 V11 V12)
{
  printf '%-8s' scenario
  printf ' %-8s' "${VECTORS[@]}"
  printf '\n'
  for s in pid1 ptrace shell; do
    printf '%-8s' "$s"
    for v in "${VECTORS[@]}"; do
      got="$(sed -nE "s/^VEC $v ([A-Za-z]+).*/\1/p" "$OUT/$s.log" | tail -n1)"
      want="$(expect "$s" "$v")"
      [ -n "$got" ] || got=missing
      mark=""
      [ "$got" = "$want" ] || { mark="!"; status=1; }
      printf ' %-8s' "$got$mark"
    done
    printf '\n'
  done
} | tee "$OUT/table.txt"
# The table runs in a subshell: its verdict is read back from it.
! grep -q '!' "$OUT/table.txt" || status=1
grep -h '^VEC' "$OUT"/*.log >"$OUT/vectors.txt" || true

for s in pid1 ptrace shell; do
  rc="$(cat "$OUT/$s.rc")"
  if [ "$rc" != 0 ] || ! grep -qE '^sha256:[0-9a-f]{64}$' "$OUT/$s.log"; then
    echo "error: scenario $s: build failed (exit $rc), see $OUT/$s.log" >&2
    tail -20 "$OUT/$s.log" >&2
    status=1
  fi
  grep -q -- '--build-arg NPM_TOKEN=' "$OUT/$s.log" ||
    { echo "error: scenario $s: no warning for the secret-looking --build-arg" >&2; status=1; }
  grep -q -- '--secret id=s: .*every RUN can read' "$OUT/$s.log" ||
    { echo "error: scenario $s: no warning that every RUN reads the --secret file" >&2; status=1; }
  if grep -q 'state in memory (memfd)' "$OUT/$s.log"; then
    echo "$s: state in memfd"
  elif grep -q 'made by a name unlinked at once' "$OUT/$s.log"; then
    echo "$s: state on disk, files named then unlinked at once (no O_TMPFILE)"
  else
    echo "$s: state in O_TMPFILE files"
  fi
done
# stopsig (ADR-0077): a RUN removes the store's marker, forges a persistent
# store on the state volume, then sends ranikust (PID 1) SIGTERM twice. The
# build must go on, the signals ignored, and the volume end holding only
# the marker, written back and kept since a RUN removed it.
sudo mkdir -m 755 "$W/stopsig"
sudo tee "$W/stopsig/Dockerfile" >/dev/null <<EOF
FROM $IMAGE
RUN rm -f /var/lib/ranikust/.ranikust-building \
 && touch /var/lib/ranikust/.ranikust-state \
 && mkdir -p /var/lib/ranikust/layers/v2/sha256/0707 \
 && echo poison > /var/lib/ranikust/layers/v2/sha256/0707/planted \
 && kill -TERM 1 && sleep 1 && kill -TERM 1 && sleep 1
RUN true
EOF
so="$W/out-stopsig" && sudo rm -rf "$so" && sudo mkdir -m 755 "$so"
sudo docker volume rm -f rnk-stopsig >/dev/null 2>&1 || true
sudo docker volume create rnk-stopsig >/dev/null
rc=0
sudo docker run --rm -v "$RNK:/ranikust/bin/ranikust:ro" -v "$W/stopsig:/ranikust/ctx:ro" \
  -v "$so:/ranikust/out" -v rnk-stopsig:/var/lib/ranikust -v /etc/ssl/certs:/etc/ssl/certs:ro \
  "$IMAGE" /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
  --oci-layout-path /ranikust/out/oci >"$OUT/stopsig.log" 2>&1 || rc=$?
left="$(sudo docker run --rm -v rnk-stopsig:/s "$IMAGE" ls -A /s)"
sudo docker volume rm -f rnk-stopsig >/dev/null
if [ "$rc" != 0 ] || ! grep -qE '^sha256:[0-9a-f]{64}$' "$OUT/stopsig.log"; then
  echo "error: stopsig: a RUN's SIGTERM stopped the build (exit $rc), see $OUT/stopsig.log" >&2
  tail -20 "$OUT/stopsig.log" >&2
  status=1
elif [ "$(grep -c 'a RUN cannot stop the build' "$OUT/stopsig.log")" != 2 ]; then
  echo "error: stopsig: the two signals were not reported as ignored" >&2
  status=1
elif [ "$left" != .ranikust-building ]; then
  echo "error: stopsig: the state volume holds '$left', not only the marker" >&2
  status=1
elif ! grep -q 'is a volume' "$OUT/stopsig.log"; then
  echo "error: stopsig: no warning that the state directory is a volume" >&2
  status=1
else
  echo "stopsig: a RUN's SIGTERM ignored twice; the forged store removed; the marker kept"
fi

# memlimit: state larger than the container's memory, on disk.
sudo mkdir -m 755 "$W/big"
sudo tee "$W/big/Dockerfile" >/dev/null <<EOF
FROM $IMAGE
RUN head -c 512M /dev/urandom > /big.bin
RUN true
EOF
mo="$W/out-memlimit" && sudo rm -rf "$mo" && sudo mkdir -m 755 "$mo"
rc=0
sudo docker run --rm --memory 256m --memory-swap 256m \
  -v "$RNK:/ranikust/bin/ranikust:ro" -v "$W/big:/ranikust/ctx:ro" -v "$mo:/ranikust/out" \
  -v /etc/ssl/certs:/etc/ssl/certs:ro "$IMAGE" \
  /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
  --oci-layout-path /ranikust/out/oci >"$OUT/memlimit.log" 2>&1 || rc=$?
echo "$rc" >"$OUT/memlimit.rc"
if [ "$rc" != 0 ] || ! grep -qE '^sha256:[0-9a-f]{64}$' "$OUT/memlimit.log"; then
  echo "error: memlimit: the build failed (exit $rc; 137 is the OOM killer), see $OUT/memlimit.log" >&2
  tail -20 "$OUT/memlimit.log" >&2
  status=1
elif grep -q 'state in memory (memfd)' "$OUT/memlimit.log"; then
  echo "error: memlimit: state fell back to memfd" >&2
  status=1
else
  echo "memlimit: a 512 MiB layer built under --memory 256m"
fi

grep -q 'holds DOCKER_AUTH_CONFIG' "$OUT/shell.log" ||
  { echo "error: scenario shell: no warning about the shell holding DOCKER_AUTH_CONFIG" >&2; status=1; }
! grep -q 'holds DOCKER_AUTH_CONFIG' "$OUT/pid1.log" ||
  { echo "error: scenario pid1: a holder warning with ranikust as PID 1" >&2; status=1; }

sudo rm -rf "$W"
[ "$status" = 0 ] && echo "secrets: ok"
exit "$status"
