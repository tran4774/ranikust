case_run() {
  local a b c d
  a="$(build first --reproducible)"
  sleep 1
  b="$(build second --reproducible --jobs 1)"
  c="$(build third --reproducible --jobs 2)"
  # The walk's worker count never changes a layer (ADR-0039).
  d="$(build fourth --reproducible --snapshot-workers=1)"
  expect "$b" "$a" "digest one second later at --jobs 1"
  expect "$c" "$a" "digest at --jobs 2"
  expect "$d" "$a" "digest at --snapshot-workers=1"
  expect_config first '{"created": "1970-01-01T00:00:00Z"}'
  # Without --reproducible the build time shows: another digest.
  [ "$(build plain)" != "$a" ] || fail "a build without --reproducible has the reproducible digest"
}
