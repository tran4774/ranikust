# Red team of rootless overlay mode (#79), Podman only (T1, both legs).
# A RUN with --allow proc.rbind writes no kernel, cgroup or sysfs file,
# reaches no process outside its PID namespace (T17, T21, T24), with
# --network=none sees no network device outside its own, leaves no
# tree past PATH_MAX in the state directory (T15) and no process running
# (T8), and its exit 75 fails the build without a fallback (ADR-0136). The
# VM's smoke suite builds `writes` and `deep-cache` as a regular user too
# (tests/vm/guest/smoke.sh). A PID namespace shared with another
# container's process is refused, podman's --init is not (ADR-0129,
# ADR-0134). ranikust killed during set-up leaves nothing that stops the
# next build. In the kaniko leg, the fallback's RUN runs in the
# container's user namespace, not the build's; ranikust killed while a
# set-up made to fail falls back leaves nothing that stops the next
# fallback; and a probe directory a killed kaniko-mode RUN planted in the
# state directory is removed without following its links (T4, T21).

# state NAME: a state directory of this case's, kept for the next run.
state() {
  WORK="$CASES_OUT/.rootless-reach-$1"
  mkdir -p "$WORK" && chmod 700 "$WORK"
}

# The containers this case starts besides ranikust's, one that joined
# another's PID namespace first: podman removes neither in one call.
PEERS=()
peers_down() {
  local c
  for c in ${PEERS[@]+"${PEERS[@]}"}; do podman rm -f -t0 "$c" >/dev/null 2>&1 || :; done
  PEERS=()
}

case_run() {
  [ "$ENGINE" = podman ] || return 0
  local m=() a
  for a in ${MODE_ARGS[@]+"${MODE_ARGS[@]}"}; do [ "$a" = --mode=kaniko ] || m+=("$a"); done
  MODE_ARGS=(${m[@]+"${m[@]}"})
  PODMAN_ARGS=()
  trap peers_down EXIT
  state user
  writes
  deep_tree
  left_behind
  exit_75
  shared_pid_namespaces
  killed_in_set_up
  in_fallback
  killed_in_fallback
  planted
}

# A RUN's writes and reach: every attempt refused, the controls done.
writes() {
  build writes --target writes --allow proc.rbind >/dev/null
  grep -q 'proc: a read-only rbind' "$(out writes)/build.log" || fail "no rbind of /proc"
  local report
  report="$(run writes cat /report)"
  ! grep -Eq '^(write [^/]*/(proc|sys)/.*|mkdir cgroup|mount proc)=done$' <<<"$report" ||
    fail "a RUN wrote a kernel interface: $report"
  ! grep -q '^reached ' <<<"$report" || fail "a RUN reached a process outside: $report"
  [ "$(grep -c '=refused$' <<<"$report")" = 12 ] || fail "not every write refused: $report"
  grep -qx 'cgroup 0::/' <<<"$report" || fail "a RUN sees a cgroup outside its own: $report"
  grep -q '^control reach=done ' <<<"$report" && grep -qx 'write /tmp/control=done' <<<"$report" ||
    fail "the controls: $report"
  # A new sysfs of its own shows lo, the rbind no device.
  grep -Eqx 'none netdevs=(lo )?' <<<"$report" &&
    grep -Eqx 'none addresses=(00:00:00:00:00:00 )?' <<<"$report" ||
    fail "a RUN with --network=none sees a network device outside: $report"
}

# A tree deeper than PATH_MAX that a RUN makes, partly a mapped user's and
# unsearchable: the exporter refuses it, naming PATH_MAX, or exports it,
# and the build's directories go either way (T15).
deep_tree() {
  local o log rc=0 left
  o="$(out deep)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target deep \
    --allow proc.rbind 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  [ "$rc" = 0 ] || { [ "$rc" = 1 ] && grep -q 'RUN mkdir /r && sh /deep.sh /r: encoding a layer: cannot export .*: an entry.s path is longer than PATH_MAX' <<<"$log"; } ||
    fail "a deep tree: neither exported nor refused by the exporter ($rc): $log"
  left="$(find "$WORK/builds" "$WORK/tmp" -mindepth 1 -maxdepth 1 2>/dev/null)"
  [ -z "$left" ] || fail "a deep tree: left in the state directory: $left"
}

