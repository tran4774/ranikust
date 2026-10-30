#!/usr/bin/env bash
# The T2 harness: runs test suites as real root in a disposable QEMU/KVM VM,
# never on the host (docs/testing.md#the-vm-harness-tier-t2).
#
#   tests/vm/vm.sh [suite]...     suites: unit, smoke, e2e, push; default unit smoke
#
# Runs as the invoking user: QEMU needs read-write access to /dev/kvm (its
# group or an ACL), never sudo. Each run boots a fresh copy-on-write overlay
# of a verified Ubuntu cloud image, copies in static musl binaries and the
# guest scripts, runs the suites over SSH, copies the results out to
# target/vm/results/<release>/ and deletes the VM, its disk and its key
# whatever the outcome.
#
# Environment:
#   RANIKUST_VM_RELEASE  Ubuntu release: noble (24.04, kernel 6.8, default)
#                        or jammy (22.04, kernel 5.15)
#   RANIKUST_VM_CACHE    verified image cache, default ~/.cache/ranikust/vm
#   RANIKUST_VM_CPUS     vCPUs, default 4
#   RANIKUST_VM_MEM      memory in MiB, default 4096 (12288 with e2e)
#   RANIKUST_VM_DISK     disk size, default 20G (80G with e2e)
#   RANIKUST_NEXTJS_KANIKO  the nextjs-kaniko checkout e2e reads, default
#                        ../nextjs-kaniko next to the repository
#   RANIKUST_E2E_*       options of the e2e suite (tests/vm/guest/e2e.sh)
#   RANIKUST_VM_IMAGE_REGISTRY  where the guest pulls Docker Hub images
#                        from: default mirror.gcr.io (Google's public Docker
#                        Hub mirror, no pull rate limit); docker.io to pull
#                        from Docker Hub itself
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$REPO/tests/vm"
RELEASE="${RANIKUST_VM_RELEASE:-noble}"
CACHE="${RANIKUST_VM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/ranikust/vm}"
CPUS="${RANIKUST_VM_CPUS:-4}"
IMAGE_REGISTRY="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
MEM="${RANIKUST_VM_MEM:-4096}"
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
MUSL=x86_64-unknown-linux-musl
VM_USER=rnk

log() { printf '[vm] %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

SUITES=("$@")
[ ${#SUITES[@]} -gt 0 ] || SUITES=(unit smoke)
for s in "${SUITES[@]}"; do
  case "$s" in unit | smoke | e2e | push) ;; *) die "unknown suite '$s' (unit, smoke, e2e, push)" ;; esac
done
case "$RELEASE" in noble | jammy) ;; *) die "RANIKUST_VM_RELEASE must be noble or jammy" ;; esac
[ "$(id -u)" != 0 ] || die "run as a regular user: QEMU needs /dev/kvm, not root"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "no read-write access to /dev/kvm (add yourself to its group or an ACL)"
for tool in qemu-system-x86_64 qemu-img xorriso ssh ssh-keygen curl sha256sum jq python3 cargo-zigbuild; do
  command -v "$tool" >/dev/null || die "missing $tool"
done

wants() { local s; for s in "${SUITES[@]}"; do [ "$s" = "$1" ] && return 0; done; return 1; }

DISK="${RANIKUST_VM_DISK:-20G}"
if wants e2e; then
  # Three builders of W1 (about 3 GB of rootfs each), their images in the
  # registry and the npm proxy's storage.
  MEM="${RANIKUST_VM_MEM:-12288}"
  DISK="${RANIKUST_VM_DISK:-80G}"
  # Next to the main checkout, also when this is a git worktree.
  MAIN="$(dirname "$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)")"
  NEXTJS="${RANIKUST_NEXTJS_KANIKO:-$(dirname "$MAIN")/nextjs-kaniko}"
  [ -f "$NEXTJS/bench/run-fair.sh" ] || die "no nextjs-kaniko checkout at $NEXTJS (set RANIKUST_NEXTJS_KANIKO)"
  command -v podman >/dev/null || die "missing podman (e2e saves the reference images from it)"
fi
if wants push; then
  # The host's test registries (tests/registry, `just registry-up`), which
  # the guest reaches as 10.0.2.2: QEMU's user network maps it to the
  # host's loopback, where they listen.
  REGDATA="$REPO/tests/registry/_data"
  curl -fsS -o /dev/null http://127.0.0.1:5000/v2/ &&
    curl -fsS -o /dev/null --cacert "$REGDATA/certs/ca.crt" -u "$(cat "$REGDATA/auth/credentials")" https://localhost:5443/v2/ ||
    die "the push suite needs the host's test registries: run \`just registry-up\`"
fi

# --- Build: static binaries and the guest tree -------------------------------

STAGE="$TARGET/vm/stage"
rm -rf "$STAGE" && mkdir -p "$STAGE/bin/tests"
cp -r "$HERE/guest" "$STAGE/guest"
cp "$REPO/tests/smoke/lib.bash" "$STAGE/guest/smoke-lib.bash"
# The smoke suite's build context; not `guest/e2e`, which holds the e2e
# suite's scripts.
cp -r "$REPO/tests/smoke/e2e" "$STAGE/guest/smoke-e2e"
# The checks of a RUN in a network namespace a user namespace owns.
cp "$REPO/tests/smoke/netns-check.py" "$STAGE/guest/netns-check.py"
zigbuild() { CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc cargo-zigbuild "$1" --locked --target "$MUSL" "${@:2}"; }

