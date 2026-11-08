# Red team of --userns as root (#83, ADR-0104, ADR-0139), T2 only (the
# VM's quick suite), with a `ranikust` range of its own in /etc/subuid and
# /etc/subgid, both restored after. A RUN in the build's user namespace
# gets no descriptor but its standard three, writes no kernel, cgroup or
# sysfs file and reaches no process outside its PID namespace (T17, T21,
# T24); it leaves no process of the range running (T8). From the host, an
# account with the range's first ids reaches nothing of ranikust's own
# processes, whose descriptors name none of root's files outside the
# build's directories (T21). ranikust killed while a RUN waits leaves no
# process of the range, and the next --userns build in that state
# directory succeeds, leaving no entry of root's in it (T4).

UW=/var/lib/rnk-quick-userns-reach

case_run() {
  [ "$ENGINE" = "sudo docker" ] || return 0
  [ "${RANIKUST_QUICK_MODE:-}" != kaniko ] || return 0
  ! grep -qF " $UW" /proc/self/mountinfo || fail "mounts left under $UW by an earlier run"
  sudo rm -rf --one-file-system "$UW"
  sudo mkdir -m 755 "$UW" "$UW/ctx" "$UW/out"
  sudo install -m 644 "$SMOKE_CASES_DIR/$CASE/Dockerfile" "$UW/ctx/Dockerfile"
  local f
  for f in subuid subgid; do
    sudo touch "/etc/$f"
    sudo cp -p "/etc/$f" "$UW/$f.orig"
  done
  trap clean_up EXIT
  sudo sed -i '/^ranikust:/d' /etc/subuid /etc/subgid
  # One past every range of both files, uids and gids apart.
  S="$(cat /etc/subuid /etc/subgid | awk -F: 'NF == 3 && $3 > 0 { e = $2 + $3; if (e > m) m = e }
    END { if (m < 1000000) m = 1000000; print int((m + 65535) / 65536) * 65536 }')"
  G=$((S + 65536))
  echo "ranikust:$S:65536" | sudo tee -a /etc/subuid >/dev/null
  echo "ranikust:$G:65536" | sudo tee -a /etc/subgid >/dev/null

  mkdir -p "$CASES_OUT/$CASE"
  writes
  left_behind
  seen_from_the_host
  killed
}

# A RUN's descriptors, writes and reach: its three, every attempt refused,
# the control done.
writes() {
  local log rc=0 report tag="localhost/rnk-case:$CASE-writes"
  log="$(urun writes --target writes --state-dir "$UW/state" --destination "$tag" 2>&1)" || rc=$?
  expect "$rc" 0 "the writes build: $log"
  sudo docker load -q -i "$UW/out/writes/image.tar" >/dev/null || fail "load the writes image"
  report="$(sudo docker run --rm "$tag" cat /report /fds-l)"
  printf '%s\n' "$report" >"$CASES_OUT/$CASE/writes.report"
  echo "  $CASE: measured: $(grep -E '^(fds|cgroup|uid_map)' <<<"$report" | paste -sd ';')" >&2
  expect "$(grep '^fds ' <<<"$report")" "fds 0 1 2 3 " "the descriptors of a RUN's process"
  grep -Eqx "uid_map +0 +$S +65536 *" <<<"$report" || fail "the writes RUN's map: $report"
  ! grep -Eq '^(write [^/]*/(proc|sys)/.*|mkdir cgroup|mount proc)=done$' <<<"$report" ||
    fail "a RUN wrote a kernel interface: $report"
  ! grep -q '^reached ' <<<"$report" || fail "a RUN reached a process outside: $report"
  grep -qx 'write /tmp/control=done' <<<"$report" || fail "the control: $report"
}

# Processes a RUN leaves: none of the range's once the build ends, and the
# cache mount one of them wrote stops growing.
left_behind() {
  local log rc=0
  log="$(urun left --target left --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 0 "the left build (its last RUN fails while the beat grows): $log"
  expect "$(in_range)" "" "processes of the range left by a RUN"
}

# The build's own processes, from the host, while a RUN waits: an account
# with the range's first ids reaches none (environment, root, descriptors,
# working directory) of those in the host's PID namespace; their
# descriptors, read as root, name no file of root's outside the build's
# directories.
seen_from_the_host() {
  local o="$CASES_OUT/$CASE/seen" bg pid p rc=0 r line fds ctl n=0
  rm -rf "$o" && mkdir -p "$o"
  urun seen --target wait --state-dir "$UW/state" >"$o/build.log" 2>&1 &
  bg=$!
  pid="$(waiting)" || fail "no RUN waited: $(cat "$o/build.log")"
  ctl="$(control_reach "$S" "$G")"
  expect "$ctl" " environ root fd cwd" "what a host account reaches of a process of its own (the control)"
  for p in $(pgrep -x ranikust); do
    line="$(awk '/^(Uid|Gid):/ { print $2 }' "/proc/$p/status" 2>/dev/null | paste -sd :)" || continue
    [ -n "$line" ] || continue
    set -- $(awk '/^NSpid:/ { $1 = ""; print }' "/proc/$p/status")
    r="$(reach "$p" "$S" "$G")"
    fds="$({ sudo find "/proc/$p/fd" -mindepth 1 -maxdepth 1 -exec readlink {} + 2>/dev/null || :; } |
      sort -u | paste -sd ' ')"
    echo "  $CASE: measured: ranikust pid $p ids $line nspids $# reach as $S:$G:$r; fds: $fds" >&2
    if [ $# = 1 ]; then
      n=$((n + 1))
      expect "$r" " nothing" "what the range's first ids reach of ranikust's process $p ($line)"
    fi
    ! grep -Eq '(^| )(/root|/etc|/var/lib/ranikust)(/| |$)' <<<"$fds" ||
      fail "ranikust's process $p holds root's files: $fds"
  done
  [ "$n" -gt 0 ] || fail "no ranikust process in the host's PID namespace"
  sudo rm "/proc/$pid/root/tmp/wait-reach"
  wait "$bg" || rc=$?
  expect "$rc" 0 "the seen build: $(cat "$o/build.log")"
}

