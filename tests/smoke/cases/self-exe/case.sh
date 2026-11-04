# Pins a barrier of the open `#!/proc/self/exe` question (Q-422): the
# script's interpreter is ranikust's own binary, whose command line refuses
# the script path as a subcommand and exits with a usage error, so the
# build fails without starting a build inside the RUN.
case_run() {
  local o rc=0 log
  o="$(out img)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
    --destination "localhost/rnk-case:$CASE-img" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  [ "$rc" != 0 ] || fail "the build succeeded" || return 1
  grep -qF "unrecognized subcommand '/x'" <<<"$log" || {
    echo "$log" >&2
    fail "no usage error from ranikust's binary run as the interpreter"
  }
}
