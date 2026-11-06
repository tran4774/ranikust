# ADR-0101 (T21): kaniko mode on `/` in a container that shares its PID
# namespace with another one (`--pid container:<peer>`). A root peer with a
# writable bind of its own, which ranikust reaches through /proc when it
# starts, is refused before the state directory exists and before any RUN
# (the red team's configuration, with a RUN that only looks); a peer of
# another user, which ranikust does not reach, and no peer build. A set-up
# that failed after its `unshare` (`half_entered`) builds with no peer and
# still refuses a root peer, ranikust started from `sh` or exec'd.
case_run() {
  local d
  d="$(out peers)"
  rm -rf "$d" && mkdir -p "$d/ctx" "$d/look" "$d/bin" "$d/w"
  printf 'FROM scratch\nCOPY Dockerfile /d\n' >"$d/ctx/Dockerfile"
  printf '%s\n' "FROM $BASE" \
    'RUN for p in /proc/[0-9]*; do [ -d "$p/root/w" ] && echo "RUN reached $p"; done; true' \
    >"$d/look/Dockerfile"
  printf '%s\n' '{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["mount"],
    "action": "SCMP_ACT_ERRNO", "errnoRet": 1}]}' >"$d/no-mount.json"
  install -m 0755 "$RNK" "$d/bin/ranikust"
  peer refused "a root peer with a writable bind of its own" "$d/look" 1 -v "$d/w:/w"
  peer built "a peer of another user" "$d/ctx" 2 --user 1000:1000
  peer built "no peer" "$d/ctx" ""
  half_entered built "a half-entered set-up, no peer" sh "$d/ctx" ""
  half_entered refused "a half-entered set-up, a root peer" sh "$d/look" 3 -v "$d/w:/w"
  half_entered refused "a half-entered set-up, a root peer, ranikust exec'd" exec \
    "$d/look" 4 -v "$d/w:/w"
}

# start_peer N ARG...: starts the N-th peer container, with ARG, running
# `sleep`; prints its name.
start_peer() {
  local name="rnk-peer-$$-$1"
  shift
  stop_peer "$name"
  $ENGINE run -d --rm --name "$name" "$@" "$BASE" sleep 300 >/dev/null || return 1
  echo "$name"
}

# stop_peer NAME: stops and removes the peer NAME, if it is there.
stop_peer() {
  # Both engines take kill and `rm -f` alike; podman's `rm` alone takes -t.
  $ENGINE kill "$1" >/dev/null 2>&1 || true
  $ENGINE rm -f "$1" >/dev/null 2>&1 || true
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
    name="$(start_peer "$@")" || fail "$what: the peer did not start" || return 1
    pid=(--pid "container:$name")
  fi
  got="$($ENGINE run --rm ${pid[@]+"${pid[@]}"} -v "$(out peers)/bin/ranikust:/ranikust/bin/ranikust:ro" \
    -v "$ctx:/ranikust/ctx:ro" "$BASE" sh -c '
    /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
      --state-dir /var/lib/rnk-peers
    rc=$?; test -e /var/lib/rnk-peers && echo STATE-MADE; exit $rc' 2>&1)" || rc=$?
  [ -z "$name" ] || stop_peer "$name"
  if [ "$want" = built ]; then
    [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got"
  else
    refused "$rc" "$got"
  fi || {
    echo "$got" >&2
    fail "$what: exit $rc, want $want"
  }
}

# half_entered WANT WHAT HOW CTX N ARG...: as `peer`, in a container given
# CAP_SYS_ADMIN whose seccomp profile refuses `mount`: the set-up's
# `unshare` succeeds and its remount fails, so ranikust goes on in a copy of
# the container's mount namespace. ranikust is started from `sh`, which
# stays in the namespace ranikust started in and tells whether the state
# directory was made, or exec'd, the only process there (HOW `sh` or
# `exec`). It builds with no peer (WANT `built`) and is refused for a root
# peer's `sleep` before any RUN.
half_entered() {
  local want="$1" what="$2" how="$3" ctx="$4" d name="" pid=() run got rc=0
  shift 4
  d="$(out peers)"
  if [ -n "$1" ]; then
    name="$(start_peer "$@")" || fail "$what: the peer did not start" || return 1
    pid=(--pid "container:$name")
  fi
  run=(/ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push
    --state-dir /var/lib/rnk-peers --verbosity=debug --log-format=text)
  # `sh` waits for ranikust, so that it is not exec'd away.
  [ "$how" = exec ] ||
    run=(sh -c '"$@"; rc=$?; test -e /var/lib/rnk-peers && echo STATE-MADE; exit $rc' sh
      "${run[@]}")
  got="$($ENGINE run --rm ${pid[@]+"${pid[@]}"} --cap-add SYS_ADMIN \
    --security-opt "seccomp=$d/no-mount.json" \
    -v "$d/bin/ranikust:/ranikust/bin/ranikust:ro" -v "$ctx:/ranikust/ctx:ro" \
    "$BASE" "${run[@]}" 2>&1)" || rc=$?
  [ -z "$name" ] || stop_peer "$name"
  grep -qF 'RUN without the full sandbox: make / a recursive slave mount' <<<"$got" &&
    if [ "$want" = built ]; then
      [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got"
    else
      refused "$rc" "$got"
    fi || {
    echo "$got" >&2
    fail "$what: exit $rc, want $want"
  }
}

# refused RC GOT: a build that exited RC, printing GOT, was refused for a
# peer's `sleep`, with no RUN reaching the peer and no state directory said
# to be made (STATE-MADE).
refused() {
  [ "$1" = 1 ] && grep -qE 'sharing its PID namespace with process [0-9]+ \(sleep\)' <<<"$2" &&
    grep -qF 'shareProcessNamespace' <<<"$2" &&
    ! grep -q -e STATE-MADE -e 'RUN reached' <<<"$2"
}