# ranikust killed while a RUN waits: no process of the range within ten
# seconds; the next build in that state directory succeeds and leaves no
# entry of root's there.
killed() {
  local o="$CASES_OUT/$CASE/killed" bg pid i=0 log rc=0 roots
  rm -rf "$o" && mkdir -p "$o"
  urun killed --target wait --state-dir "$UW/state" >"$o/build.log" 2>&1 &
  bg=$!
  pid="$(waiting)" || fail "no RUN waited: $(cat "$o/build.log")"
  sudo pkill -KILL -x ranikust || true
  wait "$bg" || true
  while [ -n "$(in_range)" ] && [ "$i" -lt 100 ]; do
    sleep 0.1
    i=$((i + 1))
  done
  expect "$(in_range)" "" "processes of the range ten seconds after ranikust was killed"
  log="$(urun after --target writes --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 0 "the build after a killed one: $log"
  roots="$(sudo find "$UW/state" -xdev -uid 0 -printf '%P\n' | paste -sd ' ')"
  echo "  $CASE: measured: entries of root's in the state directory after a killed build: ${roots:-none}" >&2
  expect "$roots" "" "entries of root's in the --userns state directory"
  expect "$(sudo find "$UW/state" -maxdepth 1 -name '.ranikust-anon-userns-probe-*' | wc -l)" 0 \
    "probe directories left after a killed build"
}

# in_range: the host processes whose uid is one of the range's, one per
# line.
in_range() {
  ps -eo pid=,uid=,comm= | awk -v s="$S" '$2 >= s && $2 < s + 65536 { print $1, $2, $3 }'
}

# waiting: the pid of the RUN that waits, once its file shows; none after
# 60 s.
waiting() {
  local i=0 pid
  while [ "$i" -lt 600 ]; do
    pid="$(pgrep -o -f -- "rnk-userns-reach-wait" || true)"
    if [ -n "$pid" ] && sudo test -e "/proc/$pid/root/tmp/wait-reach"; then
      echo "$pid"
      return 0
    fi
    sleep 0.1
    i=$((i + 1))
  done
  return 1
}

# reach PID UID GID: what of the process PID a host process of UID and GID,
# with no supplementary group, reads: its environment, root, descriptors,
# working directory.
reach() {
  local r="" as=(sudo setpriv --reuid="$2" --regid="$3" --clear-groups --)
  "${as[@]}" cat "/proc/$1/environ" >/dev/null 2>&1 && r+=" environ"
  "${as[@]}" ls "/proc/$1/root/" >/dev/null 2>&1 && r+=" root"
  "${as[@]}" ls "/proc/$1/fd/" >/dev/null 2>&1 && r+=" fd"
  "${as[@]}" ls "/proc/$1/cwd/" >/dev/null 2>&1 && r+=" cwd"
  echo "${r:- nothing}"
}

# control_reach UID GID: what a host process of UID and GID reaches of a
# process of its own ids, so that " nothing" from reach means the kernel
# refused, not that setpriv failed.
control_reach() {
  local pid r i=0
  # From /, which every uid may list: the control reaches its cwd.
  (cd / && exec sudo timeout 30 setpriv --reuid="$1" --regid="$2" --clear-groups -- sleep 1235.5) &
  while [ "$i" -lt 50 ]; do
    pid="$(pgrep -o -x -f 'sleep 1235.5' || true)"
    [ -n "$pid" ] && [ "$(awk '/^Uid:/ { print $2 }' "/proc/$pid/status" 2>/dev/null)" = "$1" ] &&
      break
    pid=""
    sleep 0.1
    i=$((i + 1))
  done
  if [ -n "$pid" ]; then
    r="$(reach "$pid" "$1" "$2")"
    sudo kill "$pid" || true
  fi
  wait "$!" 2>/dev/null || true
  echo "${r:- no process}"
}

# urun NAME ARG...: ranikust as root with --userns, its working directory,
# its context and its outputs ($UW/out/NAME, the range's first ids')
# where those ids reach them; prints the log, returns the exit code.
urun() {
  local o="$UW/out/$1"
  shift
  sudo rm -rf "$o" && sudo mkdir -m 700 "$o" && sudo chown "$S:$G" "$o"
  (cd "$UW/ctx" && sudo "$RNK" --userns --context "$UW/ctx" --no-push \
    --build-arg "BASE=$BASE" --oci-layout-path "$o/oci" --tar-path "$o/image.tar" "$@")
}

# clean_up: no RUN left waiting, and the id files as they were.
clean_up() {
  local p
  for p in $(pgrep -f -- 'rnk-userns-reach-wait' || true); do
    sudo rm -f "/proc/$p/root/tmp/wait-reach" || true
  done
  wait || true
  for f in subuid subgid; do
    sudo cp -p "$UW/$f.orig" "/etc/$f" || echo "error: $CASE: /etc/$f not restored" >&2
  done
}
