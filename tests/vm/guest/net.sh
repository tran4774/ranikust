#!/usr/bin/env bash
# Runs in the T2 VM before any suite (guest/run.sh): the VM's network must
# not reach the developer host's loopback (ADR-0086). tests/vm/vm.sh serves
# a token on a host port bound to 127.0.0.1 only, which nothing in the VM may
# reach; every address the guest could use to name the host is probed for
# it: from root in the VM, from a default and a `--privileged` Docker
# container, and from a ranikust `RUN`. The only host ports the VM reaches
# are the test registries' (5000, 5443), and only when the push suite asked
# for them; then they are probed as a control. The VM's own egress (an image
# pull, an HTTPS request) must work.
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
[ -f vm.env ] && source vm.env

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
RNK="$HOME/rnk/bin/ranikust"
PORT="${RANIKUST_VM_PROBE_PORT:?vm.sh sets the probe port}"
TOKEN="${RANIKUST_VM_PROBE_TOKEN:?vm.sh sets the probe token}"
IMAGE="$REG/library/alpine:3.20"
W=/var/tmp/rnk-net
failed=0

# The addresses that name the host to a guest of QEMU's user network
# (10.0.2.2) or of passt (the default gateway, and the guest's own address,
# which passt can map to the host), whatever the backend in use.
gw="$(ip -4 route show default | awk '{ print $3; exit }')"
self="$(ip -4 -o addr show scope global | awk '{ sub(/\/.*/, "", $4); print $4; exit }')"
ADDRS=(10.0.2.2)
for a in "$gw" "$self"; do
  [ -n "$a" ] && [[ " ${ADDRS[*]} " != *" $a "* ]] && ADDRS+=("$a")
done
echo "host addresses probed: ${ADDRS[*]} port $PORT"

# A shell probe, run as is by every vantage point: prints REACHED <addr>
# for each address that serves the token, else nothing.
PROBE="for a in ${ADDRS[*]}; do
  if wget -q -T 3 -O - http://\$a:$PORT/token 2>/dev/null | grep -qx '$TOKEN'; then echo \"REACHED \$a\"; fi
done"
# A control: an HTTPS request answered (any HTTP status) proves egress.
EGRESS="wget -S -T 10 -O /dev/null https://$REG/v2/ 2>&1 | grep -q 'HTTP/1.[01] [0-9]'"
# With the push suite, the registries' ports must answer (the control
# that the allowed path still works).
REGISTRY="wget -S -T 5 -O /dev/null http://10.0.2.2:5000/v2/ 2>&1 | grep -q 'HTTP/1.[01] 200'"

verdict() { # vantage, probe output
  if [ -n "$2" ]; then
    echo "FAIL $1: the host's loopback is reachable: $(echo "$2" | paste -sd' ')"
    failed=1
  else
    echo "ok   $1: the host's loopback is not reachable"
  fi
}
control() { # vantage, what, command status
  if [ "$3" = 0 ]; then echo "ok   $1: $2"; else echo "FAIL $1: $2"; failed=1; fi
}

for i in 1 2 3 4 5; do
  sudo docker pull -q "$IMAGE" >/dev/null && break
  [ "$i" = 5 ] && { echo "error: docker pull $IMAGE failed 5 times" >&2; exit 1; }
  sleep $((i * 5))
done
control "vm" "image pull (egress)" 0

# Root in the VM itself (busybox's wget, the same tool everywhere).
verdict "root in the VM" "$(sudo busybox sh -c "$PROBE")"
st=0; sudo busybox sh -c "$EGRESS" || st=$?
control "root in the VM" "HTTPS egress" "$st"

for flags in "" "--privileged"; do
  name="docker run ${flags:-(default)}"
  # shellcheck disable=SC2086
  verdict "$name" "$(sudo docker run --rm $flags "$IMAGE" sh -c "$PROBE")"
done

# A ranikust RUN as real root (overlay mode on the VM): the probe's output
# lands in the layer, read back from the tarball.
sudo rm -rf "$W" && sudo mkdir -m 700 -p "$W/ctx" "$W/state"
printf 'FROM %s\nRUN (%s) > /probe; (%s) && echo egress >> /probe || true\n' \
  "$IMAGE" "$(echo "$PROBE" | tr '\n' ' ')" "$EGRESS" | sudo tee "$W/ctx/Dockerfile" >/dev/null
if [ "${RANIKUST_VM_HOST_REGISTRIES:-0}" = 1 ]; then
  printf 'RUN (%s) && echo registry >> /probe || true\n' "$REGISTRY" | sudo tee -a "$W/ctx/Dockerfile" >/dev/null
fi
sudo "$RNK" --context "$W/ctx" --no-push --state-dir "$W/state" --tar-path "$W/image.tar" >"$W/build.log" 2>&1 ||
  { tail -20 "$W/build.log" >&2; echo "error: the probe build failed" >&2; exit 1; }
loaded="$(sudo docker load -i "$W/image.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
[ -n "$loaded" ] || { echo "error: docker load of the probe image printed no image" >&2; exit 1; }
out="$(sudo docker run --rm "$loaded" cat /probe)"
sudo docker rmi -f "$loaded" >/dev/null
reached="$(echo "$out" | grep '^REACHED' || true)"
verdict "ranikust RUN" "$reached"
st=0; echo "$out" | grep -qx egress || st=$?
control "ranikust RUN" "HTTPS egress" "$st"

if [ "${RANIKUST_VM_HOST_REGISTRIES:-0}" = 1 ]; then
  st=0; sudo busybox sh -c "$REGISTRY" || st=$?
  control "root in the VM" "the host's test registry (10.0.2.2:5000)" "$st"
  st=0; sudo docker run --rm "$IMAGE" sh -c "$REGISTRY" || st=$?
  control "docker run (default)" "the host's test registry (10.0.2.2:5000)" "$st"
  st=0; echo "$out" | grep -qx registry || st=$?
  control "ranikust RUN" "the host's test registry (10.0.2.2:5000)" "$st"
fi

sudo rm -rf "$W"
[ "$failed" = 0 ] || { echo "error: the VM's network does not meet ADR-0086" >&2; exit 1; }
echo "network: the host's loopback is out of reach"