# Processes a RUN leaves: none writes after it (the build's own check), none
# is left once ranikust exits.
left_behind() {
  local o got
  o="$(out left)"
  rm -rf "$o" && mkdir -p "$o"
  got="$(rnk_run_sh "$(foreign_script)"'
rc=0
"$0" "$@" >/dev/null 2>&1 || rc=$?
echo "status=$rc"
i=0
while left="$(foreign)" && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
[ -z "$left" ] || echo "left: process $left"' "$SMOKE_CASES_DIR/$CASE" "$o" \
    --build-arg "BASE=$BASE" --target left --allow proc.rbind 2>&1)"
  printf '%s\n' "$got" >"$o/left.log"
  grep -qx 'status=0' <<<"$got" || fail "processes left: the build: $got"
  ! grep -q '^left: ' <<<"$got" || fail "processes left after the build: $got"
}

# foreign_script: shell defining `foreign`, which prints a process of
# another user namespace than the script's.
foreign_script() {
  cat <<'EOF'
read -r own </proc/self/uid_map
foreign() {
  for p in /proc/[0-9]*; do
    m=
    { read -r m || :; } 2>/dev/null <"$p/uid_map" || continue
    [ "$m" = "$own" ] && continue
    c=
    read -r c 2>/dev/null <"$p/comm" || :
    echo "${p#/proc/} $c"
    return 0
  done
  return 1
}
EOF
}

# A RUN's exit 75, the status the set-up's child falls back with, is the
# build's failure: status 1 and no kaniko mode (ADR-0136).
exit_75() {
  local o log rc=0
  o="$(out exit75)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target exit75 \
    --allow proc.rbind 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  expect "$rc" 1 "a RUN's exit 75"
  ! grep -q 'kaniko mode' <<<"$log" || fail "a RUN's exit 75 fell back: $log"
}

# refused NAME WHAT LOG RC: the build NAME was refused for a PID namespace
# it shares with another mount namespace's process, before any image.
refused() {
  printf '%s\n' "$3" >"$(out "$1")/build.log"
  expect "$4" 1 "$2"
  grep -q '(--allow proc.rbind) sharing its PID namespace with ' <<<"$3" ||
    fail "$2: not refused for a shared PID namespace: $3"
  [ ! -e "$(out "$1")/image.tar" ] || fail "$2: an image was written"
}

# PID namespaces shared with other containers, each refused with the
# rbind: joining a root peer's or a regular user's, and ranikust started
# by `podman exec` where a peer joined (ADR-0129, ADR-0134). podman's
# --init, PID 1 of the container's own mount namespace, builds.
shared_pid_namespaces() {
  local o log rc who
  PEERS=("rnk-reach-root-$$" "rnk-reach-user-$$")
  podman run -d --name "rnk-reach-root-$$" "$BASE" sleep 600 >/dev/null
  podman run -d --name "rnk-reach-user-$$" --user 1000:1000 "$BASE" sleep 600 >/dev/null
  for who in root user; do
    o="$(out "pid-$who")" rc=0
    rm -rf "$o" && mkdir -p "$o"
    PODMAN_ARGS=(--pid "container:rnk-reach-$who-$$")
    log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target userns \
      --allow proc.rbind 2>&1)" || rc=$?
    PODMAN_ARGS=()
    refused "pid-$who" "--pid container: a $who peer" "$log" "$rc"
  done
  # A regular user's process, which ranikust may not open, is PID 1 there.
  grep -q 'sharing its PID namespace with PID 1, process 1 (sleep)' "$(out pid-user)/build.log" ||
    fail "--pid container: a user peer: not the PID 1 check's refusal"
  peers_down
  # ranikust in A's mount namespace, A's PID 1 too; B, root, joined A's.
  local a="rnk-reach-a-$$" b="rnk-reach-b-$$"
  PEERS=("$b" "$a")
  o="$(out pid-exec)" rc=0
  rm -rf "$o" && mkdir -p "$o"
  podman run -d --name "$a" -v "$RNK:$IN/bin/ranikust:ro" \
    -v "$SMOKE_CASES_DIR/$CASE:$IN/ctx:ro" -v "$WORK:/var/lib/ranikust" -v "$o:$IN/out" \
    "$BASE" sleep 600 >/dev/null
  podman run -d --name "$b" --pid "container:$a" "$BASE" sleep 601 >/dev/null
  log="$(podman exec "$a" "$IN/bin/ranikust" --context "$IN/ctx" --no-push \
    --tar-path "$IN/out/image.tar" --build-arg "BASE=$BASE" --target userns \
    --allow proc.rbind 2>&1)" || rc=$?
  peers_down
  refused pid-exec "ranikust exec'd where a root peer joined" "$log" "$rc"
  grep -q 'with process [0-9]* (sleep), of another mount namespace' <<<"$log" ||
    fail "ranikust exec'd where a root peer joined: not the peer scan's refusal: $log"
  PODMAN_ARGS=(--init)
  build init --target userns --allow proc.rbind >/dev/null
  PODMAN_ARGS=()
  grep -q 'proc: a read-only rbind' "$(out init)/build.log" || fail "--init: no rbind of /proc"
}

