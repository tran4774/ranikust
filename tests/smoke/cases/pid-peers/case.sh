# ADR-0101 (T21): kaniko mode on `/` in a container that shares its PID
# namespace with another one (`--pid container:<peer>`). A root peer with a
# writable bind of its own, which ranikust reaches through /proc when it
# starts, is refused before the state directory exists and before any RUN
# (the red team's configuration, with a RUN that only looks); a peer of
# another user, which ranikust does not reach, and no peer build; and a
# set-up that failed after its `unshare` builds (`half_entered`).
case_run() {
  local d
  d="$(out peers)"
  rm -rf "$d" && mkdir -p "$d/ctx" "$d/look" "$d/bin" "$d/w"
  printf 'FROM scratch\nCOPY Dockerfile /d\n' >"$d/ctx/Dockerfile"
  printf '%s\n' "FROM $BASE" \
    'RUN for p in /proc/[0-9]*; do [ -d "$p/root/w" ] && echo "RUN reached $p"; done; true' \
    >"$d/look/Dockerfile"
  install -m 0755 "$RNK" "$d/bin/ranikust"
  peer refused "a root peer with a writable bind of its own" "$d/look" 1 -v "$d/w:/w"
  peer built "a peer of another user" "$d/ctx" 2 --user 1000:1000
  peer built "no peer" "$d/ctx" ""
  half_entered "$d"
}

# half_entered DIR: a kaniko-mode set-up whose `unshare` succeeded and whose
# remount failed (CAP_SYS_ADMIN, `mount` refused by seccomp) leaves ranikust
# in a copy of the container's mount namespace; the container's `sh`, of the
# namespace ranikust started in, is still its own, so the build goes on.
half_entered() {
  local d="$1" got rc=0
  printf '%s\n' '{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["mount"],
    "action": "SCMP_ACT_ERRNO", "errnoRet": 1}]}' >"$d/no-mount.json"
  # `sh` waits for ranikust, so that it is not exec'd away.
  got="$($ENGINE run --rm --cap-add SYS_ADMIN --security-opt "seccomp=$d/no-mount.json" \
    -v "$d/bin/ranikust:/ranikust/bin/ranikust:ro" -v "$d/ctx:/ranikust/ctx:ro" "$BASE" sh -c '
    /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
      --state-dir /var/lib/rnk-peers --verbosity=debug --log-format=text
    rc=$?; exit $rc' 2>&1)" || rc=$?
  [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got" &&
    grep -qF 'RUN without the full sandbox: make / a recursive slave mount' <<<"$got" || {
    echo "$got" >&2
    fail "a set-up that failed after its unshare: exit $rc, want built"
  }
}

# peer WANT WHAT CTX N ARG...: a peer container, the N-th, started with ARG
# (none when N is empty), runs `sleep` while ranikust, bound read-only,
# builds CTX on `/` in a default container sharing its PID namespace: it
# builds (WANT `built`) or is refused for the peer's `sleep` before its
# state directory exists and before any RUN.
peer() {
  local want="$1" what="$2" ctx="$3" name="" pid=() got rc=0
  shift 3
  if [ -n "$1" ]; then
    name="rnk-peer-$$-$1"
    shift
    # Both engines take kill and `rm -f` alike; podman's `rm` alone takes -t.
    $ENGINE kill "$name" >/dev/null 2>&1 || true
    $ENGINE rm -f "$name" >/dev/null 2>&1 || true
    $ENGINE run -d --rm --name "$name" "$@" "$BASE" sleep 300 >/dev/null ||
      fail "$what: the peer did not start" || return 1
    pid=(--pid "container:$name")
  fi
  got="$($ENGINE run --rm ${pid[@]+"${pid[@]}"} -v "$(out peers)/bin/ranikust:/ranikust/bin/ranikust:ro" \
    -v "$ctx:/ranikust/ctx:ro" "$BASE" sh -c '
    /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
      --state-dir /var/lib/rnk-peers
    rc=$?; test -e /var/lib/rnk-peers && echo STATE-MADE; exit $rc' 2>&1)" || rc=$?
  if [ -n "$name" ]; then
    $ENGINE kill "$name" >/dev/null 2>&1 || true
    $ENGINE rm -f "$name" >/dev/null 2>&1 || true
  fi
  if [ "$want" = built ]; then
    [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got"
  else
    [ "$rc" = 1 ] && grep -qE 'sharing its PID namespace with process [0-9]+ \(sleep\)' <<<"$got" &&
      grep -qF 'shareProcessNamespace' <<<"$got" &&
      ! grep -q -e STATE-MADE -e 'RUN reached' <<<"$got"
  fi || {
    echo "$got" >&2
    fail "$what: exit $rc, want $want"
  }
}
