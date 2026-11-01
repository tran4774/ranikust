#!/usr/bin/env bash
# The T2 harness: runs test suites as real root in a disposable QEMU/KVM VM,
# never on the host (docs/testing.md#the-vm-harness-tier-t2).
#
#   tests/vm/vm.sh [suite]...     suites: unit, smoke, quick, e2e, push,
#                                 secrets, ultra; default unit smoke
#
# `quick` (the small Dockerfiles of tests/smoke/cases, a minute or two) is
# for smoke tests and proofs of concept that need real root; `e2e` (the
# light workload W0 built by every builder, a few minutes) for a change to
# the e2e harness or the builders' comparison; `ultra` (nextjs-kaniko's
# W1-W3, tens of minutes per round) only when the maintainer asks for
# performance numbers or an acceptance run (docs/testing.md#which-test-for-what).
# `secrets` runs kaniko mode on `/` in default Docker containers with
# attacker `RUN`s that try to read ranikust's state and credentials
# (ADR-0051).
#
# Runs as the invoking user: QEMU needs read-write access to /dev/kvm (its
# group or an ACL), never sudo. Each run boots a fresh copy-on-write overlay
# of a provisioned base image (the verified Ubuntu cloud image with Docker
# installed, built once and cached: cloud-init/provision), copies in static
# musl binaries and the guest scripts, runs the suites over SSH, copies the
# results out to target/vm/results/<release>/ and deletes the VM, its disk
# and its key whatever the outcome.
#
# Environment:
#   RANIKUST_VM_RELEASE  Ubuntu release: noble (24.04, kernel 6.8, default)
#                        or jammy (22.04, kernel 5.15)
#   RANIKUST_VM_CACHE    verified image cache (cloud images, provisioned base
#                        images, e2e reference images), default
#                        tests/e2e/_vm-image of the main checkout, shared by
#                        every git worktree (gitignored)
#   RANIKUST_VM_LOCK_WAIT  seconds to wait for another run's VM to finish,
#                        default 3600: one VM at a time on this machine
#   RANIKUST_VM_CPUS     vCPUs, default 12, at most the host's CPUs less 2
#   RANIKUST_VM_MEM      memory in MiB, default 16384, less when the host
#                        has under that plus 3 GiB available (never under
#                        4096, 8192 with e2e); a value set here is used as
#                        is, with a warning if it leaves the host short
#   RANIKUST_VM_GOLDEN   1 (default) boots the cached provisioned base image;
#                        0 boots the plain cloud image and lets cloud-init
#                        install the packages, as every run did before
#   RANIKUST_VM_DISK     disk size in M, G or T, default 20G (40G with e2e);
#                        the host needs that much free, less 2 GiB
#   RANIKUST_NEXTJS_KANIKO  the nextjs-kaniko checkout ultra reads, default
#                        ../nextjs-kaniko next to the repository
#   RANIKUST_E2E_*       options of the e2e and ultra suites (tests/vm/guest/e2e.sh)
#   RANIKUST_QUICK_CASES the cases `quick` runs, space-separated; default all
#   RANIKUST_QUICK_MODE  `kaniko` builds the `quick` cases in kaniko mode on
#                        the workdir (default: the probed mode, overlay)
#   RANIKUST_VM_IMAGE_REGISTRY  where the guest pulls Docker Hub images
#                        from: default mirror.gcr.io (Google's public Docker
#                        Hub mirror, no pull rate limit); docker.io to pull
#                        from Docker Hub itself
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The main checkout, also when this is a git worktree: the cache and the
# one-VM lock live there so that worktrees share them.
MAIN="$(dirname "$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir)")"
HERE="$REPO/tests/vm"
RELEASE="${RANIKUST_VM_RELEASE:-noble}"
CACHE="${RANIKUST_VM_CACHE:-$MAIN/tests/e2e/_vm-image}"
IMAGE_REGISTRY="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
GOLDEN_ON="${RANIKUST_VM_GOLDEN:-1}"
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"
MUSL=x86_64-unknown-linux-musl
VM_USER=rnk
# What cloud-init installs: into the base image once (cloud-init/provision),
# or at every boot with RANIKUST_VM_GOLDEN=0. Part of the base image's key.
PACKAGES=(docker.io zstd pigz busybox-static)