# ranikust sent SIGKILL, SIGTERM or SIGINT at points of its set-up: the
# next build succeeds and removes what they left (ADR-0136).
killed_in_set_up() {
  state sweep
  rm -rf "$WORK" && mkdir -p "$WORK" && chmod 700 "$WORK"
  local o got
  o="$(out sweep)"
  rm -rf "$o" && mkdir -p "$o"
  got="$(rnk_run_sh "$(foreign_script)"'
for sig in KILL TERM INT; do
  for d in 0.005 0.02 0.05 0.1 0.2 0.4; do
    "$0" "$@" >/dev/null 2>&1 &
    pid=$!
    sleep "$d"
    kill -"$sig" "$pid" 2>/dev/null
    wait "$pid"
  done
done
i=0
while foreign >/dev/null && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
rc=0
"$0" "$@" >/dev/null 2>&1 || rc=$?
echo "status=$rc"
s=/var/lib/ranikust
echo "left: $(find "$s/builds" "$s/tmp" -mindepth 1 -maxdepth 1 2>/dev/null; find "$s" -maxdepth 1 -name ".ranikust-anon-userns-probe-*")"' \
    "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target userns \
    --allow proc.rbind 2>&1)"
  printf '%s\n' "$got" >"$o/sweep.log"
  grep -qx 'status=0' <<<"$got" || fail "the build after those killed in set-up: $got"
  grep -qx 'left: ' <<<"$got" || fail "left by builds killed in set-up: $got"
}

# Kaniko leg only, where kaniko mode on / finds the binary and the context
# under /ranikust: the fallback's RUN runs in the container's user
# namespace, the rootless build's in another (ADR-0136).
in_fallback() {
  [ "${RANIKUST_CASE_MODE:-}" = kaniko ] || return 0
  state fallback
  rm -rf "$WORK" && mkdir -p "$WORK" && chmod 700 "$WORK"
  local own
  own="$(podman run --rm "$BASE" sh -c 'cat /proc/self/uid_map; readlink /proc/self/ns/user')"
  PODMAN_ARGS=(-e RANIKUST_DEBUG_FAULT=rootless-broken)
  build fallback --target userns --allow proc.rbind >/dev/null
  PODMAN_ARGS=()
  grep -q 'ranikust: kaniko mode on /, RUN directly' "$(out fallback)/build.log" ||
    fail "the hook: no fallback to kaniko mode"
  expect "$(run fallback cat /userns)" "$own" "the fallback's RUN: its user namespace"
  state user
  build userns --target userns --allow proc.rbind >/dev/null
  [ "$(run userns cat /userns)" != "$own" ] ||
    fail "the rootless build's RUN ran in the container's user namespace"
}

