case_run() {
  build img >/dev/null
  expect "$(run img)" "ok" "checks inside the image"
}
