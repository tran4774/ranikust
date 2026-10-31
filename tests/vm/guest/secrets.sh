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
#
# Scenarios: `pid1` (ranikust is the container's PID 1), `ptrace` (the same
# with --cap-add SYS_PTRACE: the RUN must not get it), `shell` (started by
# `sh -c`, as a CI script would). Also measures whether the container's
# overlayfs takes O_TMPFILE, and whether ranikust fell back to memfd.
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
RUN for v in V1 V2 V3 V4 V6 V7 V8 V9 V10; do bash /attack.sh \$v; done
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
  --build-arg NPM_TOKEN=argv-secret-4242)

# scenario NAME [docker run options...]: builds, keeps the log, the exit.
scenario() {
  local name="$1" o="$W/out-$1" rc=0
  shift
  sudo rm -rf "$o" && sudo mkdir -m 755 "$o"
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
VECTORS=(V1 V2 V3 V4 V5 V6 V7 V8 V9 V10)
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
  if grep -q 'state is kept in memory (memfd)' "$OUT/$s.log"; then
    echo "$s: state in memfd (no O_TMPFILE)"
  else
    echo "$s: state in O_TMPFILE files"
  fi
done
grep -q 'holds DOCKER_AUTH_CONFIG' "$OUT/shell.log" ||
  { echo "error: scenario shell: no warning about the shell holding DOCKER_AUTH_CONFIG" >&2; status=1; }
! grep -q 'holds DOCKER_AUTH_CONFIG' "$OUT/pid1.log" ||
  { echo "error: scenario pid1: a holder warning with ranikust as PID 1" >&2; status=1; }

sudo rm -rf "$W"
[ "$status" = 0 ] && echo "secrets: ok"
exit "$status"
