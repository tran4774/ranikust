# --seccomp (ADR-0103, ADR-0109): a RUN in the full sandbox runs under
# Docker's default profile, one seccomp filter more than with
# --seccomp=unconfined, with or without --seccomp=default; a profile file's
# rule takes effect. In the T1 kaniko leg, on the container's `/`, every
# --seccomp is refused (exit 2) and a RUN runs under the container's own
# profile.

case_run() {
  if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
    case_run_on_slash
    return
  fi
  local n
  build default >/dev/null
  n="$(run default cat /filters)"
  [ -n "$n" ] && [ "$n" -ge 1 ] || fail "filters under the default profile: $n"
  expect "$(run default cat /nice)" ok "setpriority under the default profile"
  build flag --seccomp=default >/dev/null
  expect "$(run flag cat /filters)" "$n" "--seccomp=default is the default"
  build unconfined --seccomp=unconfined >/dev/null
  expect "$(run unconfined cat /filters)" "$((n - 1))" "--seccomp=unconfined drops the profile"
  build file "--seccomp=$(rnk_ctx "$SMOKE_CASES_DIR/$CASE")/profile.json" >/dev/null
  expect "$(run file cat /filters)" "$n" "a profile file in the default's place"
  expect "$(run file cat /nice)" refused "the profile file refuses setpriority"
}

case_run_on_slash() {
  local v o log rc own
  for v in default unconfined; do
    o="$(out "refused-$v")"
    rm -rf "$o" && mkdir -p "$o"
    rc=0
    log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" "--seccomp=$v" 2>&1)" ||
      rc=$?
    expect "$rc" 2 "--seccomp=$v on /"
    grep -qF 'kaniko mode on / runs RUN directly' <<<"$log" || fail "--seccomp=$v on /: $log"
  done
  build direct >/dev/null
  own="$($ENGINE run --rm "$BASE" sed -n 's/^Seccomp_filters:[[:space:]]*//p' /proc/self/status)"
  expect "$(run direct cat /filters)" "$own" "a RUN on / under the container's own profile"
  expect "$(run direct cat /nice)" ok "setpriority under the container's own profile"
}
