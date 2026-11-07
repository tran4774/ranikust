# The kaniko-mode flags tested for their meaning (Phase 2 criterion 8):
# --snapshot-mode, --ignore-path and, in the T1 kaniko leg (a default
# container where root may not mount), --mode=auto picking kaniko mode. In
# overlay mode --ignore-path is refused (ADR-0049) and --snapshot-mode
# changes nothing. A --mode=auto build also logs the user namespace probe
# (#74): printed here; in the T1 kaniko leg (rootless Podman's defaults),
# where it routes (#79), it stops at proc, whose rbind would work but needs
# --allow proc.rbind, and kaniko mode builds. There --ignore-path keeps
# kaniko mode, which honours it, even with --allow proc.rbind.

kaniko_leg() { [[ " ${MODE_ARGS[*]} " == *" --mode=kaniko "* ]]; }

# userns_line NAME: prints the user namespace probe's line of the build
# NAME, which must have exactly one, on stdout and stderr.
userns_line() {
  local line n
  line="$(grep -Eo 'user namespace probe( \(reported only\))?: .*' "$(out "$1")/build.log")" ||
    fail "build $1 logged no user namespace probe" || return 1
  n="$(wc -l <<<"$line")"
  [ "$n" = 1 ] || fail "build $1 logged $n user namespace probe lines" || return 1
  echo "$CASE: $line" >&2
  printf '%s\n' "$line"
}

case_run() {
  if ! kaniko_leg; then
    local o log rc=0
    o="$(out refused)"
    rm -rf "$o" && mkdir -p "$o"
    log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
      '--ignore-path=/var/cache/*' 2>&1)" || rc=$?
    expect "$rc" 2 "overlay mode refuses --ignore-path"
    grep -q 'kaniko mode only' <<<"$log" || fail "refusal of --ignore-path: $log"
    build time --snapshot-mode=time >/dev/null
    expect "$(run time)" BBBB "overlay mode ignores --snapshot-mode"
    userns_line time >/dev/null
    return 0
  fi
  build full >/dev/null
  expect "$(run full)" BBBB "full catches a rewrite of the same size and mtime"
  expect "$(run full cat /var/cache/app/f)" cached "without --ignore-path"
  build time --snapshot-mode=time >/dev/null
  expect "$(run time)" AAAA "time misses that rewrite, as kaniko's"
  build ignored '--ignore-path=/var/cache/*' >/dev/null
  expect "$(run ignored sh -c 'test ! -e /var/cache/app && cat /opt/kept')" kept \
    "--ignore-path=/var/cache/* keeps /var/cache/app out of every layer"
  if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
    # No --mode: in a default container root may not mount, so --mode=auto
    # picks kaniko mode and says why (review finding #2).
    local m=() a
    for a in "${MODE_ARGS[@]}"; do [ "$a" = --mode=kaniko ] || m+=("$a"); done
    MODE_ARGS=(${m[@]+"${m[@]}"})
    build auto >/dev/null
    grep -q 'kaniko mode, because root may not mount here' "$(out auto)/build.log" ||
      fail "--mode=auto did not pick kaniko mode"
    expect "$(run auto)" BBBB "the --mode=auto build"
    local line
    line="$(userns_line auto)"
    [[ "$line" == *"probe: stops at proc; "* && "$line" == *"proc: rbind of /proc"* ]] ||
      fail "the user namespace probe should stop at proc, with an rbind of /proc: $line"
    grep -q 'rootless overlay mode cannot run here: the user namespace probe stops at proc' \
      "$(out auto)/build.log" || fail "--mode=auto did not say why rootless overlay mode did not run"
    # Rootless overlay mode, which the flag lets run here, would refuse
    # --ignore-path: kaniko mode builds it, as before rootless overlay mode.
    build auto-ignored '--ignore-path=/var/cache/*' --allow proc.rbind >/dev/null
    grep -q 'rootless overlay mode is not tried: --ignore-path' "$(out auto-ignored)/build.log" ||
      fail "--mode=auto with --ignore-path did not say why it kept kaniko mode"
    grep -q 'kaniko mode, because root may not mount here' "$(out auto-ignored)/build.log" ||
      fail "--mode=auto with --ignore-path did not pick kaniko mode"
    expect "$(run auto-ignored sh -c 'test ! -e /var/cache/app && cat /opt/kept')" kept \
      "--mode=auto with --ignore-path=/var/cache/*"
  fi
}
