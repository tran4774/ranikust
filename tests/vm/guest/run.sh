#!/usr/bin/env bash
# Runs in the T2 VM: records the environment, checks the network, then runs
# each suite given as an argument. Each suite's output goes to
# ~/rnk/out/<suite>.log; the status of every suite to ~/rnk/out/summary.txt,
# its wall time in seconds to ~/rnk/out/suite-times.txt.
#
#   run.sh [suite]...           the network check, then the suites
#   run.sh --after <suite>...   a later step of the same run (tests/vm/vm.sh
#                               runs the push suite so, with the host's
#                               registry gate open): no network check, the
#                               summary and times appended to
#
# Exits 4 when the VM is not the run's own (no suite runs, no network
# check), 3 when the network check fails (no suite runs), 1 when a suite
# fails, else 0: 0 and 1 only after a passed network check (or, with
# --after, none).
set -uo pipefail
cd "$HOME/rnk"
mkdir -p out
# RANIKUST_VM_IMAGE_REGISTRY and RANIKUST_VM_SIZE, from tests/vm/vm.sh.
# shellcheck disable=SC1091
[ -f vm.env ] && source vm.env
after=0
[ "${1:-}" != --after ] || { after=1; shift; }

{
  echo "kernel: $(uname -r)"
  echo "os: $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "docker: $(sudo docker version --format '{{.Server.Version}}')"
  echo "cgroup: $(stat -fc %T /sys/fs/cgroup)"
  echo "apparmor: $(cat /sys/module/apparmor/parameters/enabled 2>/dev/null)"
  echo "cpus: $(nproc)"
  echo "memory: $(awk '/^MemTotal/ { printf "%d MiB", $2 / 1024 }' /proc/meminfo); swap: $(awk '/^SwapTotal/ { printf "%d MiB", $2 / 1024 }' /proc/meminfo)"
  echo "vm: ${RANIKUST_VM_SIZE:-unknown}"
  # What must be this run's own, never the base image's: one login user with
  # one key (this run's), host keys made at this boot, a machine ID of its own.
  echo "users: $(getent passwd | awk -F: '$3 >= 1000 && $3 < 60000 { printf "%s ", $1 }')"
  echo "authorized keys: $(wc -l <"$HOME/.ssh/authorized_keys")"
  echo "ssh host key: $(ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub | cut -d' ' -f2), made $(stat -c %y /etc/ssh/ssh_host_ed25519_key.pub | cut -d. -f1), booted $(uptime -s)"
  echo "machine id: $(cat /etc/machine-id)"
} | tee out/env.txt

# The base image must not have brought its own: no user but rnk, and host
# keys made at this boot. Otherwise no suite runs.
users="$(getent passwd | awk -F: '$3 >= 1000 && $3 < 60000 { print $1 }' | paste -sd' ')"
boot=$(awk '/^btime / { print $2 }' /proc/stat)
own=1 keys=0
for k in /etc/ssh/ssh_host_*_key; do
  [ -f "$k" ] || continue
  keys=$((keys + 1))
  made=$(sudo stat -c %Y "$k")
  [[ "$made" =~ ^[0-9]+$ ]] && [ "$made" -ge "$boot" ] || own=0
done
if [ "$users" != rnk ] || [ "$keys" = 0 ] || [ "$own" = 0 ] || ! [[ "$boot" =~ ^[0-9]+$ ]]; then
  echo "error: this VM is not the run's own (users: '$users'; $keys host keys, all made since the boot at $boot: $own)" | tee out/summary.txt
  exit 4
fi

status=0
if [ "$after" = 0 ]; then
  : >out/summary.txt
  : >out/suite-times.txt
  # The VM must not reach the host's loopback (ADR-0086): checked before any
  # suite, which do not run otherwise.
  echo "== network"
  if bash guest/net.sh 2>&1 | tee out/network.log; then
    echo "network: ok" >>out/summary.txt
  else
    echo "network: FAILED" | tee -a out/summary.txt
    exit 3
  fi
fi
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
