#!/usr/bin/env bash
# Runs in the T2 VM before any suite (guest/run.sh): the VM's network must
# not reach the developer host's loopback (ADR-0086). tests/vm/vm.sh serves
# a token over TCP and UDP on a port of the host's loopback only (127.0.0.1,
# and ::1 where the host has it), which nothing in the VM may reach.
#
# Every address the guest could use to name the host is probed for it from
# root in the VM, a default and a `--privileged` Docker container, and a
# ranikust `RUN`; root also hands passt raw frames addressed to the loopback
# addresses themselves (guest/rawprobe.py), which CAP_NET_RAW allows all of
# them. The test registries' ports (10.0.2.2:5000 and :5443) must not answer
# either: vm.sh opens their gate only while the push suite runs.
#
# A vantage passes only when its probe ran to the end and the same tool, on
# the same way out, reached the internet (a positive control): a probe that
# could not run, or could reach nothing at all, fails the run too.
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
[ -f vm.env ] && source vm.env

REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
RNK="$HOME/rnk/bin/ranikust"
PORT="${RANIKUST_VM_PROBE_PORT:?vm.sh sets the probe port}"
TOKEN="${RANIKUST_VM_PROBE_TOKEN:?vm.sh sets the probe token}"
REGISTRY_PORTS="${RANIKUST_VM_REGISTRY_PROBE:-10.0.2.2:5000 10.0.2.2:5443}"
IMAGE="$REG/library/alpine:3.20"
W=/var/tmp/rnk-net
failed=0
for tool in curl python3; do
  command -v "$tool" >/dev/null || { echo "error: no $tool in the VM" >&2; exit 1; }
done

# The addresses that can name the host: QEMU's user network maps its host
# address (10.0.2.2 by default) to the host's loopback, passt its default
# gateway and the guest's own address unless told not to; vm.sh adds the
# others it set up (RANIKUST_VM_PROBE_ADDRS), whatever the backend in use.
ADDRS=()
# shellcheck disable=SC2086
for a in 10.0.2.2 ${RANIKUST_VM_PROBE_ADDRS:-} \
  $(ip -4 route show default | awk '{ for (i = 1; i < NF; i++) if ($i == "via") print $(i + 1) }') \
  $(ip -4 -o addr show scope global | awk '{ sub(/\/.*/, "", $4); print $4 }'); do
  [[ "$a" =~ ^[0-9]+(\.[0-9]+){3}$ ]] && [[ " ${ADDRS[*]} " != *" $a "* ]] && ADDRS+=("$a")
done
echo "host addresses probed: ${ADDRS[*]} port $PORT; registry ports: $REGISTRY_PORTS"

# The probe the containers and the RUN run as is, with alpine's busybox
# wget: "REACHED <what>" for each address that serves the token and each
# registry port that answers HTTP at all (the HTTPS one answers plain HTTP
# with a 400); "EGRESS" when an HTTP request to the image registry is
# answered, any status (the positive control, with the same tool on the
# same way out); "PROBED" once it ran through.
PROBE="for a in ${ADDRS[*]}; do"
PROBE+=" if wget -q -T 2 -O - http://\$a:$PORT/token 2>/dev/null | grep -qx '$TOKEN'; then echo \"REACHED \$a:$PORT\"; fi;"
PROBE+=" done; for r in $REGISTRY_PORTS; do"
PROBE+=" if wget -S -T 2 -O /dev/null http://\$r/v2/ 2>&1 | grep -q 'HTTP/1\\.[01] [0-9]'; then echo \"REACHED \$r (registry)\"; fi;"
PROBE+=" done; if wget -S -T 10 -O /dev/null http://$REG/v2/ 2>&1 | grep -q 'HTTP/1\\.[01] [0-9]'; then echo EGRESS; fi;"
PROBE+=" echo PROBED"
# The same probe for root in the VM, with the VM's curl: the VM's own
# busybox wget is no tool to trust (jammy's segfaults on every request,
# which the control caught). An HTTP status of 000 is no answer.
PROBE_CURL="for a in ${ADDRS[*]}; do"
PROBE_CURL+=" if curl -sS -m 2 --noproxy '*' http://\$a:$PORT/token 2>/dev/null | grep -qx '$TOKEN'; then echo \"REACHED \$a:$PORT\"; fi;"
PROBE_CURL+=" done; for r in $REGISTRY_PORTS; do"
PROBE_CURL+=" c=\$(curl -s -m 2 --noproxy '*' -o /dev/null -w '%{http_code}' http://\$r/v2/); [ \"\${c:-000}\" = 000 ] || echo \"REACHED \$r (registry)\";"
PROBE_CURL+=" done; c=\$(curl -s -m 10 -o /dev/null -w '%{http_code}' http://$REG/v2/); [ \"\${c:-000}\" = 000 ] || echo EGRESS;"
PROBE_CURL+=" echo PROBED"

