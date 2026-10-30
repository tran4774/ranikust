case_run() {
  build img >/dev/null
  expect "$(run img)" "ep cmd" "output"
  expect_config img '{
    "config": {
      "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
              "APP=/opt/app", "VER=v1.2"],
      "WorkingDir": "/opt/app/sub",
      "User": "usr:grp",
      "Labels": {"org.example.name": "meta", "org.example.ver": "v1.2"},
      "ExposedPorts": {"8080/tcp": {}, "53/udp": {}, "9000/tcp": {}, "9001/tcp": {}},
      "StopSignal": "SIGQUIT",
      "Healthcheck": {"Test": ["CMD", "true"], "Interval": 30000000000,
                      "Timeout": 3000000000, "Retries": 2},
      "Shell": ["/bin/sh", "-ec"],
      "Entrypoint": ["/bin/echo", "ep"],
      "Cmd": ["cmd"]
    }
  }'

  build shellform --target shellform >/dev/null
  expect "$(run shellform)" "shell-entry" "shell-form ENTRYPOINT output"
  expect_config shellform '{"config": {"Entrypoint": ["/bin/sh", "-c", "echo shell-entry"]}}'
  # The CMD of the base stage is gone, not inherited.
  config shellform | python3 -Ic 'import json, sys; c = json.load(sys.stdin)["config"]; sys.exit("Cmd" in c)' ||
    fail "shellform kept the inherited CMD"
}
