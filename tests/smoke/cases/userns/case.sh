# --userns as root (#83, ADR-0003, ADR-0104), T2 only (the VM's quick
# suite): ranikust runs as real root with a `ranikust` range of its own in
# /etc/subuid and /etc/subgid, both restored after. Refused before
# anything is written without the range, with a state directory below a
# directory the range's first uid cannot search, with an existing state
# directory of root's or root's default, and with a flag of kaniko mode
# alone; missing parents are made root's 0711; a RUN's uid 0 is the
# range's first uid on the host, by its uid_map, gid_map and status read
# from the host and by the owner there of a file it makes; the
# --reproducible digest equals the same build's without --userns; and
# ADR-0074's reach of a distinct host account is measured again: neither
# an account with the RUN's own ids nor one with the ids they map to on
# the host reaches it, the latter reaching a process of its own (the
# control). In the kaniko leg, --userns with --mode=kaniko is a usage
# error.

# Below /var/lib, which only root writes and every uid may search.
UW=/var/lib/rnk-quick-userns

case_run() {
  [ "$ENGINE" = "sudo docker" ] || return 0
  if [ "${RANIKUST_QUICK_MODE:-}" = kaniko ]; then
    local log rc=0
    log="$(sudo "$RNK" --userns --mode=kaniko --context "$SMOKE_CASES_DIR/$CASE" --no-push 2>&1)" ||
      rc=$?
    expect "$rc" 2 "--userns with --mode=kaniko"
    grep -qF -- '--userns cannot be used with --mode=kaniko' <<<"$log" ||
      fail "--userns with --mode=kaniko: $log"
    return 0
  fi
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

  local log rc=0
  log="$(urun none --target content --state-dir "$UW/state" 2>&1)" || rc=$?
  expect "$rc" 1 "--userns without a ranikust range"
  grep -qF '/etc/subuid has no ranikust range' <<<"$log" &&
    grep -qF 'ranikust:<start>:65536 to both /etc/subuid and /etc/subgid' <<<"$log" ||
    fail "--userns without a ranikust range: $log"
  ! sudo test -e "$UW/state" || fail "--userns without a range made the state directory"

  # One past every range of both files, uids and gids apart.
  S="$(cat /etc/subuid /etc/subgid | awk -F: 'NF == 3 && $3 > 0 { e = $2 + $3; if (e > m) m = e }
    END { if (m < 1000000) m = 1000000; print int((m + 65535) / 65536) * 65536 }')"
  G=$((S + 65536))
  echo "ranikust:$S:65536" | sudo tee -a /etc/subuid >/dev/null
  echo "ranikust:$G:65536" | sudo tee -a /etc/subgid >/dev/null

  sudo mkdir -m 700 "$UW/closed"
  rc=0
  log="$(urun closed --target content --state-dir "$UW/closed/state" 2>&1)" || rc=$?
  expect "$rc" 1 "--userns below a directory its uid cannot search"
  grep -qF "$UW/closed is not searchable by uid $S and gid $G" <<<"$log" ||
    fail "--userns below a directory its uid cannot search: $log"
  ! sudo test -e "$UW/closed/state" || fail "--userns made a state directory it cannot search"

  sudo mkdir -m 700 "$UW/roots"
  rc=0
  log="$(urun roots --target content --state-dir "$UW/roots" 2>&1)" || rc=$?
  expect "$rc" 1 "--userns with a state directory of root's"
  grep -qF "$UW/roots is owned by uid 0, not $S" <<<"$log" ||
    fail "--userns with a state directory of root's: $log"
  expect "$(sudo stat -c '%u:%g %a' "$UW/roots") $(sudo ls -A "$UW/roots" | wc -l)" "0:0 700 0" \
    "root's state directory after --userns was refused"

  rc=0
  log="$(urun flagged --target content --state-dir "$UW/flagged" --ignore-path /srv 2>&1)" || rc=$?
  expect "$rc" 2 "--userns with a flag of kaniko mode alone"
  grep -qF -- '--ignore-path takes effect in kaniko mode only' <<<"$log" ||
    fail "--userns with a flag of kaniko mode alone: $log"
  ! sudo test -e "$UW/flagged" || fail "--userns made a state directory before its usage error"

  local before
  before="$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)"
  rc=0
  log="$(urun default --target content 2>&1)" || rc=$?
  expect "$rc" 1 "--userns with root's default state directory"
  grep -qF "/var/lib/ranikust is root's default" <<<"$log" ||
    fail "--userns with root's default state directory: $log"
  expect "$(sudo stat -c '%u:%g %a' /var/lib/ranikust 2>/dev/null || echo missing)" "$before" \
    "root's default state directory after --userns was refused"

  # Missing parents are root's, searchable only.
  rc=0
  log="$(urun nested --target content --state-dir "$UW/new/a/state" 2>&1)" || rc=$?
  expect "$rc" 0 "--userns below missing parents: $log"
  expect "$(sudo stat -c '%u:%g %a' "$UW/new" "$UW/new/a" "$UW/new/a/state" | paste -sd ' ')" \
    "0:0 711 0:0 711 $S:$G 700" "the --userns state directory and the parents it made"

  local mapped plain
  mapped="$(udigest userns --reproducible --target content \
    --destination "localhost/rnk-case:$CASE-userns")"
  expect "$(sudo stat -c '%u:%g %a' "$UW/state")" "$S:$G 700" "the state directory --userns made"
  plain="$(build plain --reproducible --target content)"
  expect "$mapped" "$plain" "the --userns digest against the same build's without it"
  sudo docker load -q -i "$UW/out/userns/image.tar" >/dev/null || fail "load the --userns image"
  expect "$(sudo docker run --rm "localhost/rnk-case:$CASE-userns" \
    sh -c 'stat -c "%u:%g %a" /home/u/f /d /bin/suid && readlink /home/u/l')" \
    "$(printf '1234:1234 644\n0:0 2750\n0:0 4755\nf')" "the --userns image's owners and modes"
  seen_from_the_host
}

