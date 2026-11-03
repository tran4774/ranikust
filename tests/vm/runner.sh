#!/usr/bin/env bash
# The T2 suites as real root on a GitHub-hosted runner (ADR-0090). Such a
# runner is a fresh VM GitHub makes for one job and deletes after it, with
# passwordless sudo: it takes the place of tests/vm/vm.sh's QEMU VM, and the
# same guest scripts run on it, staged at ~/rnk by tests/vm/stage.sh. The
# workflow .github/workflows/t2.yml is the only caller: this script refuses
# to run anywhere but a GitHub-hosted runner, before anything else.
#
#   RANIKUST_T2_ON_RUNNER=1 tests/vm/runner.sh <suite>...
#
# Suites: unit, smoke, quick and secrets (tests/vm/guest/<suite>.sh, as in
# the VM), and unit-c (the T2c lane of ADR-0063: tests/container/lane.sh,
# its rootful Docker containers on this runner; this script makes the lane's
# marker naming the runner, for the job, ADR-0090). Not here: e2e and ultra
# (benchmarks: a shared
# runner's times are noise, and the workloads need more disk than a runner
# has) and push (the developer host's test registries).
#
# No network check (tests/vm/guest/net.sh): it keeps a VM's root off the
# developer host's loopback (ADR-0086). Here root already is root on the
# machine, which serves nothing of anyone's.
#
# Writes ~/rnk/out: env.txt, <suite>.log, summary.txt (one line per suite)
# and suite-times.txt (suite, seconds, peak MiB used on /, MiB used on /
# before it).
#
# Exit status: 0 when every suite passed; 1 when a suite failed (a refusal
# inside tests/container/lane.sh counts as unit-c failing); 2 when this
# script refused (not this runner, a bad argument), before anything ran;
# 3 when the set-up failed (staging, a build): no suite ran and ~/rnk/out
# holds no results.
set -euo pipefail

die() { printf 'error: %s\n' "$*" >&2; exit 2; }
log() { printf '[runner +%ss] %s\n' "$SECONDS" "$*" >&2; }

# First of all: only on a GitHub-hosted runner, and only when the workflow
# asks. Never on a developer's machine, which this script would change as
# root (files under /, the T2c marker in /etc/ranikust, Docker containers
# and images). The environment alone is not enough (`act` sets
# GITHUB_ACTIONS=true and passes a step's variables), so the machine must
# also look like GitHub's runner: its user and home, and the Azure VM it is.
not_runner() { die "not a GitHub-hosted runner ($1); on your machine use \`just test-vm\` (ADR-0090)"; }
[ "${GITHUB_ACTIONS:-}" = true ] || not_runner "GITHUB_ACTIONS=${GITHUB_ACTIONS:-}"
[ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] || not_runner "RUNNER_ENVIRONMENT=${RUNNER_ENVIRONMENT:-}"
[ -z "${ACT:-}" ] || not_runner "ACT is set: this is act"
[ "$(id -un)" = runner ] || not_runner "user $(id -un), not runner"
[ "${HOME:-}" = /home/runner ] || not_runner "HOME=${HOME:-}, not /home/runner"
vendor="$(cat /sys/class/dmi/id/sys_vendor 2>/dev/null || true)"
[ "$vendor" = "Microsoft Corporation" ] || not_runner "system vendor '$vendor', not Azure's"
[ "${RANIKUST_T2_ON_RUNNER:-}" = 1 ] ||
  die "RANIKUST_T2_ON_RUNNER=1 is not set: only .github/workflows/t2.yml runs this (ADR-0090)"
sudo -n true 2>/dev/null || die "no passwordless sudo"
# Where the guest scripts pull Docker Hub images from: a host name with an
# optional port, as tests/vm/vm.sh requires; it goes into ~/rnk/vm.env.
REGISTRY="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
[[ "$REGISTRY" =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:[0-9]{1,5})?$ ]] ||
  die "RANIKUST_VM_IMAGE_REGISTRY must be a host name with an optional port (got '$REGISTRY')"

[ $# -gt 0 ] || die "name the suites: unit, unit-c, smoke, quick, secrets"
for s in "$@"; do
  case "$s" in
    unit | unit-c | smoke | quick | secrets) ;;
    e2e | ultra | push) die "the $s suite runs only in the local VM (\`just test-vm $s\`, ADR-0090)" ;;
    *) die "unknown suite '$s' (unit, unit-c, smoke, quick, secrets)" ;;
  esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