# Seconds since the start on every line, for the timings of a run.
log() { printf '[vm +%ss] %s\n' "$SECONDS" "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

SUITES=("$@")
[ ${#SUITES[@]} -gt 0 ] || SUITES=(unit smoke)
for s in "${SUITES[@]}"; do
  case "$s" in unit | smoke | quick | e2e | push | secrets | ultra) ;; *) die "unknown suite '$s' (unit, smoke, quick, e2e, push, secrets, ultra)" ;; esac
done
case "$RELEASE" in noble | jammy) ;; *) die "RANIKUST_VM_RELEASE must be noble or jammy" ;; esac
case "$GOLDEN_ON" in 0 | 1) ;; *) die "RANIKUST_VM_GOLDEN must be 0 or 1" ;; esac
# They reach QEMU's command line and arithmetic: whole numbers only.
for v in RANIKUST_VM_CPUS RANIKUST_VM_MEM; do
  [ -z "${!v:-}" ] || [[ "${!v}" =~ ^[1-9][0-9]*$ ]] || die "$v must be a whole number (got '${!v}')"
done
[ "$(id -u)" != 0 ] || die "run as a regular user: QEMU needs /dev/kvm, not root"
[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "no read-write access to /dev/kvm (add yourself to its group or an ACL)"
for tool in qemu-system-x86_64 qemu-img xorriso ssh ssh-keygen curl sha256sum jq python3 cargo-zigbuild; do
  command -v "$tool" >/dev/null || die "missing $tool"
done

wants() { local s; for s in "${SUITES[@]}"; do [ "$s" = "$1" ] && return 0; done; return 1; }

# The VM's disk: a qcow2 overlay over the cloud image that grows with what
# the guest writes, up to about $DISK less the 2 GiB or so of the cloud
# image's own content, which stays in the backing file. What really bounds
# it is the guest: e2e.sh stops before its free space falls under
# RANIKUST_E2E_MIN_FREE_GB, and the disk has discard, so the blocks the
# guest frees and trims (fstrim after each prune) go back to the host. The
# host must still have room for the worst case, $DISK less 2 GiB: checked
# here, before anything is written or locked, and again right before the
# disk is created.
DISK="${RANIKUST_VM_DISK:-20G}"
# e2e: the reference images, the npm proxy's storage, one build's root and
# image at a time (the registry is pruned between builds): the guest
# peaked at 6.3 GB on W1 and W3 (2026-11-01), so 40G leaves room for W2
# and many rounds.
wants ultra && DISK="${RANIKUST_VM_DISK:-40G}"
# Validated before any arithmetic: no octal, no expansion of raw input.
[[ "$DISK" =~ ^[1-9][0-9]*[MmGgTt]$ ]] ||
  die "RANIKUST_VM_DISK must be a whole number of M, G or T without a leading zero (got '$DISK')"
disk_n=${DISK%?}
case "$DISK" in
  *[Tt]) disk_gib=$((disk_n * 1024)) ;;
  *[Gg]) disk_gib=$disk_n ;;
  *) disk_gib=$((disk_n / 1024)) ;;
esac
need_gib=$((disk_gib > 2 ? disk_gib - 2 : 0))
# The provisioned base image is a disk of GOLDEN_GIB: a run's disk on top of
# it cannot be smaller.
GOLDEN_GIB=10
[ "$GOLDEN_ON" = 0 ] || [ "$disk_gib" -ge "$GOLDEN_GIB" ] ||
  die "RANIKUST_VM_DISK must be at least ${GOLDEN_GIB}G over the provisioned base image (or set RANIKUST_VM_GOLDEN=0)"
check_host_free() {
  local there="$TARGET" free
  until [ -d "$there" ]; do there="$(dirname "$there")"; done
  free=$(($(df -Pk "$there" | awk 'NR == 2 { print $4 }') / 1024 / 1024))
  [ "$free" -ge "$need_gib" ] ||
    die "only ${free} GiB free under $TARGET for a VM disk of $DISK (needs ${need_gib} GiB); free space or set RANIKUST_VM_DISK"
}
check_host_free