# seen_from_the_host: the `seen` stage's RUNs, from the host while each
# waits: as root, its maps, ids and a file it made; as USER 1234:1234, the
# reach of a host account with its ids and of one with the ids they map
# to (ADR-0074), none.
seen_from_the_host() {
  local o="$CASES_OUT/$CASE/seen" pid bg rc=0
  rm -rf "$o" && mkdir -p "$o"
  urun seen --target seen --state-dir "$UW/state" >"$o/build.log" 2>&1 &
  bg=$!
  pid="$(waiting root)" || fail "no RUN waited as root: $(cat "$o/build.log")"
  expect "$(awk '{ print $1, $2, $3 }' "/proc/$pid/uid_map")" "0 $S 65536" "the RUN's uid_map"
  expect "$(awk '{ print $1, $2, $3 }' "/proc/$pid/gid_map")" "0 $G 65536" "the RUN's gid_map"
  expect "$(awk '/^(Uid|Gid):/ { print $2, $3, $4, $5 }' "/proc/$pid/status" | paste -sd ' ')" \
    "$S $S $S $S $G $G $G $G" "the RUN's ids on the host"
  expect "$(sudo stat -c '%u:%g' "/proc/$pid/root/made-by-run")" "$S:$G" \
    "the owner on the host of a file the RUN made"
  sudo rm "/proc/$pid/root/tmp/wait-root"

  pid="$(waiting user)" || fail "no RUN waited as USER 1234: $(cat "$o/build.log")"
  expect "$(awk '/^Uid:/ { print $2 }' "/proc/$pid/status")" "$((S + 1234))" \
    "the USER 1234 RUN's uid on the host"
  local own far ctl owner ids
  own="$(reach "$pid" 1234 1234)"
  far="$(reach "$pid" $((S + 1234)) $((G + 1234)))"
  # The kernel shows a process's /proc files as its own only while it is
  # dumpable, else as root of its memory's user namespace.
  owner="$(stat -c '%u:%g' "/proc/$pid/environ")"
  ids="$(awk '/^(Uid|Gid):/ { print $2, $3, $4, $5 }' "/proc/$pid/status" | paste -sd ' ')"
  ctl="$(control_reach $((S + 1234)) $((G + 1234)))"
  echo "  $CASE: measured (ADR-0074): a host account 1234:1234 reaches:$own; one $((S + 1234)):$((G + 1234)) reaches:$far (its own process:$ctl); the RUN's ids $ids, its /proc files owned by $owner" >&2
  expect "$ctl" " environ root" "what a host account reaches of a process of its own (the control)"
  expect "$own" " nothing" "what a host account with the RUN's own ids reaches"
  expect "$far" " nothing" "what a host account with the ids the RUN's map to reaches"
  sudo rm "/proc/$pid/root/tmp/wait-user"
  wait "$bg" || rc=$?
  expect "$rc" 0 "the seen build: $(cat "$o/build.log")"
}

