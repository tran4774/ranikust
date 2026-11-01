case_run() {
  build img --jobs 2 >/dev/null
  expect "$(run img)" "a b c " "output"
  if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
    # On a container's / the stages run one at a time; --jobs still bounds
    # the uploads, and the warning says so (review finding #6).
    grep -q -- '--jobs still bounds the uploads' "$(out img)/build.log" ||
      fail "no --jobs warning in kaniko mode on /"
  fi
}