# One VM at a time on this machine, whichever checkout or worktree starts
# it: the lock is held on fd 9 until this script exits, so it also guards
# the cache while images are downloaded or saved into it.
mkdir -p "$CACHE"
exec 9>"$CACHE/.vm.lock"
if ! flock -n 9; then
  log "another run's VM is up; waiting for it (at most ${RANIKUST_VM_LOCK_WAIT:-3600} s)"
  flock -w "${RANIKUST_VM_LOCK_WAIT:-3600}" 9 || die "another run's VM still holds $CACHE/.vm.lock"
fi

# --- Size: measured now, once the previous run's VM has gone ------------------

# 12 vCPUs by default, leaving the host two of its own; a value set in
# RANIKUST_VM_CPUS is used as is.
nproc_host=$(nproc)
if [ -n "${RANIKUST_VM_CPUS:-}" ]; then
  CPUS=$RANIKUST_VM_CPUS cpus_how=set
else
  CPUS=12 cpus_how=default
  if [ "$CPUS" -gt $((nproc_host - 2)) ]; then
    CPUS=$((nproc_host > 3 ? nproc_host - 2 : 1)) cpus_how="capped: the host has $nproc_host"
  fi
fi
# 16 GiB by default. A guest touches all of its memory (page cache), and
# QEMU keeps it: on a host short of memory the VM then stalls for minutes
# while the host swaps. So the default shrinks to what the host can spare
# (3 GiB left to it), never below what the suites need: 4 GiB, 8 GiB for
# e2e, whose builders are each limited to 8 GiB (RANIKUST_E2E_MEMORY).
avail=$(($(sed -n 's/^MemAvailable: *\([0-9]*\) kB/\1/p' /proc/meminfo) / 1024))
mem_floor=4096
{ wants e2e || wants ultra; } && mem_floor=8192
if [ -n "${RANIKUST_VM_MEM:-}" ]; then
  MEM=$RANIKUST_VM_MEM mem_how=set
else
  MEM=16384 mem_how=default
  if [ "$MEM" -gt $((avail - 3072)) ]; then
    MEM=$(((avail - 3072) / 1024 * 1024))
    [ "$MEM" -ge "$mem_floor" ] || MEM=$mem_floor
    mem_how="capped: the host had $avail MiB available"
    log "the host has ${avail} MiB available: the VM gets $MEM MiB"
  fi
fi
[ "$MEM" -le $((avail - 2048)) ] ||
  log "warning: $MEM MiB for the VM leaves the host under 2 GiB of its ${avail} MiB available; the VM may stall (close memory-hungry programs, or set RANIKUST_VM_MEM)"
VM_SIZE="cpus=$CPUS ($cpus_how) mem=${MEM}MiB ($mem_how) disk=$DISK base=$([ "$GOLDEN_ON" = 1 ] && echo provisioned || echo cloud-image)"

if wants ultra; then
  # Next to the main checkout, also when this is a git worktree.
  NEXTJS="${RANIKUST_NEXTJS_KANIKO:-$(dirname "$MAIN")/nextjs-kaniko}"
  [ -f "$NEXTJS/bench/run-fair.sh" ] || die "no nextjs-kaniko checkout at $NEXTJS (set RANIKUST_NEXTJS_KANIKO)"
fi
if wants e2e || wants ultra; then
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
# The quick smoke cases and their runner.
cp "$REPO/tests/smoke/cases.bash" "$STAGE/guest/smoke-cases.bash"
cp "$REPO/tests/smoke/oci-config.py" "$STAGE/guest/oci-config.py"
cp -r "$REPO/tests/smoke/cases" "$STAGE/guest/cases"
# Quoted: `RANIKUST_QUICK_CASES="run copy"` names two cases.
: >"$STAGE/quick.env"
for v in RANIKUST_QUICK_CASES RANIKUST_QUICK_MODE; do
  [ -z "${!v:-}" ] || printf 'export %s=%q\n' "$v" "${!v}" >>"$STAGE/quick.env"
done
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
if wants smoke || wants quick || wants e2e || wants ultra || wants push || wants secrets; then
  log "building the smoke-build example and the ranikust binary"
  (cd "$REPO" && zigbuild zigbuild --release -p ranikust --example smoke-build --bin ranikust)
  cp "$TARGET/$MUSL/release/examples/smoke-build" "$TARGET/$MUSL/release/ranikust" "$STAGE/bin/"
