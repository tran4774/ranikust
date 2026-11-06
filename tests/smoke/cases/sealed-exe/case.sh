# Red team (ADR-0101, T7, T21): reach.sh fails the build when a RUN reaches
# ranikust's binary or descriptors.
case_run() {
  build img >/dev/null
  local got
  got="$(run img)"
  grep -Eq '^reach: [1-9][0-9]* threads of ranikust.s out of reach$' <<<"$got" ||
    fail "reach.sh: got $(printf %q "$got")"
}