R="$HOME/rnk"
OUT="$R/out"

# The guest scripts read ~/rnk, as in the VM.
guest=()
for s in "$@"; do [ "$s" = unit-c ] || guest+=("$s"); done
setup_failed() { printf 'error: the set-up failed (%s): no suite ran\n' "$*" >&2; exit 3; }
{ rm -rf "$R" && mkdir -p "$R"; } || setup_failed "making $R"
if [ "${#guest[@]}" -gt 0 ]; then
  log "staging ${guest[*]} at $R"
  RANIKUST_LOG_T0=$SECONDS bash "$REPO/tests/vm/stage.sh" "$R" "${guest[@]}" || setup_failed "staging"
fi
mkdir -p "$OUT" || setup_failed "making $OUT"
printf 'export RANIKUST_VM_IMAGE_REGISTRY=%q\nexport RANIKUST_VM_SIZE=%q\n' \
  "$REGISTRY" "github-hosted runner" >"$R/vm.env"

{
  echo "kernel: $(uname -r)"
  echo "os: $(. /etc/os-release && echo "$PRETTY_NAME")"
  echo "runner: ${RUNNER_NAME:-?} (${ImageOS:-?} ${ImageVersion:-?})"
  echo "docker: $(sudo docker version --format '{{.Server.Version}}'), storage $(sudo docker info --format '{{.Driver}}')"
  echo "cgroup: $(stat -fc %T /sys/fs/cgroup)"
  echo "apparmor: $(cat /sys/module/apparmor/parameters/enabled 2>/dev/null)"
  echo "apparmor_restrict_unprivileged_userns: $(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo n/a)"
  echo "cpus: $(nproc)"
  echo "memory: $(awk '/^MemTotal/ { printf "%d MiB", $2 / 1024 }' /proc/meminfo); swap: $(awk '/^SwapTotal/ { printf "%d MiB", $2 / 1024 }' /proc/meminfo)"
  echo "disk /: $(df -BM --output=size,used,avail / | tail -1)"
  echo "disk /mnt: $(df -BM --output=size,used,avail /mnt 2>/dev/null | tail -1 || echo n/a)"
} | tee "$OUT/env.txt"

used_mib() { df -BM --output=used / | tail -1 | tr -dc 0-9; }

# The T2c lane's marker, made for this job (ADR-0090 amends ADR-0063's
# "made once by the machine's owner" for GitHub-hosted runners only): it
# names this runner, which GitHub deletes after the job, and only the guard
# above lets it be made. Owned by root, written by no one else.
make_marker() {
  sudo install -d -m 0755 -o root -g root /etc/ranikust
  uname -n | sudo tee /etc/ranikust/throwaway-test-host >/dev/null
  sudo chown root:root /etc/ranikust/throwaway-test-host
  sudo chmod 0644 /etc/ranikust/throwaway-test-host
}

: >"$OUT/summary.txt"
: >"$OUT/suite-times.txt"
status=0
for suite in "$@"; do
  log "suite $suite"
  before=$(used_mib)
  peak_file="$OUT/.peak-$suite"
  echo "$before" >"$peak_file"
  # Peak disk use on / while the suite runs, sampled every 2 s.
  (
    peak=$before
    while sleep 2; do
      u=$(used_mib)
      [ "$u" -le "$peak" ] || { peak=$u; echo "$peak" >"$peak_file"; }
    done
  ) &
  sampler=$!
  t0=$SECONDS
  rc=0
  if [ "$suite" = unit-c ]; then
    make_marker
    RANIKUST_CONTAINER_LANE=1 bash "$REPO/tests/container/lane.sh" unit-c 2>&1 | tee "$OUT/$suite.log" || rc=$?
  else
    (cd "$R" && bash "guest/$suite.sh") 2>&1 | tee "$OUT/$suite.log" || rc=$?
  fi
  kill "$sampler" 2>/dev/null || true
  wait "$sampler" 2>/dev/null || true
  peak=$(cat "$peak_file")
  rm -f "$peak_file"
  if [ "$rc" = 0 ]; then
    echo "$suite: ok" >>"$OUT/summary.txt"
  else
    echo "$suite: FAILED" >>"$OUT/summary.txt"
    status=1
  fi
  echo "$suite $((SECONDS - t0)) s, peak $peak MiB used on / (before: $before MiB)" | tee -a "$OUT/suite-times.txt"
done
echo "== summary"
cat "$OUT/summary.txt"
exit "$status"