if wants unit; then
  # T2 tests: every `#[ignore = "T2: ..."]` and the name of the fn under it.
  (cd "$REPO" && grep -rn --include='*.rs' -A3 '#\[ignore = "T2' crates) |
    sed -nE 's/^.*fn ([A-Za-z0-9_]+).*$/\1/p' | sort -u >"$STAGE/t2-tests.txt"
  # The crates holding them.
  mapfile -t crates < <(cd "$REPO" && grep -rl --include='*.rs' '#\[ignore = "T2' crates |
    cut -d/ -f2 | sort -u)
  [ -s "$STAGE/t2-tests.txt" ] || die "no T2 tests found"
  log "building the test binaries of ${crates[*]} ($(wc -l <"$STAGE/t2-tests.txt") T2 tests)"
  pkgs=(); for c in "${crates[@]}"; do pkgs+=(-p "$c"); done
  (cd "$REPO" && zigbuild test --no-run --message-format=json "${pkgs[@]}") |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null) | .executable' |
    while read -r exe; do cp "$exe" "$STAGE/bin/tests/"; done
fi
if wants smoke || wants e2e || wants push; then
  log "building the smoke-build example and the ranikust binary"
  (cd "$REPO" && zigbuild zigbuild --release -p ranikust --example smoke-build --bin ranikust)
  cp "$TARGET/$MUSL/release/examples/smoke-build" "$TARGET/$MUSL/release/ranikust" "$STAGE/bin/"
fi

if wants e2e; then
  # The reference images, saved once from the host's podman (where
  # nextjs-kaniko's harness built or pulled them) and kept in the cache.
  E2E_IMAGES=(localhost/kaniko-bench:stream docker.io/library/registry:2 docker.io/verdaccio/verdaccio:6.1.6
    docker.io/moby/buildkit@sha256:f8a833b2de9d68e27f0815e4a737abdfaf8a2e4c615650557df11025101557b4)
  E2E_NAMES=(localhost/kaniko-bench:stream registry:2 verdaccio/verdaccio:6.1.6 rnk-e2e/buildkit:v0.33.1-rootless)
  # One archive per image: Docker with the containerd store gives a loaded
  # image another ID, so the guest tags what each `docker load` reports.
  mkdir -p "$CACHE/e2e"
  : >"$STAGE/e2e-images.txt"
  for i in "${!E2E_IMAGES[@]}"; do
    id="$(SUPPRESS_BOLTDB_WARNING=1 podman image inspect -f '{{.Id}}' "${E2E_IMAGES[$i]}" 2>/dev/null)" ||
      die "podman has no ${E2E_IMAGES[$i]} (nextjs-kaniko's harness uses it)"
    if [ ! -f "$CACHE/e2e/$id.tar" ]; then
      log "saving ${E2E_IMAGES[$i]} to $CACHE/e2e/"
      SUPPRESS_BOLTDB_WARNING=1 podman save -q --format docker-archive -o "$CACHE/e2e/$id.tar.part" "$id"
      mv "$CACHE/e2e/$id.tar.part" "$CACHE/e2e/$id.tar"
    fi
    echo "$id ${E2E_NAMES[$i]}" >>"$STAGE/e2e-images.txt"
  done
  env | grep '^RANIKUST_E2E_' | sed 's/^/export /' >"$STAGE/e2e.env" || true
fi
if wants push; then
  mkdir -p "$STAGE/registry"
  cp "$REGDATA/certs/ca.crt" "$REGDATA/auth/credentials" "$STAGE/registry/"
fi
# Read by guest/run.sh: the suites pull Docker Hub images from here.
printf 'export RANIKUST_VM_IMAGE_REGISTRY=%q\n' "$IMAGE_REGISTRY" >"$STAGE/vm.env"

# --- Image: fetch and verify against SHA256SUMS ------------------------------

IMG_NAME="$RELEASE-server-cloudimg-amd64.img"
IMG_URL="https://cloud-images.ubuntu.com/$RELEASE/current"
IMG="$CACHE/$IMG_NAME"
mkdir -p "$CACHE"
curl -fsSL "$IMG_URL/SHA256SUMS" -o "$CACHE/$RELEASE-SHA256SUMS"
want="$(awk -v f="*$IMG_NAME" '$2 == f { print $1 }' "$CACHE/$RELEASE-SHA256SUMS")"
[ -n "$want" ] || die "$IMG_NAME is not in $IMG_URL/SHA256SUMS"
if [ ! -f "$IMG" ] || [ "$(sha256sum "$IMG" | cut -d' ' -f1)" != "$want" ]; then
  log "downloading $IMG_URL/$IMG_NAME"
  curl -fL --progress-bar "$IMG_URL/$IMG_NAME" -o "$IMG.part"
  [ "$(sha256sum "$IMG.part" | cut -d' ' -f1)" = "$want" ] || { rm -f "$IMG.part"; die "SHA-256 mismatch for $IMG_NAME"; }
  mv "$IMG.part" "$IMG"
