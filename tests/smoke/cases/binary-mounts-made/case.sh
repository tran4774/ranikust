# Red team (ADR-0101, T21): mount tables made inside a default container by
# a privileged exec in its mount namespace, ranikust keeping the container's
# capabilities: a writable mount covered by a later one, stacked binds of
# the binary and another file over its name (run from a descriptor opened
# before), and a mount added while a long walk runs are refused before the
# state directory exists; a mount on a writable mount, and a deep chain of
# writable mounts, build. A container told only once ranikust started is
# refused as not weighed.
case_run() {
  # A privileged `docker exec` keeps the container's AppArmor and seccomp
  # profiles, which deny `mount`.
  [ "$ENGINE" = podman ] || return 0
  local d bin unplaced='cannot tell whether a writable mount reaches' chain
  d="$(out made)"
  rm -rf "$d" && mkdir -p "$d/ctx" "$d/t" "$d/o/a" "$d/f"
  printf 'FROM scratch\nCOPY Dockerfile /d\n' >"$d/ctx/Dockerfile"
  install -m 0755 "$RNK" "$d/t/ranikust"
  printf 'other\n' >"$d/f/other"
  bin=(-v "$d/t/ranikust:/ranikust/bin/ranikust:ro")
  made reached "its tree bound read-write, a mount over it later" path \
    'mount -t tmpfs t /w' '' "${bin[@]}" -v "$d/t:/w"
  made "$unplaced" "another writable mount of its file system, a mount over it later" path \
    'mount -t tmpfs t /o' '' "${bin[@]}" -v "$d/o:/o"
  made built "another writable mount of its file system, a mount on it" path \
    'mount -t tmpfs t /o/a' '' "${bin[@]}" -v "$d/o:/o"
  made reached "the same file bound read-write at its path, a read-only bind over it" /p \
    'touch /p && mount --bind /ranikust/bin/ranikust /p && mount -o remount,bind,rw /p &&
     mount --bind /p /p && mount -o remount,bind,ro /p' '' "${bin[@]}"
  made reached "the same file bound read-write over its name, run from a descriptor" fd \
    'mount --bind /ranikust/bin/ranikust /ranikust/bin/ranikust &&
     mount -o remount,bind,rw /ranikust/bin/ranikust' '' "${bin[@]}"
  made "$unplaced" "another file bound over its name, run from a descriptor" fd \
    'mount --bind /f/other /ranikust/bin/ranikust' '' "${bin[@]}" -v "$d/f:/f:ro"
  made "$unplaced" "its directory covered, run from a descriptor" fd \
    'mount -t tmpfs t /ranikust/bin' '' "${bin[@]}"
  # A chain of writable binds, each on the one before, makes the walk last
  # (about 1.5 s for 1000 on a laptop): its tree bound read-write once the
  # table was first read is caught by the second read. Two names for the
  # remount: with one, busybox looks for it in /proc/mounts, whose chain
  # lines are longer than its buffer.
  chain='p=/n; mkdir -p /n /w; i=0
    while [ $i -lt 1000 ]; do mount --bind /o $p; p=$p/a; i=$((i + 1)); done'
  made built "a chain of 1000 writable mounts of its file system" path \
    "$chain" '' "${bin[@]}" -v "$d/o:/o" -v "$d/t:/t:ro"
  made "$unplaced" "its tree bound read-write while the walk runs" path \
    "$chain" 'sleep 0.2; mount --bind /t /w && mount -o remount,bind,rw /w /w' \
    "${bin[@]}" -v "$d/o:/o" -v "$d/t:/t:ro"
  unweighed
}

