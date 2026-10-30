case_run() {
  build img >/dev/null
  expect "$(run img)" "static" "output"
  expect_config img '{
    "config": {
      "Entrypoint": ["/bin/busybox", "cat", "/hello"],
      "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]
    }
  }'
}
