#!/usr/bin/env bash
# Runs in the T2 VM: records the environment, then each suite given as an
# argument. Each suite's output goes to ~/rnk/out/<suite>.log; the status of
# every suite to ~/rnk/out/summary.txt, its wall time in seconds to
# ~/rnk/out/suite-times.txt.
set -uo pipefail
cd "$HOME/rnk"
mkdir -p out
# RANIKUST_VM_IMAGE_REGISTRY and RANIKUST_VM_SIZE, from tests/vm/vm.sh.
# shellcheck disable=SC1091
[ -f vm.env ] && source vm.env

{
  echo "kernel: $(uname -r)"
  echo "os: $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "docker: $(sudo docker version --format '{{.Server.Version}}')"
  echo "cgroup: $(stat -fc %T /sys/fs/cgroup)"
  echo "apparmor: $(cat /sys/module/apparmor/parameters/enabled 2>/dev/null)"
  echo "cpus: $(nproc)"
  echo "memory: $(awk '/^MemTotal/ { printf "%d MiB", $2 / 1024 }' /proc/meminfo); swap: $(awk '/^SwapTotal/ { printf "%d MiB", $2 / 1024 }' /proc/meminfo)"
  echo "vm: ${RANIKUST_VM_SIZE:-unknown}"
} | tee out/env.txt

status=0
: >out/summary.txt
: >out/suite-times.txt
for suite in "$@"; do
  echo "== suite $suite"
  t0=$SECONDS
  if bash "guest/$suite.sh" 2>&1 | tee "out/$suite.log"; then
    echo "$suite: ok" >>out/summary.txt
  else
    echo "$suite: FAILED" >>out/summary.txt
    status=1
  fi
  echo "$suite $((SECONDS - t0))" >>out/suite-times.txt
done
echo "== summary"
cat out/summary.txt
exit "$status"
