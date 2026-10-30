#!/usr/bin/env bash
# Runs in the T2 VM: the quick smoke cases (tests/smoke/cases) built by the
# ranikust binary as real root on the VM, Docker loading and running what
# they built. RANIKUST_QUICK_CASES names cases (space-separated); unset, all
# run. For a smoke test or a proof of concept that needs real root; the
# nextjs-kaniko workloads (`e2e`) are for large benchmarks only.
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
source guest/smoke-lib.bash
# shellcheck disable=SC1091
source guest/smoke-cases.bash
# shellcheck disable=SC1091
[ -f quick.env ] && source quick.env

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
BASE="$REG/library/alpine:3.20"
RNK="$HOME/rnk/bin/ranikust"
ENGINE="sudo docker"
CASES_OUT="$HOME/rnk/out/quick"
W=/var/tmp/rnk-quick
SECRET="$W/secret"

sudo rm -rf "$W" && sudo mkdir -m 700 -p "$W/w"
printf %s "$SMOKE_CASE_SECRET" | sudo tee "$SECRET" >/dev/null
mkdir -p "$CASES_OUT"

rnk_run() {
  local ctx="$1" o="$2" rc=0
  shift 2
  sudo "$RNK" --context "$ctx" --workdir "$W/w" --no-push --oci-layout-path "$o/oci" \
    --tar-path "$o/image.tar" --secret "id=tok,src=$SECRET" "$@" || rc=$?
  sudo chown -R "$(id -u):$(id -g)" "$o"
  return "$rc"
}

# A plain command: under `||` errexit would be off inside the cases.
set +e
# shellcheck disable=SC2086
smoke_cases ${RANIKUST_QUICK_CASES:-}
rc=$?
set -e

sudo docker images --format '{{.Repository}}:{{.Tag}}' | grep '^localhost/rnk-case:' |
  xargs -r sudo docker rmi -f >/dev/null
! grep -q "$W" /proc/self/mountinfo || { echo "error: mounts left under $W" >&2; exit 1; }
! pgrep -x ranikust >/dev/null || { echo "error: ranikust processes left" >&2; exit 1; }
sudo rm -rf "$W"
[ "$rc" = 0 ] && echo "quick: ok"
exit "$rc"
