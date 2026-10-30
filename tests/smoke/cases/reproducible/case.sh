case_run() {
  local a b c
  a="$(build first --reproducible)"
  sleep 1
  b="$(build second --reproducible --jobs 1)"
  c="$(build third --reproducible --jobs 2)"
  expect "$b" "$a" "digest one second later at --jobs 1"
  expect "$c" "$a" "digest at --jobs 2"
  expect_config first '{"created": "1970-01-01T00:00:00Z"}'
  # Without --reproducible the build time shows: another digest.
  [ "$(build plain)" != "$a" ] || fail "a build without --reproducible has the reproducible digest"
}
