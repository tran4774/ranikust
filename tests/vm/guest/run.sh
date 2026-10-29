#!/usr/bin/env bash
# Runs in the T2 VM: records the environment, then each suite given as an
# argument. Each suite's output goes to ~/rnk/out/<suite>.log; the status of
# every suite to ~/rnk/out/summary.txt.
set -uo pipefail
cd "$HOME/rnk"
mkdir -p out

{
  echo "kernel: $(uname -r)"
  echo "os: $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "docker: $(sudo docker version --format '{{.Server.Version}}')"
  echo "cgroup: $(stat -fc %T /sys/fs/cgroup)"
  echo "apparmor: $(cat /sys/module/apparmor/parameters/enabled 2>/dev/null)"
  echo "cpus: $(nproc)"
} | tee out/env.txt

status=0
: >out/summary.txt
for suite in "$@"; do
  echo "== suite $suite"
  if bash "guest/$suite.sh" 2>&1 | tee "out/$suite.log"; then
    echo "$suite: ok" >>out/summary.txt
  else
    echo "$suite: FAILED" >>out/summary.txt
    status=1
  fi
done
echo "== summary"
cat out/summary.txt
exit "$status"