# judge <vantage> <probe output>: the probe ran through (PROBED), reached
# nothing (no REACHED) and got out (EGRESS).
judge() {
  local reached
  if ! grep -qx PROBED <<<"$2"; then
    echo "FAIL $1: the probe did not run through"
    failed=1
    return
  fi
  reached="$(grep '^REACHED' <<<"$2" | sed 's/^REACHED //' | paste -sd' ')" || true
  if [ -n "$reached" ]; then
    echo "FAIL $1: the host's loopback is reachable: $reached"
    failed=1
  else
    echo "ok   $1: the host's loopback is not reachable"
  fi
  if grep -qx EGRESS <<<"$2"; then
    echo "ok   $1: egress, same tool and way out"
  else
    echo "FAIL $1: no egress with the probe's own tool: the probe proves nothing"
    failed=1
  fi
}
# vantage <name> <command...>: runs the probe through that command; one that
# fails to run fails the vantage.
vantage() {
  local name=$1 out
  shift
  if out="$("$@")"; then
    judge "$name" "$out"
  else
    echo "FAIL $name: the probe did not run (status $?)"
    failed=1
  fi
}

for i in 1 2 3 4 5; do
  sudo docker pull -q "$IMAGE" >/dev/null && break
  [ "$i" = 5 ] && { echo "error: docker pull $IMAGE failed 5 times" >&2; exit 1; }
  sleep $((i * 5))
done
echo "ok   vm: image pull (egress)"

vantage "root in the VM" sudo sh -c "$PROBE_CURL"
for flags in "" "--privileged"; do
  # shellcheck disable=SC2086
  vantage "docker run ${flags:-(default)}" sudo docker run --rm $flags "$IMAGE" sh -c "$PROBE"
done

# A ranikust RUN as real root (overlay mode on the VM, the VM's network):
# the probe's output lands in the layer, read back from the tarball.
sudo rm -rf "$W" && sudo mkdir -m 700 -p "$W/ctx" "$W/state"
printf 'FROM %s\nRUN (%s) > /probe 2>&1\n' "$IMAGE" "$PROBE" | sudo tee "$W/ctx/Dockerfile" >/dev/null
if sudo "$RNK" --context "$W/ctx" --no-push --state-dir "$W/state" --tar-path "$W/image.tar" >out/network-build.log 2>&1 &&
  loaded="$(sudo docker load -i "$W/image.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)" && [ -n "$loaded" ] &&
  out="$(sudo docker run --rm "$loaded" cat /probe)"; then
  sudo docker rmi -f "$loaded" >/dev/null
  judge "ranikust RUN" "$out"
else
  tail -20 out/network-build.log
  echo "FAIL ranikust RUN: the probe build or its read-back failed"
  failed=1
fi

read -r gw egress_if < <(ip -4 route show default | awk '{ v = d = ""; for (i = 1; i < NF; i++) { if ($i == "via") v = $(i + 1); if ($i == "dev") d = $(i + 1) } print v, d; exit }')
[ -n "$gw" ] && [ -n "$egress_if" ] || { echo "error: no IPv4 default route in the VM" >&2; exit 1; }

# IPv6 is off on the egress (ADR-0086): no global IPv6 address on its NIC
# and no IPv6 default route; the raw probe below also finds that passt
# answers no IPv6 frame.
v6="$(ip -6 -o addr show dev "$egress_if" scope global; ip -6 route show default)"
if [ -z "$v6" ]; then
  echo "ok   vm: IPv6 egress is off (no global IPv6 address on $egress_if, no IPv6 default route)"
else
  echo "FAIL vm: IPv6 egress is on: $(echo "$v6" | awk '{ print $1, $2, $3, $4 }' | paste -sd';')"
  failed=1
fi

# raw <name> <rawprobe.py arguments...>: raw frames from root in the VM,
# straight to that NIC's backend, to the loopback addresses themselves.
raw() {
  local name=$1 out rc=0
  shift
  out="$(sudo python3 -I guest/rawprobe.py --port "$PORT" --token "$TOKEN" "$@" 2>&1)" || rc=$?
  echo "$out" | sed 's/^/  /'
  if [ "$rc" != 0 ] || ! grep -qx PROBED <<<"$out"; then
    echo "FAIL $name: the probe or a control failed (status $rc)"
    failed=1
  elif grep -q '^REACHED' <<<"$out"; then
    echo "FAIL $name: the host's loopback is reachable: $(grep '^REACHED' <<<"$out" | paste -sd';')"
    failed=1
  else
    echo "ok   $name: the host's loopback is not reachable"
  fi
}
# The egress NIC (passt); controls through the same socket: a SYN to the
# registry, a DNS query.
dns="$(awk '$1 == "nameserver" { print $2; exit }' /run/systemd/resolve/resolv.conf 2>/dev/null || true)"
raw "raw frames, egress" --iface "$egress_if" --gateway "$gw" --dns "$dns" --registry "$REG"
# The harness NIC (QEMU's user network with restrict=on), whose only
# control is a forward to nothing on the host.
if [ -n "${RANIKUST_VM_HARNESS:-}" ]; then
  read -r h_guest h_host h_control <<<"$RANIKUST_VM_HARNESS"
  h_if="$(ip -4 -o addr show | awk -v a="$h_guest" '{ split($4, p, "/") } p[1] == a { print $2; exit }')"
  if [ -n "$h_if" ] && [ "$h_if" != "$egress_if" ]; then
    raw "raw frames, harness" --iface "$h_if" --gateway "$h_host" --control "$h_control"
  else
    echo "FAIL raw frames, harness: no NIC of its own holds $h_guest"
    failed=1
  fi
fi

sudo rm -rf "$W"
[ "$failed" = 0 ] || { echo "error: the VM's network does not meet ADR-0086" >&2; exit 1; }
echo "network: the host's loopback is out of reach"