fi

if wants e2e || wants ultra; then
  # The reference images, saved once from the host's podman (where
  # nextjs-kaniko's harness built or pulled them) and kept in the cache;
  # the npm proxy only for ultra.
  E2E_IMAGES=(localhost/kaniko-bench:stream docker.io/library/registry:2
    docker.io/moby/buildkit@sha256:f8a833b2de9d68e27f0815e4a737abdfaf8a2e4c615650557df11025101557b4)
  E2E_NAMES=(localhost/kaniko-bench:stream registry:2 rnk-e2e/buildkit:v0.33.1-rootless)
  if wants ultra; then
    E2E_IMAGES+=(docker.io/verdaccio/verdaccio:6.1.6)
    E2E_NAMES+=(verdaccio/verdaccio:6.1.6)
  fi
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
  # Quoted: `RANIKUST_E2E_BUILDERS="kaniko rk-kaniko"` names two builders.
  : >"$STAGE/e2e.env"
  # Not the profile: each suite sets its own (guest/ultra.sh).
  for v in $(compgen -e | grep '^RANIKUST_E2E_' | grep -vx RANIKUST_E2E_PROFILE || true); do
    printf 'export %s=%q\n' "$v" "${!v}" >>"$STAGE/e2e.env"
  done
fi
if wants push; then
  mkdir -p "$STAGE/registry"
  cp "$REGDATA/certs/ca.crt" "$REGDATA/auth/credentials" "$STAGE/registry/"
fi
# Read by guest/run.sh: the suites pull Docker Hub images from here; the
# VM's size goes into the results' env.txt.
printf 'export RANIKUST_VM_IMAGE_REGISTRY=%q\nexport RANIKUST_VM_SIZE=%q\n' "$IMAGE_REGISTRY" "$VM_SIZE" >"$STAGE/vm.env"

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
MONITOR=
# Seconds since the start at each step, written to the results' timings.txt.
T_BOOT='' T_SSH='' T_READY='' T_SUITES=''