fi
log "image $IMG_NAME sha256 $want (verified)"

# --- Run directory, torn down on every exit path -----------------------------

mkdir -p "$TARGET/vm"
RUN="$(mktemp -d "$TARGET/vm/run.XXXXXX")"
SOCKDIR=
RESULTS="$TARGET/vm/results/$RELEASE"
rm -rf "$RESULTS"
QEMU_PID=

teardown() {
  local status=$?
  set +e
  if [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
    log "stopping the VM"
    python3 -c '
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.sendall(b"quit\n"); s.recv(4096)
' "$MONITOR" 2>/dev/null
    for _ in $(seq 20); do kill -0 "$QEMU_PID" 2>/dev/null || break; sleep 0.5; done
    kill -9 "$QEMU_PID" 2>/dev/null
  fi
  mkdir -p "$RESULTS" && cp "$RUN/serial.log" "$RESULTS/serial.log" 2>/dev/null
  rm -rf "$RUN" ${SOCKDIR:+"$SOCKDIR"}
  if [ -n "$QEMU_PID" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
    echo "error: QEMU (pid $QEMU_PID) is still running" >&2
    status=1
  fi
  exit "$status"
}
trap teardown EXIT
trap 'exit 130' INT TERM

# A Unix socket path must fit in 108 bytes, which a run directory in a deep
# checkout (a git worktree) does not.
SOCKDIR="$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/rnk-vm.XXXXXX")"
MONITOR="$SOCKDIR/monitor.sock"

ssh-keygen -q -t ed25519 -N '' -C "ranikust-vm" -f "$RUN/key"
mkdir "$RUN/seed"
# The guest's Docker pulls docker.io images through the same mirror.
if [ "$IMAGE_REGISTRY" = docker.io ]; then MIRRORS='[]'; else MIRRORS="[\"https://$IMAGE_REGISTRY\"]"; fi
sed -e "s|@SSH_KEY@|$(cat "$RUN/key.pub")|" -e "s|@REGISTRY_MIRRORS@|$MIRRORS|" \
  "$HERE/cloud-init/user-data" >"$RUN/seed/user-data"
printf 'instance-id: ranikust-vm-%s\nlocal-hostname: ranikust-vm\n' "$(basename "$RUN")" >"$RUN/seed/meta-data"
xorriso -as mkisofs -quiet -output "$RUN/seed.iso" -volid cidata -joliet -rock \
  "$RUN/seed/user-data" "$RUN/seed/meta-data"
qemu-img create -q -f qcow2 -F qcow2 -b "$IMG" "$RUN/disk.qcow2" "$DISK"

PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
log "booting $RELEASE ($CPUS vCPUs, $MEM MiB), SSH on 127.0.0.1:$PORT"
qemu-system-x86_64 \
  -name ranikust-vm -enable-kvm -cpu host -smp "$CPUS" -m "$MEM" -machine q35,accel=kvm \
  -drive file="$RUN/disk.qcow2",if=virtio,format=qcow2 \
  -drive file="$RUN/seed.iso",if=virtio,format=raw,readonly=on \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:"$PORT"-:22 -device virtio-net-pci,netdev=n0 \
  -display none -serial file:"$RUN/serial.log" \
  -monitor unix:"$MONITOR",server,nowait \
  -pidfile "$RUN/qemu.pid" -daemonize
QEMU_PID="$(cat "$RUN/qemu.pid")"

SSH_OPTS=(-i "$RUN/key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR -o ConnectTimeout=5 -o ServerAliveInterval=15)
vssh() { ssh "${SSH_OPTS[@]}" -p "$PORT" "$VM_USER@127.0.0.1" "$@"; }

for i in $(seq 120); do
  vssh true 2>/dev/null && break
  [ "$i" = 120 ] && die "no SSH after 10 minutes (serial log: $RESULTS/serial.log)"
  sleep 5
done
log "waiting for cloud-init (installs Docker)"
vssh 'cloud-init status --wait >/dev/null; test -e /var/lib/ranikust-vm-ready' ||
  die "cloud-init failed (serial log: $RESULTS/serial.log)"

log "copying in $(du -sh "$STAGE" | cut -f1)"
tar -C "$STAGE" -cf - . | vssh 'mkdir rnk && tar -C rnk -xf -'
if wants e2e; then
  log "copying in the e2e seed (images, registry, npm proxy storage, workloads)"
  python3 -I "$HERE/e2e-seed.py" "$NEXTJS" "$CACHE/e2e" "$STAGE/e2e-images.txt" |
    vssh 'mkdir -p rnk/e2e && tar -C rnk/e2e -xf -'
fi

set +e
vssh "bash rnk/guest/run.sh ${SUITES[*]}"
status=$?
set -e
mkdir -p "$RESULTS"
vssh 'tar -C rnk/out -cf - .' | tar -C "$RESULTS" -xf - ||
  log "could not copy the results out"
log "results in $RESULTS"
exit "$status"
