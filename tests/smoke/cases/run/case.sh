case_run() {
  build first >/dev/null
  expect "$(run first)" "shell exec heredoc shebang " "output"
  smoke_check_no_empty_layer "$(out first)/oci"
  # The second build sees what the first left in the cache mount; the
  # first saw at most marks of earlier runs on the same workdir.
  local before after
  before="$(run first cat /seen | wc -l)"
  build second >/dev/null
  after="$(run second cat /seen | wc -l)"
  expect "$after" "$((before + 1))" "marks the second build saw in the cache"
}
