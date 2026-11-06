# ADR-0101 (T21): kaniko mode on `/` refuses a PID namespace shared with a
# process of another mount namespace that it reaches as it starts; what
# each step checks is in docs/testing.md (`pid-peers`).
case_run() {
  local d
  d="$(out peers)"
  rm -rf "$d" && mkdir -p "$d/ctx" "$d/look" "$d/bin" "$d/w"
  printf 'FROM scratch\nCOPY Dockerfile /d\n' >"$d/ctx/Dockerfile"
  # Four descriptors: the standard streams and the directory `ls` lists.
  printf '%s\n' "FROM $BASE" \
    'RUN n=0; for p in /proc/[0-9]*; do n=$((n + 1)); [ -d "$p/root/w" ] && echo "RUN reached $p"; done; echo "RUN looked at $n processes"; echo "RUN saw PID 1 as $(cat /proc/1/comm)"' \
    'RUN ls -l /proc/self/fd; test "$(ls /proc/self/fd | wc -l)" -le 4' \
    >"$d/look/Dockerfile"
  printf '%s\n' '{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["mount"],
    "action": "SCMP_ACT_ERRNO", "errnoRet": 1}]}' >"$d/no-mount.json"
  # Refuses ranikust's remount (MS_SLAVE), not busybox unshare's (MS_PRIVATE).
  printf '%s\n' '{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{"names": ["mount"],
    "action": "SCMP_ACT_ERRNO", "errnoRet": 1, "args": [{"index": 3, "value": 524288,
    "valueTwo": 524288, "op": "SCMP_CMP_MASKED_EQ"}]}]}' >"$d/no-slave.json"
  install -m 0755 "$RNK" "$d/bin/ranikust"
  peer refused "a root peer with a writable bind of its own" "$d/look" 1 -v "$d/w:/w"
  peer built "a peer of another user" "$d/ctx" 2 --user 1000:1000
  peer built "no peer" "$d/ctx" ""
  half_entered built "a half-entered set-up, no peer" sh "$d/ctx" ""
  half_entered refused "a half-entered set-up, a root peer" sh "$d/look" 3 -v "$d/w:/w"
  half_entered refused "a half-entered set-up, a root peer, ranikust exec'd" exec \
    "$d/look" 4 -v "$d/w:/w"
  peer built "a root peer with a capability ranikust lacks" "$d/look" 5 --cap-add NET_ADMIN \
    -v "$d/w:/w"
  half_entered built "a half-entered set-up, no peer, a RUN" sh "$d/look" ""
  half_entered built "a half-entered set-up, no peer, a RUN, ranikust exec'd" exec "$d/look" ""
  half_entered refused "a half-entered set-up, a process of its own unshared just before" \
    unshared "$d/look" ""
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
# builds (WANT `built`), its RUNs checked as `built` says, or is refused
# for the peer's `sleep` before its state directory exists and before any
# RUN.
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
    built "$ctx" "$rc" "$got" "$name"
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
# `exec`); or from `sh` once a `sleep` of the container has entered a mount
# namespace of its own (HOW `unshared`; seccomp then refuses only a slave
# remount, and no AppArmor profile refuses busybox's private one). It
# builds with no peer (WANT `built`) and is refused for a root peer's
# `sleep` before any RUN; with HOW `unshared` it is refused with no peer,
# for the container's own `sleep`.
half_entered() {
  local want="$1" what="$2" how="$3" ctx="$4" d name="" pid=() run got rc=0 opts
  shift 4
  d="$(out peers)"
  opts=(--security-opt "seccomp=$d/no-mount.json")
  if [ -n "$1" ]; then
    name="$(start_peer "$@")" || fail "$what: the peer did not start" || return 1
    pid=(--pid "container:$name")
  fi
  run=(/ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push
    --state-dir /var/lib/rnk-peers --verbosity=debug --log-format=text)
  # `sh` waits for ranikust, so that it is not exec'd away.
  case "$how" in
    sh)
      run=(sh -c '"$@"; rc=$?; test -e /var/lib/rnk-peers && echo STATE-MADE; exit $rc' sh
        "${run[@]}")
      ;;
    unshared)
      opts=(--security-opt "seccomp=$d/no-slave.json" --security-opt apparmor=unconfined)
      run=(sh -c 'unshare -m sleep 300 & p=$! i=0 n=
        until n="$(readlink /proc/$p/ns/mnt)" && [ "$n" != "$(readlink /proc/self/ns/mnt)" ]; do
          i=$((i + 1)) && [ $i -lt 100 ] && sleep 0.1 || { echo NOT-UNSHARED; exit 9; }
        done
        "$@"; rc=$?; kill $p; test -e /var/lib/rnk-peers && echo STATE-MADE; exit $rc' sh
        "${run[@]}")
      ;;
  esac
  got="$($ENGINE run --rm ${pid[@]+"${pid[@]}"} --cap-add SYS_ADMIN "${opts[@]}" \
    -v "$d/bin/ranikust:/ranikust/bin/ranikust:ro" -v "$ctx:/ranikust/ctx:ro" \
    "$BASE" "${run[@]}" 2>&1)" || rc=$?
  [ -z "$name" ] || stop_peer "$name"
  grep -qF 'RUN without the full sandbox: make / a recursive slave mount' <<<"$got" &&
    if [ "$want" = built ]; then
      built "$ctx" "$rc" "$got" "$name"
    else
      refused "$rc" "$got"
    fi || {
    echo "$got" >&2
    fail "$what: exit $rc, want $want"
  }
}

# built CTX RC GOT PEER: a build of CTX that exited RC, printing GOT, made
# an image; with the `look` context its first RUN went through every
# process, saw PID 1 as the `sleep` of PEER when one runs, and reached no
# peer, and its second held only the standard streams.
built() {
  [ "$2" = 0 ] && grep -q '^sha256:' <<<"$3" && ! grep -q 'RUN reached /proc/[0-9]' <<<"$3" &&
    { [ "$1" != "$(out peers)/look" ] || { grep -qE 'RUN looked at [0-9]+ processes' <<<"$3" &&
      { [ -z "$4" ] || grep -q 'RUN saw PID 1 as sleep$' <<<"$3"; }; }; }
}

# refused RC GOT: a build that exited RC, printing GOT, was refused for a
# peer's `sleep` before any RUN, and no state directory was said to be made
# (STATE-MADE).
refused() {
  [ "$1" = 1 ] && grep -qE 'sharing its PID namespace with process [0-9]+ \(sleep\)' <<<"$2" &&
    grep -qF 'shareProcessNamespace' <<<"$2" &&
    ! grep -q -e STATE-MADE -e 'RUN reached' -e 'RUN looked at' <<<"$2"
}
