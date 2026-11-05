# ADR-0101 (T21): kaniko mode on `/` in a container that shares its PID
# namespace with another one (`--pid container:<peer>`). A root peer with a
# writable bind of its own, which ranikust reaches through /proc when it
# starts, is refused before the state directory exists and before any RUN
# (the red team's configuration, with a RUN that only looks); a peer of
# another user, which ranikust does not reach, and no peer build.
case_run() {
  local d
  d="$(out peers)"
  rm -rf "$d" && mkdir -p "$d/ctx" "$d/look" "$d/t"
  printf 'FROM scratch\nCOPY Dockerfile /d\n' >"$d/ctx/Dockerfile"
  printf '%s\n' "FROM $BASE" \
    'RUN for p in /proc/[0-9]*; do [ -d "$p/root/w" ] && echo "RUN reached $p"; done; true' \
    >"$d/look/Dockerfile"
  install -m 0755 "$RNK" "$d/t/ranikust"
  peer refused "a root peer with a writable bind of its own" "$d/look" 1 -v "$d/t:/w"
  peer built "a peer of another user" "$d/ctx" 2 --user 1000:1000 -v "$d/t:/w"
  peer built "no peer" "$d/ctx" ""
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
  got="$($ENGINE run --rm ${pid[@]+"${pid[@]}"} -v "$(out peers)/t/ranikust:/ranikust/bin/ranikust:ro" \
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
