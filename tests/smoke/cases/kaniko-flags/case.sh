# The kaniko-mode flags tested for their meaning (Phase 2 criterion 8):
# --snapshot-mode, --ignore-path and, in the T1 kaniko leg (a default
# container where root may not mount), --mode=auto picking kaniko mode. In
# overlay mode --ignore-path is refused (ADR-0049) and --snapshot-mode
# changes nothing.

kaniko_leg() { [[ " ${MODE_ARGS[*]} " == *" --mode=kaniko "* ]]; }

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
  fi
}
