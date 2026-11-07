# Rootless overlay mode end to end (#79, ADR-0107, ADR-0126), Podman only
# (T1, both legs): where root may not mount (rootless Podman's defaults), a
# build with --allow proc.rbind runs in overlay mode in the build's user
# namespace, and is refused without the flag, naming it, and with it in the
# host's PID namespace (--pid=host, ADR-0134). Its --reproducible
# digest equals overlay mode as root's (--privileged), also at --jobs 1; a
# file capability a RUN sets is exported; ranikust killed during a RUN
# (SIGKILL, SIGINT, SIGINT to its group) leaves no process, and the next
# build removes the killed builds' directories. In the kaniko leg, a set-up
# that fails in the build's user namespace falls back to kaniko mode
# (ADR-0136). Each mode has a persistent state directory of its own: the
# leg's may be kaniko mode's.

# state NAME: a state directory of this case's, kept for the next run.
state() {
  WORK="$CASES_OUT/.rootless-$1"
  mkdir -p "$WORK" && chmod 700 "$WORK"
}

case_run() {
  [ "$ENGINE" = podman ] || return 0
  local m=() a
  for a in ${MODE_ARGS[@]+"${MODE_ARGS[@]}"}; do [ "$a" = --mode=kaniko ] || m+=("$a"); done
  MODE_ARGS=(${m[@]+"${m[@]}"})
  PODMAN_ARGS=(--privileged)
  state root
  local root
  root="$(build root --reproducible --target content)"
  grep -q 'ranikust: overlay mode as root' "$(out root)/build.log" ||
    fail "--privileged: not overlay mode as root"
  PODMAN_ARGS=()
  state user
  local o log rc=0
  o="$(out refused)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --mode=overlay \
    --target content 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  expect "$rc" 1 "--mode=overlay where a new proc is refused, without --allow proc.rbind"
  grep -q 'or pass --allow proc.rbind, through which every RUN' <<<"$log" ||
    fail "the refusal does not name --allow proc.rbind: $log"
  # The host's PID namespace, whose processes ranikust may not open, is
  # refused with the flag before any RUN (ADR-0134).
  o="$(out pid-host)" rc=0
  rm -rf "$o" && mkdir -p "$o"
  PODMAN_ARGS=(--pid=host)
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target content \
    --allow proc.rbind 2>&1)" || rc=$?
  PODMAN_ARGS=()
  printf '%s\n' "$log" >"$o/build.log"
  expect "$rc" 1 "--pid=host with --allow proc.rbind"
  grep -q "in the host's PID namespace (process 2 (kthreadd) is a kernel thread)" <<<"$log" ||
    fail "--pid=host: not refused as the host's PID namespace: $log"
  [ ! -e "$o/image.tar" ] || fail "--pid=host: an image was written"
  local first one
  first="$(build user --reproducible --target content --allow proc.rbind)"
  grep -q "ranikust: overlay mode, rootless, in the build's user namespace" \
    "$(out user)/build.log" || fail "not rootless overlay mode"
  grep -q 'proc: a read-only rbind' "$(out user)/build.log" || fail "no rbind of /proc"
  one="$(build user-j1 --reproducible --jobs 1 --target content --allow proc.rbind)"
  expect "$first" "$root" "the rootless digest against overlay mode as root's"
  expect "$one" "$first" "the rootless digest at --jobs 1"
  expect "$(run user sh -c 'stat -c "%u:%g %a" /home/u/f /d && readlink /home/u/l')" \
    "$(printf '1234:1234 644\n0:0 2750\nf')" "ownership, a setgid directory and a symlink"
  build caps --target caps --allow proc.rbind >/dev/null
  expect "$(run caps getcap /bin/pinger)" "/bin/pinger cap_net_raw=ep" "the file capability"
  local how
  for how in KILL INT group; do killed_mid_run "$how"; done
  build after --target content --allow proc.rbind >/dev/null
  local left
  left="$(find "$WORK/builds" "$WORK/tmp" -mindepth 1 -maxdepth 1 2>/dev/null)"
  [ -z "$left" ] || fail "left in the state directory after the next build: $left"
  falls_back
}

