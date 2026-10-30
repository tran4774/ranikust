case_run() {
  build img >/dev/null
  expect "$(run img)" "hello from ranikust" "output"
}