# unweighed: ranikust, bound :ro, in a directory root of the base image's
# files where only /.dockerenv, made and removed in a loop, tells a
# container, runs until a first run finds none and its copy finds one: that
# run is refused as not weighed. The others are refused as outside a
# container, or at the state directory, past the binary's check (a build
# would empty the root).
unweighed() {
  local d sig name cid i got
  d="$(out made)"
  sig="$d/nw"
  name="rnk-made-$$"
  rm -rf "$sig" "$d/root" && mkdir -p "$sig" "$d/root"
  cid="$($ENGINE create "$BASE" true)"
  $ENGINE export "$cid" | tar -C "$d/root" -xf -
  $ENGINE rm "$cid" >/dev/null
  $ENGINE rm -f -t 0 "$name" >/dev/null 2>&1 || true
  # PID 1's command line names no runtime: the file's name is split.
  $ENGINE run --rm --name "$name" -e container=none -v "$sig:/sig" \
    -v "$d/t/ranikust:/ranikust/bin/ranikust:ro" -v "$d/ctx:/ranikust/ctx:ro" \
    --rootfs "$d/root" /bin/sh -c '
    touch /sig/up
    while [ ! -e /sig/go ]; do sleep 0.1; done
    f=/.dock""erenv
    while :; do : >"$f"; rm -f "$f"; done &
    i=0
    while [ $i -lt 1000 ]; do
      /ranikust/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
        --state-dir /dev/null/x >/sig/log 2>&1
      echo "$? $(grep -m1 error: /sig/log)" >>/sig/runs
      grep -q "did not find itself in a container" /sig/log && break
      i=$((i + 1))
    done
    kill $!' >"$sig.log" 2>&1 &
  local pid=$!
  for i in $(seq 600); do
    [ -e "$sig/up" ] && break
    sleep 0.1
  done
  # Podman's own mark, bound over a file of the root.
  [ -e "$sig/up" ] && $ENGINE exec --privileged "$name" \
    sh -c 'umount /run/.containerenv && rm /run/.containerenv' || {
    touch "$sig/go"
    wait "$pid" || true
    cat "$sig.log" >&2
    fail "not weighed: the container did not start, or its mark stayed"
    return 1
  }
  touch "$sig/go"
  wait "$pid" || true
  got="$(cat "$sig/runs")"
  tail -n 1 <<<"$got" | grep -q '^1 ranikust: error: --mode=kaniko: ranikust did not find itself' &&
    ! grep -v -e '^1 ranikust: error: --mode=kaniko: ranikust did not find itself' \
      -e '^1 ranikust: error: kaniko mode outside a container' \
      -e '^1 ranikust: error: cannot open /dev/null as a directory' <<<"$got" >/dev/null || {
    sort <<<"$got" | uniq -c >&2
    fail "not weighed: a run was not refused as expected, or none was refused as not weighed"
  }
}

# made WANT WHAT BIN SETUP DURING ARG...: ranikust in a default container
# with ARG (binds), started from BIN (`path` for /ranikust/bin/ranikust,
# `fd` for a descriptor of it opened before SETUP, or a path), builds on `/`
# (WANT `built`) or is refused before its state directory exists, because a
# writable mount reaches its binary (`reached`) or with WANT in its message.
# SETUP runs in a privileged exec in the container before ranikust starts;
# DURING, if any, in another, from the moment ranikust starts.
made() {
  local want="$1" what="$2" bin="$3" setup="$4" during="$5" name sig pid i got rc=0 dpid=
  shift 5
  case "$bin" in
    path) bin=/ranikust/bin/ranikust ;;
    fd) bin=/proc/self/fd/3 ;;
  esac
  name="rnk-made-$$"
  sig="$(out made)/sig"
  rm -rf "$sig" && mkdir -p "$sig"
  $ENGINE rm -f -t 0 "$name" >/dev/null 2>&1 || true
  $ENGINE run --rm --name "$name" "$@" -v "$sig:/sig" -v "$(out made)/ctx:/ranikust/ctx:ro" \
    "$BASE" sh -c '
    exec 3</ranikust/bin/ranikust
    touch /sig/up
    while [ ! -e /sig/go ]; do sleep 0.1; done
    touch /sig/started
    "$0" --mode=kaniko --context /ranikust/ctx --no-push --state-dir /var/lib/rnk-made
    rc=$?; test -e /var/lib/rnk-made && echo STATE-MADE; exit $rc' "$bin" >"$sig.log" 2>&1 &
  pid=$!
  for i in $(seq 600); do
    [ -e "$sig/up" ] && break
    sleep 0.1
  done
  [ -e "$sig/up" ] && $ENGINE exec --privileged "$name" sh -c "set -e; $setup" || {
    touch "$sig/go"
    wait "$pid" || true
    cat "$sig.log" >&2
    fail "$what: the container did not start, or its mounts were not made"
    return 1
  }
  if [ -n "$during" ]; then
    $ENGINE exec --privileged "$name" sh -c \
      "while [ ! -e /sig/started ]; do sleep 0.01; done; $during" >"$sig.during" 2>&1 &
    dpid=$!
    # The exec is up before ranikust starts.
    sleep 1
  fi
  touch "$sig/go"
  wait "$pid" || rc=$?
  if [ -n "$dpid" ] && ! wait "$dpid"; then
    cat "$sig.log" "$sig.during" >&2
    fail "$what: the mounts made while ranikust ran failed"
    return 1
  fi
  got="$(cat "$sig.log")"
  [ "$want" = reached ] && want='a writable mount also reaches'
  if [ "$want" = built ]; then
    [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got"
  else
    [ "$rc" = 1 ] && grep -qF -- "$want" <<<"$got" && ! grep -q STATE-MADE <<<"$got"
  fi || {
    echo "$got" >&2
    fail "$what: exit $rc, want $want"
  }
}