# falls_back: T1 kaniko leg only, where kaniko mode on / finds the binary
# and the context under /ranikust. A set-up failing in the build's user
# namespace (RANIKUST_DEBUG_FAULT=rootless-broken, once the store is open)
# falls back to kaniko mode for root's --mode=auto, the state directory it
# made a store left empty; --mode=overlay gets the error (ADR-0136).
falls_back() {
  [ "${RANIKUST_CASE_MODE:-}" = kaniko ] || return 0
  state fallback
  rm -rf "$WORK" && mkdir -p "$WORK" && chmod 700 "$WORK"
  PODMAN_ARGS=(-e RANIKUST_DEBUG_FAULT=rootless-broken)
  build fallback --target content --allow proc.rbind >/dev/null
  local log
  log="$(cat "$(out fallback)/build.log")"
  grep -q 'refused by RANIKUST_DEBUG_FAULT=rootless-broken; kaniko mode instead, before anything was built (ADR-0136)' <<<"$log" &&
    grep -q 'ranikust: kaniko mode on /, RUN directly' <<<"$log" ||
    fail "the hook: no fallback to kaniko mode on /: $log"
  expect "$(run fallback sh -c 'stat -c "%u:%g %a" /home/u/f /d && readlink /home/u/l')" \
    "$(printf '1234:1234 644\n0:0 2750\nf')" "the image kaniko mode built after the fallback"
  [ -z "$(ls -A "$WORK")" ] || fail "left in the state directory: $(ls -A "$WORK")"
  local o rc=0
  o="$(out overlay-broken)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --mode=overlay \
    --target content --allow proc.rbind 2>&1)" || rc=$?
  PODMAN_ARGS=()
  printf '%s\n' "$log" >"$o/build.log"
  expect "$rc" 1 "--mode=overlay with the hook"
  grep -q 'rootless overlay mode: refused by RANIKUST_DEBUG_FAULT=rootless-broken' <<<"$log" &&
    ! grep -q 'kaniko mode' <<<"$log" || fail "--mode=overlay with the hook: $log"
  [ ! -e "$o/image.tar" ] || fail "--mode=overlay with the hook: an image was written"
}

# killed_mid_run HOW: ranikust is sent SIGKILL (KILL), SIGINT (INT) or
# SIGINT to its process group (group) once a RUN's `sleep` shows in
# another user namespace; then no process of another user namespace is
# left.
killed_mid_run() {
  local o got
  o="$(out "kill-$1")"
  rm -rf "$o" && mkdir -p "$o"
  got="$(rnk_run_sh "how=$1 at=$IN/out
$(kill_script)" "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" --target slow \
    --allow proc.rbind 2>&1)"
  printf '%s\n' "$got" >"$o/kill.log"
  echo "  $CASE: killed during a RUN ($1): $(grep '^caught=' <<<"$got")" >&2
  ! grep -q '^left: ' <<<"$got" || fail "killed during a RUN ($1): $got"
  grep -q '^caught=1$' <<<"$got" || fail "killed during a RUN ($1): no RUN caught: $got"
}

# The script killed_mid_run runs in the container, with $how and $at set.
# ranikust runs in the foreground, so that SIGINT keeps its default action;
# a background poller sends the signal.
kill_script() {
  cat <<'EOF'
read -r own </proc/self/uid_map
# A process in another user namespace, whose comm is $1 if given.
foreign() {
  for p in /proc/[0-9]*; do
    m=
    { read -r m || :; } 2>/dev/null <"$p/uid_map" || continue
    [ "$m" = "$own" ] && continue
    c=
    read -r c 2>/dev/null <"$p/comm" || :
    [ -z "$1" ] || [ "$c" = "$1" ] || continue
    echo "${p#/proc/} $c"
    return 0
  done
  return 1
}
alive() { read -r _ _ s _ 2>/dev/null <"/proc/$1/stat" && [ "$s" != Z ]; }
rm -f "$at/pid" "$at/caught"
(
  while [ ! -s "$at/pid" ]; do :; done
  read -r pid <"$at/pid"
  while alive "$pid"; do
    foreign sleep >/dev/null || continue
    case "$how" in group) kill -INT "-$pid" ;; *) kill -"$how" "$pid" ;; esac
    echo 1 >"$at/caught"
    break
  done
) &
poller=$!
sh -c 'echo $$ >"$1/pid"; shift; exec setsid "$@"' sh "$at" "$0" "$@" >/dev/null 2>&1
kill "$poller" 2>/dev/null
wait "$poller" 2>/dev/null
if [ -s "$at/caught" ]; then echo caught=1; else echo caught=0; fi
i=0
while left="$(foreign)" && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
[ -z "$left" ] || echo "left: process $left"
rm -f "$at/pid" "$at/caught"
:
EOF
}