# waiting WHO: the pid of the RUN waiting as WHO (root, user), once its
# file shows; none after 60 s.
waiting() {
  local i=0 pid
  while [ "$i" -lt 600 ]; do
    pid="$(pgrep -o -f -- "rnk-userns-wait-$1" || true)"
    if [ -n "$pid" ] && sudo test -e "/proc/$pid/root/tmp/wait-$1"; then
      echo "$pid"
      return 0
    fi
    sleep 0.1
    i=$((i + 1))
  done
  return 1
}

# reach PID UID GID: what of the process PID a host process of UID and GID,
# with no supplementary group, reads: its environment, its root.
reach() {
  local r="" as=(sudo setpriv --reuid="$2" --regid="$3" --clear-groups --)
  "${as[@]}" cat "/proc/$1/environ" >/dev/null 2>&1 && r+=" environ"
  "${as[@]}" ls "/proc/$1/root/" >/dev/null 2>&1 && r+=" root"
  echo "${r:- nothing}"
}

# control_reach UID GID: what a host process of UID and GID reaches of a
# process of its own ids, so that " nothing" from reach means the kernel
# refused, not that setpriv failed.
control_reach() {
  local pid r i=0
  sudo setpriv --reuid="$1" --regid="$2" --clear-groups -- sleep 1234.5 &
  while [ "$i" -lt 50 ]; do
    pid="$(pgrep -o -x -f 'sleep 1234.5' || true)"
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
# its context and its outputs ($UW/out/NAME, the range's first ids') where
# those ids reach them; prints the log, returns the exit code.
urun() {
  local o="$UW/out/$1"
  shift
  sudo rm -rf "$o" && sudo mkdir -m 700 "$o" && sudo chown "${S:-0}:${G:-0}" "$o"
  # The default --dockerfile is looked for from here first.
  (cd "$UW/ctx" && sudo "$RNK" --userns --context "$UW/ctx" --no-push \
    --build-arg "BASE=$BASE" --oci-layout-path "$o/oci" --tar-path "$o/image.tar" "$@")
}

# udigest NAME ARG...: urun into the state directory $UW/state; prints
# the digest.
udigest() {
  local name="$1" log rc=0
  shift
  log="$(urun "$name" --state-dir "$UW/state" "$@" 2>&1)" || rc=$?
  if [ "$rc" != 0 ] || ! grep -Eq '^sha256:[0-9a-f]{64}$' <<<"$log"; then
    echo "$log" >&2
    fail "--userns build $name exited $rc" || return 1
  fi
  grep -Eo '^sha256:[0-9a-f]{64}$' <<<"$log" | tail -1
}

# clean_up: no RUN left waiting, and the id files as they were.
clean_up() {
  local p
  for p in $(pgrep -f -- 'rnk-userns-wait-' || true); do
    sudo rm -f "/proc/$p/root/tmp/wait-root" "/proc/$p/root/tmp/wait-user" || true
  done
  wait || true
  for f in subuid subgid; do
    sudo cp -p "$UW/$f.orig" "/etc/$f" || echo "error: $CASE: /etc/$f not restored" >&2
  done
}
