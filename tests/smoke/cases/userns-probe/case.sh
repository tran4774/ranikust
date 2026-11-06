# Red team of the user namespace probe of --mode=auto (#74; T4, T8, T9):
# one probe line per build, and its RUN sees nothing of it (seen.sh). On
# T1 also: RANIKUST_DEBUG_FAULT=overlay-refused, a state directory on
# tmpfs, and ranikust killed mid-probe; what each step checks is in
# docs/testing.md (`userns-probe`).

# The probe runs with --mode=auto only.
auto_only() {
  local m=() a
  for a in ${MODE_ARGS[@]+"${MODE_ARGS[@]}"}; do [ "$a" = --mode=kaniko ] || m+=("$a"); done
  MODE_ARGS=(${m[@]+"${m[@]}"})
}

kaniko_leg() { [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; }

# one_probe_line WHAT LOG: LOG holds exactly one probe line; printed.
one_probe_line() {
  local n
  n="$(grep -c 'user namespace probe (reported only)' <<<"$2" || true)"
  expect "$n" 1 "$1: user namespace probe lines" || return 1
  echo "  $CASE: $1: $(grep -o 'user namespace probe (reported only).*' <<<"$2")" >&2
}

# mode_line LOG: the mode the build picked, up to the first comma.
mode_line() { grep -Eo 'ranikust: (overlay|kaniko) mode[^,]*' <<<"$1" | head -1; }

case_run() {
  auto_only
  local log
  build auto >/dev/null
  log="$(cat "$(out auto)/build.log")"
  one_probe_line auto "$log"
  expect "$(run auto)" "seen: nothing" "what a RUN sees of the probe"
  [ "$ENGINE" = podman ] || return 0
  ( fault_hook )
  ( tmpfs_state "$(mode_line "$log")" )
  local how
  for how in KILL INT group; do ( killed_mid_probe "$how" ); done
  # The killed builds' directories are gone once a build has started after.
  build after >/dev/null
  local left
  left="$(find "$WORK" -maxdepth 3 \( -name userns-probe -o -name '.ranikust-anon-*' \) 2>/dev/null)"
  [ -z "$left" ] || fail "probe directories left in the state directory: $left"
}

# RANIKUST_DEBUG_FAULT=overlay-refused: where root may mount (--privileged),
# overlay mode is refused in a container, with no image; where it may not,
# kaniko mode on / builds as without it, and the RUN sees no variable.
fault_hook() {
  PODMAN_ARGS+=(-e RANIKUST_DEBUG_FAULT=overlay-refused)
  local log
  if kaniko_leg; then
    build fault >/dev/null
    log="$(cat "$(out fault)/build.log")"
    grep -q 'kaniko mode, because root may not mount here' <<<"$log" ||
      fail "the hook changed the mode where root may not mount"
    one_probe_line fault "$log"
    expect "$(run fault)" "seen: nothing" "a RUN of a build with the hook set"
    return 0
  fi
  local o rc=0
  o="$(out fault)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  expect "$rc" 1 "the hook in a container where root may mount"
  grep -q 'refused by RANIKUST_DEBUG_FAULT=overlay-refused' <<<"$log" &&
    grep -q 'overlay mode only (ADR-0054)' <<<"$log" ||
    fail "the hook in a container: not refused for overlay mode: $log"
  one_probe_line fault "$log"
  [ ! -e "$o/image.tar" ] && [ ! -e "$o/oci/index.json" ] ||
    fail "the hook in a container: an image was written"
}

# A state directory on tmpfs lands in the mode of the volume ($1).
tmpfs_state() {
  PODMAN_ARGS+=(--tmpfs "$IN/tstate:rw,mode=0700")
  build tmpfs --state-dir "$IN/tstate" >/dev/null
  local log
  log="$(cat "$(out tmpfs)/build.log")"
  expect "$(mode_line "$log")" "$1" "the mode with the state directory on tmpfs"
  one_probe_line tmpfs "$log"
  expect "$(run tmpfs)" "seen: nothing" "a RUN with the state directory on tmpfs"
}

# killed_mid_probe HOW: ranikust is sent SIGKILL (KILL), SIGINT (INT) or
# SIGINT to its process group (group) as soon as a process in another user
# namespace shows (one of the probe's), up to 8 tries; then no such
# process and no probe mount are left. One try must catch a probe child.
# In the kaniko leg the next build in that state directory removes the
# probe's directory.
killed_mid_probe() {
  local o got st=()
  o="$(out "kill-$1")"
  rm -rf "$o" && mkdir -p "$o"
  # A build cut short on / marks its state directory for later builds
  # (ADR-0066): each round gets one of its own, never $WORK, below a
  # directory no other user may write.
  if kaniko_leg; then
    chmod 700 "$o"
    st=(--state-dir "$IN/out/state")
  fi
  got="$(rnk_run_sh "how=$1 at=$IN/out
$(kill_script)" "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" ${st[@]+"${st[@]}"} 2>&1)"
  printf '%s\n' "$got" >"$o/kill.log"
  echo "  $CASE: killed mid-probe ($1): $(grep '^tries=' <<<"$got")" >&2
  ! grep -q '^left: ' <<<"$got" || fail "killed mid-probe ($1): $got"
  grep -q '^tries=[0-9]* caught=1$' <<<"$got" ||
    fail "killed mid-probe ($1): no probe child caught, so nothing was tested: $got"
  kaniko_leg || return 0
  rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" "${st[@]}" >"$o/after.log" 2>&1 ||
    fail "killed mid-probe ($1): the next build failed: $(cat "$o/after.log")"
  local left
  left="$(find "$o/state" -maxdepth 1 -name '.ranikust-anon-*')"
  [ -z "$left" ] || fail "killed mid-probe ($1): left in the state directory: $left"
}

# The script killed_mid_probe runs in the container, with $how and $at
# (a directory kaniko mode on / never empties) set. ranikust runs in the
# foreground, so that SIGINT keeps its default action (a background job
# of a shell ignores it); a background poller sends the signal. Builtins
# only while it polls: kaniko mode on / may replace /bin.
kill_script() {
  cat <<'EOF'
read -r own </proc/self/uid_map
# The first process in another user namespace: its map reads otherwise
# from here, and empty before it is written.
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
alive() { read -r _ _ s _ 2>/dev/null <"/proc/$1/stat" && [ "$s" != Z ]; }
caught=0 tries=0
while [ "$caught" = 0 ] && [ "$tries" -lt 8 ]; do
  tries=$((tries + 1))
  rm -f "$at/pid" "$at/caught"
  (
    while [ ! -s "$at/pid" ]; do :; done
    read -r pid <"$at/pid"
    while alive "$pid"; do
      foreign >/dev/null || continue
      case "$how" in group) kill -INT "-$pid" ;; *) kill -"$how" "$pid" ;; esac
      echo 1 >"$at/caught"
      break
    done
  ) &
  poller=$!
  sh -c 'echo $$ >"$1/pid"; shift; exec setsid "$@"' sh "$at" "$0" "$@" >/dev/null 2>&1
  kill "$poller" 2>/dev/null
  wait "$poller" 2>/dev/null
  [ -s "$at/caught" ] && caught=1
done
echo "tries=$tries caught=$caught"
i=0
while left="$(foreign)" && [ "$i" -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
[ -z "$left" ] || echo "left: process $left"
# The mount point (field 5) only: the context's source path names the case.
awk '$5 ~ /userns-probe|[.]ranikust-anon-/ { print "left: mount " $5 }' /proc/self/mountinfo
rm -f "$at/pid" "$at/caught"
:
EOF
}