teardown() {
  local status=$?
  set +e
  # The shared SSH connection, if one was opened.
  [ -z "$SOCKDIR" ] || [ ! -S "$SOCKDIR/ssh" ] || ssh -S "$SOCKDIR/ssh" -O exit rnk-vm >/dev/null 2>&1
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
  cp "$RUN/golden/serial.log" "$RESULTS/base-image-serial.log" 2>/dev/null
  printf 'size: %s\nbase image: %s\naio: %s\nboot started: %s s\nSSH up: %s s\ncloud-init done: %s s\nsuites done: %s s\ntotal: %s s\n' \
    "$VM_SIZE" "${BASE_IMG:-}" "${QEMU_AIO:-}" "$T_BOOT" "$T_SSH" "$T_READY" "$T_SUITES" "$SECONDS" >"$RESULTS/timings.txt"
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

# start_qemu <disk> <seed.iso> <serial log> <pidfile> <monitor> <mem> <cache>
#            [extra QEMU arguments...]
# Starts the VM detached; QEMU_PID and QEMU_AIO tell which. All virtio: the
# disk on its own I/O thread, through io_uring where this QEMU and kernel
# allow it (else the thread pool), an entropy source (no boot waits for
# entropy), a balloon that hands freed memory back to the host.
start_qemu() {
  local disk=$1 seed=$2 serial=$3 pidf=$4 mon=$5 mem=$6 cache=$7 aio
  shift 7
  for aio in io_uring threads; do
    rm -f "$pidf"
    if qemu-system-x86_64 \
      -name ranikust-vm -enable-kvm -cpu host -smp "$CPUS" -m "$mem" -machine q35,accel=kvm \
      -object iothread,id=io0 \
      -drive file="$disk",if=none,id=disk0,format=qcow2,cache="$cache",aio="$aio",discard=unmap,detect-zeroes=unmap \
      -device virtio-blk-pci,drive=disk0,iothread=io0 \
      -drive file="$seed",if=virtio,format=raw,readonly=on \
      -object rng-random,id=rng0,filename=/dev/urandom -device virtio-rng-pci,rng=rng0 \
      -device virtio-balloon-pci,free-page-reporting=on \
      -display none -serial file:"$serial" -monitor unix:"$mon",server,nowait \
      "$@" -pidfile "$pidf" -daemonize 2>"$RUN/qemu.err"; then
      MONITOR=$mon QEMU_PID="$(cat "$pidf")" QEMU_AIO=$aio
      return 0
    fi
    [ "$aio" = threads ] || log "QEMU did not start with aio=io_uring ($(head -c 300 "$RUN/qemu.err")); trying aio=threads"
  done
  cat "$RUN/qemu.err" >&2
  return 1
}

# A seed's user-data: @PACKAGES@ becomes cloud-init's package list, or
# nothing when the base image already holds the packages.
packages_yaml() { printf 'package_update: true\npackages:\n'; printf '  - %s\n' "${PACKAGES[@]}"; }
render() { # <template> <file holding what replaces @PACKAGES@>
  awk -v f="$2" '$0 == "@PACKAGES@" { while ((getline l < f) > 0) print l; close(f); next } { print }' "$1"
}

check_free_gib() { # <dir> <GiB>
  local free
  free=$(($(df -Pk "$1" | awk 'NR == 2 { print $4 }') / 1024 / 1024))
  [ "$free" -ge "$2" ] || die "only ${free} GiB free under $1 (needs $2 GiB)"
}

# --- Base image: the cloud image with the packages, built once and cached ------
#
# Booting the plain cloud image costs every run cloud-init's package
# installation (and snapd's first-boot seeding). The provisioned base image
# is that boot done once: cloud-init/provision installs the packages, removes
# what is per machine (SSH host keys, machine ID) and powers the VM off;
# the disk is then flattened into one qcow2 in the cache, read-only, with its
# SHA-256 beside it. Its name holds a hash of what it is made from (release,
# the cloud image's SHA-256, the package list, the provisioning seed), so a
# new cloud image or a changed seed builds a new one, and the old one of the
# release is deleted. Each run boots a throwaway overlay on it with its own
# seed (new instance ID: new host keys, user rnk with the run's own key).
BASE_IMG=$IMG
if [ "$GOLDEN_ON" = 1 ]; then
  gkey="$({ echo base-image-v1 "$RELEASE" "$want" "${PACKAGES[*]}"; cat "$HERE/cloud-init/provision"; } | sha256sum | cut -c1-16)"
  GOLDEN="$CACHE/base-$RELEASE-$gkey.qcow2"
  build_golden() {
    local g="$RUN/golden" t0=$SECONDS
    log "building the provisioned base image $GOLDEN (once per cloud image and provisioning seed)"
    # The flattened image (about 3 GiB) in the cache, the provisioning disk
    # (about 2 GiB) in the run directory.
    check_free_gib "$CACHE" 6
    check_free_gib "$RUN" 4
    mkdir -p "$g/seed"
    packages_yaml >"$g/packages.yaml"
    render "$HERE/cloud-init/provision" "$g/packages.yaml" >"$g/seed/user-data"
    printf 'instance-id: ranikust-base-%s\nlocal-hostname: ranikust-base\n' "$gkey" >"$g/seed/meta-data"
    xorriso -as mkisofs -quiet -output "$g/seed.iso" -volid cidata -joliet -rock "$g/seed/user-data" "$g/seed/meta-data"
    qemu-img create -q -f qcow2 -F qcow2 -b "$IMG" "$g/disk.qcow2" "${GOLDEN_GIB}G"
    start_qemu "$g/disk.qcow2" "$g/seed.iso" "$g/serial.log" "$g/qemu.pid" "$SOCKDIR/base.sock" 4096 writeback \
      -netdev user,id=n0 -device virtio-net-pci,netdev=n0 || die "QEMU did not start"
    # The VM powers itself off once cloud-init is done (power_state).
    local deadline=$((SECONDS + 1200))
    while kill -0 "$QEMU_PID" 2>/dev/null; do
      [ "$SECONDS" -lt "$deadline" ] || die "provisioning the base image took over 20 minutes (serial log: $RESULTS/base-image-serial.log)"
      sleep 2
    done
    QEMU_PID=
    # The last line provisioning prints (its own line, so not the command's
    # text if cloud-init ever echoed it).
    awk '{ sub(/\r$/, "") } $0 == "ranikust-golden-ready" { ok = 1 } END { exit !ok }' "$g/serial.log" ||
      die "provisioning the base image failed (serial log: $RESULTS/base-image-serial.log)"
    # Flattened, so that it no longer needs the cloud image; checked, hashed,
    # read-only, then put in place under its final name.
    rm -f "$GOLDEN.part" "$GOLDEN.sha256.part"
    qemu-img convert -q -O qcow2 "$g/disk.qcow2" "$GOLDEN.part"
    qemu-img check -q "$GOLDEN.part"
    sha256sum "$GOLDEN.part" | cut -d' ' -f1 >"$GOLDEN.sha256.part"
    chmod 0444 "$GOLDEN.part"
    find "$CACHE" -maxdepth 1 -type f -name "base-$RELEASE-*.qcow2*" ! -name "base-$RELEASE-$gkey.*" -delete
    mv -f "$GOLDEN.part" "$GOLDEN"
    mv -f "$GOLDEN.sha256.part" "$GOLDEN.sha256"
    rm -rf "$g"
    log "base image ready after $((SECONDS - t0)) s ($(du -h "$GOLDEN" | cut -f1))"
  }
  if [ ! -f "$GOLDEN" ] || [ ! -f "$GOLDEN.sha256" ]; then
    build_golden
  elif [ "$(sha256sum "$GOLDEN" | cut -d' ' -f1)" != "$(cat "$GOLDEN.sha256")" ]; then
    log "the base image no longer matches its SHA-256: building it again"
    rm -f "$GOLDEN" "$GOLDEN.sha256"
    build_golden
  fi
  log "base image $(basename "$GOLDEN") sha256 $(cat "$GOLDEN.sha256") (verified)"
  BASE_IMG=$GOLDEN
fi

# --- Boot ----------------------------------------------------------------------

ssh-keygen -q -t ed25519 -N '' -C "ranikust-vm" -f "$RUN/key"
mkdir "$RUN/seed"
# The guest's Docker pulls docker.io images through the same mirror.
if [ "$IMAGE_REGISTRY" = docker.io ]; then MIRRORS='[]'; else MIRRORS="[\"https://$IMAGE_REGISTRY\"]"; fi
if [ "$GOLDEN_ON" = 1 ]; then : >"$RUN/packages.yaml"; else packages_yaml >"$RUN/packages.yaml"; fi
render "$HERE/cloud-init/user-data" "$RUN/packages.yaml" |
  sed -e "s|@SSH_KEY@|$(cat "$RUN/key.pub")|" -e "s|@REGISTRY_MIRRORS@|$MIRRORS|" >"$RUN/seed/user-data"
printf 'instance-id: ranikust-vm-%s\nlocal-hostname: ranikust-vm\n' "$(basename "$RUN")" >"$RUN/seed/meta-data"
xorriso -as mkisofs -quiet -output "$RUN/seed.iso" -volid cidata -joliet -rock \
  "$RUN/seed/user-data" "$RUN/seed/meta-data"
# Again: the builds and downloads above took space since the first check.
check_host_free
# The base image is only ever a backing file: QEMU opens it read-only, and
# every write of the run goes to this overlay, deleted with the run.
qemu-img create -q -f qcow2 -F qcow2 -b "$BASE_IMG" "$RUN/disk.qcow2" "$DISK"

PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
log "booting $RELEASE ($VM_SIZE), SSH on 127.0.0.1:$PORT"
T_BOOT=$SECONDS
# The disk is thrown away with the run: cache=unsafe skips the guest's
# flushes to the host's disk.
start_qemu "$RUN/disk.qcow2" "$RUN/seed.iso" "$RUN/serial.log" "$RUN/qemu.pid" "$SOCKDIR/monitor.sock" "$MEM" unsafe \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:"$PORT"-:22 -device virtio-net-pci,netdev=n0 || die "QEMU did not start"

# One SSH connection shared by every command (no handshake per poll); a
# dropped one is opened again by the next command.
SSH_OPTS=(-i "$RUN/key" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR -o ConnectTimeout=30 -o ServerAliveInterval=15 -o ServerAliveCountMax=8
  -o ControlMaster=auto -o ControlPath="$SOCKDIR/ssh" -o ControlPersist=120)
vssh() { ssh "${SSH_OPTS[@]}" -p "$PORT" "$VM_USER@127.0.0.1" "$@"; }

ssh_deadline=$((SECONDS + 600))
until vssh true 2>/dev/null; do
  [ "$SECONDS" -lt "$ssh_deadline" ] || die "no SSH after 10 minutes (serial log: $RESULTS/serial.log)"
  sleep 1
done
T_SSH=$SECONDS
log "SSH up $((T_SSH - T_BOOT)) s after boot; waiting for cloud-init"
vssh 'cloud-init status --wait >/dev/null; test -e /var/lib/ranikust-vm-ready' ||
  die "cloud-init failed (serial log: $RESULTS/serial.log)"
T_READY=$SECONDS
log "cloud-init done $((T_READY - T_BOOT)) s after boot"

log "copying in $(du -sh "$STAGE" | cut -f1)"
tar -C "$STAGE" -cf - . | vssh 'mkdir rnk && tar -C rnk -xf -'
if wants ultra; then
  log "copying in the ultra seed (images, registry, npm proxy storage, workloads)"
  python3 -I "$HERE/e2e-seed.py" "$NEXTJS" "$CACHE/e2e" "$STAGE/e2e-images.txt" |
    vssh 'mkdir -p rnk/e2e && tar -C rnk/e2e -xf -'
elif wants e2e; then
  log "copying in the e2e seed (the reference images)"
  python3 -I "$HERE/e2e-seed.py" --images-only "$CACHE/e2e" "$STAGE/e2e-images.txt" |
    vssh 'mkdir -p rnk/e2e && tar -C rnk/e2e -xf -'
fi

# The suites run detached in the guest, so that a dropped SSH connection
# (the VM stalled while the host swapped) loses neither the run nor its
# results: the host follows the log and reconnects.
vssh "setsid nohup bash -c 'bash rnk/guest/run.sh ${SUITES[*]} >rnk/suite.log 2>&1; echo \$? >rnk/suite.rc' </dev/null >/dev/null 2>&1 &"
shown=0 lost=0
while :; do
  # The whole log each time (small), printed from where the last poll
  # stopped; then the exit status once the suites have finished.
  # Each command succeeds in the guest whether or not the files exist yet,
  # so a failure here is SSH itself.
  if vssh 'cat rnk/suite.log 2>/dev/null; true' >"$RUN/suite.log" &&
     rc="$(vssh 'cat rnk/suite.rc 2>/dev/null; true')"; then
    lost=0
    size=$(stat -c %s "$RUN/suite.log")
    [ "$size" -le "$shown" ] || { tail -c +$((shown + 1)) "$RUN/suite.log"; shown=$size; }
    [ -z "$rc" ] || { status=$rc; break; }
  else
    lost=$((lost + 1))
    log "no SSH to the VM ($lost); the suites keep running"
    [ "$lost" -lt 180 ] || die "the VM stopped answering SSH for 15 minutes"
  fi
  sleep 5
done
T_SUITES=$SECONDS
log "suites done $((T_SUITES - T_READY)) s after cloud-init"
mkdir -p "$RESULTS"
for i in 1 2 3 4 5; do
  # The guest's results hold only regular files and directories: a link
  # could point the host's tar outside $RESULTS, a device or FIFO has no
  # business there. A builder container left running could still write, so
  # those go first. The guest deletes everything else, and the host again
  # after every attempt, failed or not (tar still creates the links it
  # delayed after an error); the host's tar refuses absolute names and `..`.
  copied=0
  if vssh 'c=$(sudo docker ps -aq --filter name=rnk-e2e- 2>/dev/null); [ -z "$c" ] || sudo docker rm -f $c >/dev/null
    sudo find rnk/out ! -type f ! -type d -delete && tar -C rnk/out -cf - .' |
    tar -C "$RESULTS" --no-same-owner --no-same-permissions -xf -; then
    copied=1
  fi
  find "$RESULTS" ! -type f ! -type d -delete
  [ "$copied" = 1 ] && break
  [ "$i" = 5 ] && log "could not copy the results out"
  sleep 10
done
log "results in $RESULTS"
exit "$status"