# Kaniko leg only: ranikust sent SIGKILL, SIGTERM or SIGINT at points of
# a set-up made to fail (RANIKUST_DEBUG_FAULT=rootless-broken), before,
# during and after its store's open and abandon, leaves no store that
# stops the next fallback: it builds in kaniko mode and leaves the state
# directory with no store marker (ADR-0136).
killed_in_fallback() {
  [ "${RANIKUST_CASE_MODE:-}" = kaniko ] || return 0
  state fallback-sweep
  rm -rf "$WORK" && mkdir -p "$WORK" && chmod 700 "$WORK"
  local o got
  o="$(out fallback-sweep)"
  rm -rf "$o" && mkdir -p "$o"
  PODMAN_ARGS=(-e RANIKUST_DEBUG_FAULT=rootless-broken)
  got="$(rnk_run_sh "$(foreign_script)"'
for sig in KILL TERM INT; do
  for d in 0.005 0.01 0.02 0.03 0.04 0.05 0.07 0.1 0.15; do
    "$0" "$@" >/dev/null 2>&1 &
    pid=$!
    sleep "$d"
    kill -"$sig" "$pid" 2>/dev/null
    wait "$pid"
  done
done
i=0
while foreign >/dev/null && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
rc=0
"$0" "$@" >/tmp/last.log 2>&1 || rc=$?
echo "status=$rc"
grep -q "ranikust: kaniko mode on /, RUN directly" /tmp/last.log && echo "fell back"
echo "left: $(ls -A /var/lib/ranikust | tr "\n" " ")"
[ "$rc" = 0 ] || cat /tmp/last.log' \
    "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target userns \
    --allow proc.rbind 2>&1)"
  PODMAN_ARGS=()
  printf '%s\n' "$got" >"$o/sweep.log"
  grep -qx 'status=0' <<<"$got" && grep -qx 'fell back' <<<"$got" ||
    fail "the fallback after those killed in one: $got"
  ! grep -Eq '^left: .*\.ranikust-(state|pruning)' <<<"$got" ||
    fail "a store left by fallbacks killed: $got"
}

# Kaniko leg only: a kaniko-mode build killed during a RUN that planted a
# probe directory in the state directory, linking out, and deleted the
# build's marker; the rootless build after it removes the directory, and
# nothing its links name.
planted() {
  [ "${RANIKUST_CASE_MODE:-}" = kaniko ] || return 0
  state plant
  rm -rf "$WORK" && mkdir -p "$WORK" && chmod 700 "$WORK"
  local o got
  o="$(out plant)"
  rm -rf "$o" && mkdir -p "$o"
  echo keep >"$o/canary"
  got="$(rnk_run_sh '
"$0" "$@" --mode=kaniko --target plant >/dev/null 2>&1 &
pid=$!
i=0
until pgrep -x sleep >/dev/null || [ "$i" -gt 300 ]; do sleep 0.1; i=$((i + 1)); done
kill -9 "$pid"
wait "$pid"
echo "killed=$?"
pkill -9 -x sleep
rc=0
"$0" "$@" --target userns --allow proc.rbind >/ranikust/out/after.log 2>&1 || rc=$?
echo "status=$rc"
echo "probe: $(ls -A /var/lib/ranikust | grep "^\.ranikust-anon-userns-probe-")"' \
    "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" 2>&1)"
  printf '%s\n' "$got" >"$o/plant.log"
  grep -qx 'killed=137' <<<"$got" || fail "the planting build was not killed during its RUN: $got"
  grep -qx 'probe: ' <<<"$got" || fail "the planted probe directory was left: $got"
  [ "$(cat "$o/canary" 2>/dev/null)" = keep ] || fail "a planted link was followed: the canary went"
  grep -q 'removed /var/lib/ranikust/.ranikust-anon-userns-probe-1-1 left by an earlier build' \
    "$o/after.log" || fail "the planted directory: no removal logged: $(cat "$o/after.log")"
}
