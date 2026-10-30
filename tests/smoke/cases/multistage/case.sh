case_run() {
  build img --jobs 2 >/dev/null
  expect "$(run img)" "a b c " "output"
}
