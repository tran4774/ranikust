#!/usr/bin/env bash
# Runs in the T2 VM: container images built by kaniko and BuildKit as
# references and by ranikust, each build timed as nextjs-kaniko's
# bench/run-fair.sh times it (start to exit of the build), with a local
# registry inside the VM. The images of the first round are flattened and
# compared (path, type, mode, owner, size, sha256, link, xattrs, config) and
# checked for leaks; kaniko's and ranikust's metrics reports are put side by
# side.
#
# Two profiles (docs/testing.md#which-test-for-what):
# - light (`just test-vm e2e`, the default): W0, a few small Dockerfiles in
#   guest/e2e/w0 on a base image pushed by guest/e2e/base-w0.py (busybox,
#   nothing pulled from a network): a few seconds per build, the whole
#   pipeline; results in ~/rnk/out/e2e/ (target/vm/results/<release>/e2e/);
# - ultra (`just test-vm ultra`, guest/ultra.sh): the Phase 1 workloads of
#   nextjs-kaniko (W1 app, W2 cold app-cache, W3 app-par) with the verdaccio
#   npm proxy; tens of minutes per round; only when the maintainer asks for
#   performance numbers or an acceptance run. Results in ~/rnk/out/ultra/.
#
# A ranikust build that stops because something is not built yet ("not
# implemented", a flag refused until a later phase) is recorded as pending and
# does not fail the suite; any other failure does.
#
# Seed (copied in by tests/vm/vm.sh from tests/vm/e2e-seed.py): ~/rnk/e2e/
# with images/ and images.txt; for ultra also registry/ (the base and tools
# images with their original digests), verdaccio/ (the npm proxy's storage)
# and nk/ (the workloads and the files of nextjs-kaniko/bench it needs).
#
# Builders: the references `kaniko` and `buildkit`; `ranikust` in overlay
# mode as root on the VM; `rk-kaniko`, ranikust in kaniko mode on the `/` of
# a default `docker run` (root, Docker's default capabilities, no option:
# the shared root, ADR-0048); `rk-kaniko-host`, ranikust in kaniko mode as
# root on the VM itself, one directory per stage in its state directory
# (ADR-0059). The ranikust builders' images are also checked for what must
# never reach a layer (guest/e2e/leaks.py).
#
# Options (RANIKUST_E2E_* on the host, written to ~/rnk/e2e.env):
#   WORKLOADS  light: "w0"; ultra: "w1 w2 w3" (w0 too if named)
#   BUILDERS   default "kaniko buildkit ranikust rk-kaniko rk-kaniko-host"
#   ROUNDS     default 1                  WARMUP    light 0, ultra 1 (one discarded kaniko build per workload)
#   CPUS       default 4                  MEMORY    default 8g (as run-fair.sh)
#   CPUSET     the vCPUs every builder is pinned to, the same for all (a
#              container's --cpuset-cpus, a scope's AllowedCPUs): default
#              the VM's last CPUS vCPUs when it has CPUS + 2 or more, the
#              registry and the npm proxy on the others; `none` pins nothing
#              (the quota alone, as run-fair.sh)
#   JOBS       light "1 default", ultra "1 2 default": --jobs values on W0
#              and W3 (with --reproducible) of `ranikust` and `rk-kaniko-host`
#   CHECK_ALL  1: flatten, compare and check every round's images; by
#              default only the first round's, later rounds are timed only
#   MIN_FREE_GB  default 10: the guest's free disk below which no build
#              starts (after pruning the suite fails instead of filling it)
set -euo pipefail
cd "$HOME/rnk"
# The profile is the suite's (ultra.sh sets it), never the host's e2e.env.
PROFILE=${RANIKUST_E2E_PROFILE:-light}
# shellcheck disable=SC1091
[ -f e2e.env ] && source e2e.env
case "$PROFILE" in
  light) DEF_WORKLOADS=w0 DEF_WARMUP=0 DEF_JOBS="1 default" OUT="$HOME/rnk/out/e2e" ;;
  ultra) DEF_WORKLOADS="w1 w2 w3" DEF_WARMUP=1 DEF_JOBS="1 2 default" OUT="$HOME/rnk/out/ultra" ;;
  *) echo "error: unknown e2e profile '$PROFILE'" >&2; exit 2 ;;
esac

E="$HOME/rnk/e2e"
NK="$E/nk"
W=/var/lib/rnk-e2e
BIN="$HOME/rnk/bin/ranikust"
PY=(python3 -I)
WORKLOADS=(${RANIKUST_E2E_WORKLOADS:-$DEF_WORKLOADS})
BUILDERS=(${RANIKUST_E2E_BUILDERS:-kaniko buildkit ranikust rk-kaniko rk-kaniko-host})
ROUNDS=${RANIKUST_E2E_ROUNDS:-1}
WARMUP=${RANIKUST_E2E_WARMUP:-$DEF_WARMUP}
CPUS=${RANIKUST_E2E_CPUS:-4}
MEMORY=${RANIKUST_E2E_MEMORY:-8g}
JOBS=(${RANIKUST_E2E_JOBS:-$DEF_JOBS})
CHECK_ALL=${RANIKUST_E2E_CHECK_ALL:-0}
MIN_FREE_GB=${RANIKUST_E2E_MIN_FREE_GB:-10}
# The options reach sudo command lines and arithmetic: whole numbers only
# (MEMORY a number with an optional k, m or g; JOBS also `default`).
bad_opt() { echo "error: RANIKUST_E2E_$1 must be $2 (got '$3')" >&2; exit 2; }
# (No leading zero: bash would read it as octal.)
for v in ROUNDS CPUS MIN_FREE_GB; do
  [[ "${!v}" =~ ^[1-9][0-9]*$ ]] || bad_opt "$v" "a whole number of at least 1" "${!v}"
done
[[ "$WARMUP" =~ ^[01]$ ]] || bad_opt WARMUP "0 or 1" "$WARMUP"
[[ "$MEMORY" =~ ^[0-9]+[kKmMgG]?$ ]] || bad_opt MEMORY "a number with k, m or g" "$MEMORY"
for j in "${JOBS[@]}"; do
  [[ "$j" =~ ^([1-9][0-9]*|default)$ ]] || bad_opt JOBS "positive whole numbers or default" "$j"
done
[[ "$CHECK_ALL" =~ ^[01]$ ]] || bad_opt CHECK_ALL "0 or 1" "$CHECK_ALL"
for w in "${WORKLOADS[@]}"; do
  case "$PROFILE:$w" in
    light:w0 | ultra:w0 | ultra:w1 | ultra:w2 | ultra:w3) ;;
    light:w1 | light:w2 | light:w3) bad_opt WORKLOADS "w0 in the light e2e (W1-W3 are the ultra benchmark: just test-vm ultra)" "$w" ;;
    *) bad_opt WORKLOADS "w0, w1, w2 or w3" "$w" ;;
  esac
done
for b in "${BUILDERS[@]}"; do
  case "$b" in kaniko | buildkit | ranikust | rk-kaniko | rk-kaniko-host) ;; *) bad_opt BUILDERS "known builders" "$b" ;; esac
done

# Every builder gets the same budget, whether Docker or a systemd scope
# enforces it: CPUS CPUs of quota and MEMORY of memory (and as much swap:
# what Docker gives a container with --memory alone), on the same pinned
# vCPUs. Pinned, because a tool sizes its threads from what it sees: with
# a 12-vCPU VM, Node or Go in a container would see 12 CPUs for a quota of
# 4 and be throttled (Rust's available_parallelism reads the quota).
NVCPU=$(nproc)
CPUSET=${RANIKUST_E2E_CPUSET:-}
if [ -z "$CPUSET" ]; then
  if [ "$NVCPU" -ge $((CPUS + 2)) ]; then CPUSET="$((NVCPU - CPUS))-$((NVCPU - 1))"; else CPUSET=none; fi
fi
SVC_CPUSET=
if [ "$CPUSET" = none ]; then
  CPUSET=
elif [[ "$CPUSET" =~ ^[0-9]+(-[0-9]+)?(,[0-9]+(-[0-9]+)?)*$ ]]; then
  # The services stay off the builders' vCPUs when the default set is used.
  [ -n "${RANIKUST_E2E_CPUSET:-}" ] || SVC_CPUSET="0-$((NVCPU - CPUS - 1))"
else
  bad_opt CPUSET "a cpuset list such as 8-11, or none" "$CPUSET"
fi
# The analysis between builds reads layers on the vCPUs a build would not get.
export RANIKUST_FLATTEN_JOBS=$((NVCPU > CPUS ? NVCPU - CPUS : 1))
DOCKER_LIMITS=(--cpus="$CPUS" --memory="$MEMORY" ${CPUSET:+--cpuset-cpus="$CPUSET"})
SCOPE_LIMITS=(-p CPUQuota="${CPUS}00%" -p MemoryMax="${MEMORY^^}" -p MemorySwapMax="${MEMORY^^}" ${CPUSET:+-p AllowedCPUs="$CPUSET"})
SVC_LIMITS=(${SVC_CPUSET:+--cpuset-cpus="$SVC_CPUSET"})

# Empties a state directory of the VM's builds: never while anything is
# mounted below it (a build cut short), and never across a filesystem.
reset_state() {
  local dir="$W/$1"
  if grep -qF " $dir/" /proc/self/mountinfo || grep -qF " $dir " /proc/self/mountinfo; then
    echo "error: mounts left under $dir; not deleting it" >&2
    exit 1
  fi
  sudo rm -rf --one-file-system "$dir"
}
KANIKO_IMAGE=localhost/kaniko-bench:stream
BK_IMAGE=rnk-e2e/buildkit:v0.33.1-rootless
# An empty image for rk-kaniko: ranikust is static and builds over its `/`.
EMPTY_IMAGE=rnk-e2e/empty:latest
REPOS="$W/registry/docker/registry/v2/repositories"

declare -A CTX=([w0]="$HOME/rnk/guest/e2e/w0" [w1]="$NK/app" [w2]="$NK/app-cache" [w3]="$NK/app-par")
# The base each workload names (BASE_IMAGE): W0's is pushed below, W1-W3's
# comes with the seed's registry.
declare -A BASE_OF=()
# W1-W3 need nextjs-kaniko's seed and the npm proxy; W0 neither.
NEEDS_NK=0
for w in "${WORKLOADS[@]}"; do [ "$w" = w0 ] || NEEDS_NK=1; done
if [ "$NEEDS_NK" = 1 ]; then
  for w in w1 w2 w3; do BASE_OF[$w]="$(cat "$NK/bench/base-image.txt")"; done
fi
# Workloads with stages that may run side by side: their --jobs variants.
takes_jobs() { case "$1" in w0 | w3) return 0 ;; *) return 1 ;; esac; }
# kaniko's options of run-fair.sh (cfg-final-opt.env), when the seed has them.
KANIKO_OPTS=()
[ ! -f "$NK/bench/cfg-final-opt.env" ] || read -r -a KANIKO_OPTS <<<"$(tr '\n' ' ' <"$NK/bench/cfg-final-opt.env")"

log() { printf '[e2e] %s\n' "$*"; }
now() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(now)" 'BEGIN { printf "%.1f", b - a }'; }
mkdir -p "$OUT"

# The analysis of an image (flatten, leaks, blob check) runs after its build
# has been timed and before the next build starts, never beside one. It
# reads each layer once per run into a cache (a base layer every builder's
# image shares is read once), on the vCPUs the builds are not pinned to.
export RANIKUST_FLATTEN_CACHE="$HOME/rnk/flatten-cache"
rm -rf "$RANIKUST_FLATTEN_CACHE"
# How long each step took, per build: workload, build, step, seconds.
PHASES="$OUT/phases.tsv"
printf 'workload\tbuild\tstep\tseconds\n' >"$PHASES"
phase() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >>"$PHASES"; }
T_SUITE=$(now)

# --- Services: the images, a registry and verdaccio, as docker-compose.yml ----

command -v zstd >/dev/null || { log "installing zstd"; sudo apt-get install -qq -y zstd >/dev/null; }
log "loading the reference images"
while read -r id name; do
  loaded="$(sudo docker load -q -i "$E/images/$id.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
  [ -n "$loaded" ] || { echo "error: docker load of $name gave no image" >&2; exit 1; }
  sudo docker tag "$loaded" "$name"
done <"$E/images.txt"
tar -cf - -T /dev/null | sudo docker import - "$EMPTY_IMAGE" >/dev/null

sudo rm -rf "$W" && sudo mkdir -p "$W"
SERVICES=(nk-registry)
if [ "$NEEDS_NK" = 1 ]; then
  sudo mv "$E/registry" "$W/registry"
  sudo mv "$E/verdaccio" "$W/verdaccio"
  sudo chown -R 10001:65533 "$W/verdaccio" # the verdaccio image's user
else
  sudo mkdir -p "$W/registry"
fi
sudo docker network create --internal nk-internal >/dev/null
sudo docker network create nk-egress >/dev/null
sudo docker run -d --name nk-registry --network nk-egress -p 127.0.0.1:5000:5000 "${SVC_LIMITS[@]}" \
  -e REGISTRY_STORAGE_DELETE_ENABLED=true -v "$W/registry:/var/lib/registry" registry:2 >/dev/null
sudo docker network connect --alias registry nk-internal nk-registry
if [ "$NEEDS_NK" = 1 ]; then
  SERVICES+=(nk-verdaccio)
  sudo docker run -d --name nk-verdaccio --network nk-egress -p 127.0.0.1:4873:4873 "${SVC_LIMITS[@]}" \
    -v "$NK/verdaccio/config.yaml:/verdaccio/conf/config.yaml:ro" -v "$W/verdaccio:/verdaccio/storage" \
    verdaccio/verdaccio:6.1.6 >/dev/null
  sudo docker network connect --alias verdaccio nk-internal nk-verdaccio
fi
# ranikust runs on the VM itself and its RUN shares the VM's network (ADR-0010).
echo "127.0.0.1 registry verdaccio" | sudo tee -a /etc/hosts >/dev/null
wait_up() { for _ in $(seq 120); do curl -fs -o /dev/null "$1" && return 0; sleep 0.5; done; echo "error: $1 is not up" >&2; exit 1; }
wait_up http://127.0.0.1:5000/v2/
[ "$NEEDS_NK" = 0 ] || wait_up http://127.0.0.1:4873/-/ping
# W0's base, made here from the VM's static busybox (base-w0.py).
for w in "${WORKLOADS[@]}"; do
  [ "$w" = w0 ] || continue
  # Static (busybox-static in the base image): the image has no libc.
  ! ldd /bin/busybox >/dev/null 2>&1 || { echo "error: /bin/busybox is not static" >&2; exit 1; }
  BASE_OF[w0]="$("${PY[@]}" guest/e2e/base-w0.py 127.0.0.1:5000 rnk-base/w0 /bin/busybox)"
done

TOML="$OUT/buildkitd.toml"
printf '[registry."registry:5000"]\n  http = true\n  insecure = true\n[dns]\n  nameservers = ["127.0.0.11"]\n' >"$TOML"
chmod 644 "$TOML"

{
  echo "profile: $PROFILE"
  for w in "${WORKLOADS[@]}"; do echo "base image of $w: ${BASE_OF[$w]}"; done
  echo "kaniko: $KANIKO_IMAGE ($(sudo docker image inspect -f '{{.Id}}' "$KANIKO_IMAGE")), ${KANIKO_OPTS[*]:-no cfg-final-opt.env} + FF_KANIKO_STREAM_TAR_COMPRESS=1"
  echo "buildkit: $BK_IMAGE ($(sudo docker image inspect -f '{{.Id}}' "$BK_IMAGE")), zstd"
  echo "ranikust: $("$BIN" version 2>/dev/null | head -1) (sha256 $(sha256sum "$BIN" | cut -d' ' -f1))"
  echo "kernel: $(uname -r); cpus: $(nproc); mem: $(awk '/MemTotal/ {print $2}' /proc/meminfo) kB; disk free: $(df -Ph / | awk 'NR == 2 { print $4 }')"
  echo "limits: --cpus=$CPUS --memory=$MEMORY --cpuset-cpus=${CPUSET:-none} (ranikust, rk-kaniko-host: a systemd scope, CPUQuota=${CPUS}00% MemoryMax=MemorySwapMax=${MEMORY^^} AllowedCPUs=${CPUSET:-all}); registry and npm proxy on vCPUs ${SVC_CPUSET:-any}; each build's own in <build>/limits.txt and run.json"
  echo "workloads: ${WORKLOADS[*]}; builders: ${BUILDERS[*]}; rounds: $ROUNDS; warm-up: $WARMUP; jobs on W0 and W3: ${JOBS[*]}; content checks: $([ "$CHECK_ALL" = 1 ] && echo every round || echo round 1)"
} | tee "$OUT/env.txt"

# --- One build ----------------------------------------------------------------

# Every build starts from an empty e2e/ namespace in a restarted registry, as
# run-fair.sh wipes its repositories, so nothing is "already there".
# The registry frees no blob when a repository goes, so the blobs nothing
# links any more are deleted while it is stopped (guest/e2e/prune-registry.py):
# without that every build's layers stayed and filled the guest's disk.
wipe() {
  sudo rm -rf "$REPOS/e2e"
  sudo docker stop nk-registry >/dev/null
  sudo "${PY[@]}" guest/e2e/prune-registry.py "$W/registry" >>"$OUT/prune.log"
  sudo docker start nk-registry >/dev/null
  # Hands the freed blocks back to the host (the VM's disk has discard).
  sudo fstrim / >/dev/null 2>&1 || true
  wait_up http://127.0.0.1:5000/v2/
  guard_disk
}

free_gb() { df -Pk / | awk 'NR == 2 { print int($4 / 1048576) }'; }

# Fails the suite, rather than letting the guest's disk fill (a full disk
# stops sshd and the host loses the VM), when less than MIN_FREE_GB is
# left after dropping what earlier builds left behind.
guard_disk() {
  [ "$(free_gb)" -lt "$MIN_FREE_GB" ] || return 0
  sudo docker container prune -f >/dev/null
  sudo docker image prune -f >/dev/null
  reset_state rnk-work || exit 1
  reset_state rnk-kwork || exit 1
  local f
  f=$(free_gb)
  [ "$f" -lt "$MIN_FREE_GB" ] || return 0
  echo "error: only $f GiB free in the VM (RANIKUST_E2E_MIN_FREE_GB=$MIN_FREE_GB): stopping before the disk fills" >&2
  df -h / >&2
  sudo du -sh "$W"/* /var/lib/docker >&2 2>/dev/null || true
  exit 1
}

LIMITS="$HOME/rnk/guest/e2e/limits.sh"
# container_limits <container> <dir>: the limits a container's build runs
# under, from its cgroup while it runs (Docker's settings if it has ended).
# mount_dir <dir>: the only directory of <dir> a builder container may
# write (its results, as /bench-out or /rnk-out); everything the harness
# writes goes to <dir> itself, which no container reaches.
mount_dir() { mkdir -p "$1/mnt" && echo "$1/mnt"; }
# take_results <dir> <container>: once the container is gone (nothing can
# write <dir>/mnt any more), copies its known result files out if they are
# regular files, never through a link, then drops the rest.
take_results() {
  local f
  if sudo docker inspect "$2" >/dev/null 2>&1; then
    echo "error: container $2 still exists; its results are not read" >&2
    return 1
  fi
  for f in metrics.json digest.txt; do
    if sudo test -f "$1/mnt/$f" && ! sudo test -L "$1/mnt/$f"; then
      # Read as root (O_NOFOLLOW), written as rnk into a directory no
      # container reaches.
      # shellcheck disable=SC2024
      sudo dd if="$1/mnt/$f" iflag=nofollow status=none >"$1/$f"
    fi
  done
  sudo rm -rf -- "$1/mnt"
}
container_limits() {
  local pid
  pid="$(sudo docker inspect -f '{{.State.Pid}}' "$1" 2>/dev/null || echo 0)"
  # Read as root, written as rnk.
  # shellcheck disable=SC2024
  [ "${pid:-0}" = 0 ] || sudo bash "$LIMITS" pid "$pid" >"$2/limits.txt" 2>/dev/null || true
  [ -s "$2/limits.txt" ] ||
    sudo docker inspect -f 'docker.cpus={{.HostConfig.NanoCpus}} docker.cpuset={{.HostConfig.CpusetCpus}} docker.memory={{.HostConfig.Memory}} docker.memory_swap={{.HostConfig.MemorySwap}}' "$1" |
    tr ' ' '\n' >"$2/limits.txt" 2>/dev/null || true
}

is_rnk() { case "$1" in ranikust | rk-*) return 0 ;; *) return 1 ;; esac; }
# The builders that take --jobs on W3 (their stages may run side by side).
has_jobs() { case "$1" in ranikust | rk-kaniko-host) return 0 ;; *) return 1 ;; esac; }

# build <workload> <builder> <jobs|-> <dir> <repository>; writes <dir>/run.json
build() {
  local w=$1 b=$2 jobs=$3 d=$4 repo=$5 ctx="${CTX[$1]}" base="${BASE_OF[$1]}" name="rnk-e2e-$RANDOM" vol= t0 t1 rc status reason=
  mkdir -p "$d"
  sync
  case "$b" in
    kaniko)
      # shellcheck disable=SC2046
      t0=$(now)
      sudo docker run -d --name "$name" --network nk-internal "${DOCKER_LIMITS[@]}" \
        "${KANIKO_OPTS[@]}" -e FF_KANIKO_STREAM_TAR_COMPRESS=1 \
        -v "$ctx:/workspace:ro" -v "$(mount_dir "$d"):/bench-out" -e KANIKO_METRICS_FILE=/bench-out/metrics.json "$KANIKO_IMAGE" \
        --dockerfile=/workspace/Dockerfile --context=dir:///workspace --destination="registry:5000/$repo:latest" \
        --insecure-registry=registry:5000 --build-arg "BASE_IMAGE=$base" --digest-file=/bench-out/digest.txt \
        --verbosity=info --log-timestamp=true >/dev/null
      container_limits "$name" "$d"
      rc=$(sudo docker wait "$name"); t1=$(now)
      sudo docker logs "$name" >"$d/build.log" 2>&1 || true
      sudo docker rm -f "$name" >/dev/null
      take_results "$d" "$name"
      ;;
    buildkit)
      vol="rnk-e2e-bk-$RANDOM"; sudo docker volume create "$vol" >/dev/null
      t0=$(now)
      sudo docker run -d --name "$name" --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
        --network nk-internal "${DOCKER_LIMITS[@]}" \
        -e BUILDKITD_FLAGS="--oci-worker-no-process-sandbox --config /home/user/.config/buildkit/buildkitd.toml" \
        -v "$vol:/home/user/.local/share/buildkit" -v "$ctx:/workspace:ro" \
        -v "$TOML:/home/user/.config/buildkit/buildkitd.toml:ro" --entrypoint buildctl-daemonless.sh "$BK_IMAGE" \
        build --progress plain --frontend dockerfile.v0 --local context=/workspace --local dockerfile=/workspace \
        --opt "build-arg:BASE_IMAGE=$base" \
        --output "type=image,\"name=registry:5000/$repo:latest\",push=true,compression=zstd" >/dev/null
      container_limits "$name" "$d"
      rc=$(sudo docker wait "$name"); t1=$(now)
      sudo docker logs "$name" >"$d/build.log" 2>&1 || true
      sudo docker rm -f "$name" >/dev/null
      sudo docker volume rm -f "$vol" >/dev/null
      ;;
    rk-kaniko)
      # A default `docker run`: root, Docker's capabilities, no option. The
      # results go to /rnk-out, not /out, which W3's Dockerfile writes: a
      # mount point is never part of a layer in kaniko mode, as in kaniko.
      local extra=()
      [ "$jobs" = - ] || extra+=(--reproducible)
      t0=$(now)
      sudo docker run -d --name "$name" --network nk-internal "${DOCKER_LIMITS[@]}" \
        -v "$BIN:/ranikust/bin/ranikust:ro" -v "$ctx:/workspace:ro" -v "$(mount_dir "$d"):/rnk-out" \
        -v /etc/ssl/certs:/etc/ssl/certs:ro "$EMPTY_IMAGE" \
        /ranikust/bin/ranikust build --mode=kaniko --context=/workspace --dockerfile=/workspace/Dockerfile \
        --destination="registry:5000/$repo:latest" --insecure-registry=registry:5000 \
        --build-arg="BASE_IMAGE=$base" --metrics-file=/rnk-out/metrics.json --digest-file=/rnk-out/digest.txt \
        "${extra[@]}" >/dev/null
      container_limits "$name" "$d"
      rc=$(sudo docker wait "$name"); t1=$(now)
      sudo docker logs "$name" >"$d/build.log" 2>&1 || true
      sudo docker rm -f "$name" >/dev/null
      take_results "$d" "$name"
      ;;
    rk-kaniko-host)
      local extra=()
      [ "$jobs" = - ] || [ "$jobs" = default ] || extra+=("--jobs=$jobs")
      [ "$jobs" = - ] || extra+=(--reproducible)
      reset_state rnk-kwork || exit 1
      sudo mkdir -m 700 -p "$W/rnk-kwork"
      t0=$(now)
      set +e
      sudo systemd-run --quiet --scope "${SCOPE_LIMITS[@]}" -- \
        bash "$LIMITS" self "$d/limits.txt" "$BIN" build --mode=kaniko --context="$ctx" --dockerfile="$ctx/Dockerfile" \
        --destination="registry:5000/$repo:latest" --insecure-registry=registry:5000 \
        --build-arg="BASE_IMAGE=$base" --state-dir="$W/rnk-kwork" \
        --metrics-file="$d/metrics.json" "${extra[@]}" >"$d/build.log" 2>&1
      rc=$?
      set -e
      t1=$(now)
      sudo chown -R "$(id -u):$(id -g)" "$d"
      ;;
    ranikust)
      local extra=()
      [ "$jobs" = - ] || [ "$jobs" = default ] || extra+=("--jobs=$jobs")
      [ "$jobs" = - ] || extra+=(--reproducible)
      reset_state rnk-work || exit 1
      sudo mkdir -m 700 -p "$W/rnk-work"
      t0=$(now)
      set +e
      sudo systemd-run --quiet --scope "${SCOPE_LIMITS[@]}" -- \
        bash "$LIMITS" self "$d/limits.txt" "$BIN" build --context="$ctx" --dockerfile="$ctx/Dockerfile" --destination="registry:5000/$repo:latest" \
        --insecure-registry=registry:5000 --build-arg="BASE_IMAGE=$base" --state-dir="$W/rnk-work" \
        --metrics-file="$d/metrics.json" "${extra[@]}" >"$d/build.log" 2>&1
      rc=$?
      set -e
      t1=$(now)
      sudo chown -R "$(id -u):$(id -g)" "$d"
      ;;
    *) echo "error: unknown builder $b" >&2; return 1 ;;
  esac
  status=ok
  if [ "$rc" != 0 ]; then
    status=failed
    if is_rnk "$b" && grep -Eiq 'not implemented|not yet|comes in Phase|\[open: Q-|\(Q-[0-9]+\)' "$d/build.log"; then
      status=pending
      reason="$(grep -Ei -m1 'not implemented|not yet|comes in Phase|Q-[0-9]+' "$d/build.log" | cut -c1-200)"
    fi
  fi
  local label=$b
  [ "$jobs" = - ] || label="$b-j$jobs"
  "${PY[@]}" - "$d/run.json" "$w" "$label" "$rc" "$t0" "$t1" "$status" "$reason" "$repo" <<'PY'
import json, os, sys
out, w, b, rc, t0, t1, status, reason, repo = sys.argv[1:10]
# The CPU and memory limits the build ran under (guest/e2e/limits.sh).
limits = {}
lf = os.path.join(os.path.dirname(out), "limits.txt")
if os.path.exists(lf):
    for line in open(lf):
        k, _, v = line.strip().partition("=")
        if k:
            limits[k] = v
json.dump({"workload": w, "builder": b, "exit_code": int(rc), "wall_secs": round(float(t1) - float(t0), 3),
           "status": status, "reason": reason, "repository": repo, "limits": limits}, open(out, "w"), indent=2)
PY
  log "$w $label: $status (exit $rc, $(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }') s)"
  [ ! -s "$d/limits.txt" ] || log "  limits: $(paste -sd' ' "$d/limits.txt")"
  [ "$status" != failed ] || { tail -20 "$d/build.log" >&2; return 1; }
}

# check <dir> <repository> <gzip|zstd> <builder> <workload> <build>: flatten
# the image and check its blobs, each step timed into phases.tsv
check() {
  local t times=()
  t=$(now)
  "${PY[@]}" guest/e2e/flatten.py 127.0.0.1:5000 "$2" latest "$1/listing.tsv.gz" "$1/config.json" | tee "$1/flatten.txt"
  times+=("flatten $(since "$t")")
  # What the builders mount or keep (context, results, state) never reaches
  # a ranikust layer; the runtime's and the harness's mounted files are
  # judged against kaniko's image of the workload when it was built first
  # (round 1 starts with the first builder, kaniko by default).
  if is_rnk "$4"; then
    local ref=() kref
    kref="$(dirname "$1")/kaniko-r1/listing.tsv.gz"
    [ ! -f "$kref" ] || ref=(--reference "$kref")
    t=$(now)
    "${PY[@]}" guest/e2e/leaks.py "${ref[@]}" "$1/listing.tsv.gz" /workspace /rnk-out /bench-out "$W" >"$1/leaks.txt" ||
      { cat "$1/leaks.txt" >&2; echo "error: $2 holds files that must not be in a layer" >&2; return 1; }
    times+=("leaks $(since "$t")")
  fi
  curl -fsI -H 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    "http://127.0.0.1:5000/v2/$2/manifests/latest" | tr -d '\r' | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: //p' >"$1/digest"
  # nextjs-kaniko's digest-check: every layer blob describes itself (gzip only).
  if [ "$3" = gzip ] && [ -f "$NK/bench/digest-check/check-blobs.py" ]; then
    t=$(now)
    "${PY[@]}" "$NK/bench/digest-check/check-blobs.py" registry 127.0.0.1:5000 "$2" latest >"$1/check-blobs.txt" 2>&1 ||
      { cat "$1/check-blobs.txt" >&2; echo "error: blob check failed for $2" >&2; return 1; }
    times+=("blobs $(since "$t")")
  fi
  local x
  for x in "${times[@]}"; do phase "$5" "$6" ${x}; done
  log "  analysis: $(printf '%s s, ' "${times[@]}" | sed 's/, $//')"
}

# --- The runs -----------------------------------------------------------------

if [ "$WARMUP" = 1 ]; then
  for w in "${WORKLOADS[@]}"; do
    wipe; build "$w" kaniko - "$OUT/warmup/$w" "e2e/warmup-$w" || true
  done
fi

failed=0
for r in $(seq 1 "$ROUNDS"); do
  for w in "${WORKLOADS[@]}"; do
    # The builders' order rotates every round, as in run-fair.sh.
    n=${#BUILDERS[@]}
    for k in $(seq 0 $((n - 1))); do
      b=${BUILDERS[$(((k + r - 1) % n))]}
      variants=(-)
      has_jobs "$b" && takes_jobs "$w" && variants=("${JOBS[@]}")
      for j in "${variants[@]}"; do
        tag="$b"; [ "$j" = - ] || tag="$b-j$j"
        d="$OUT/$w/$tag-r$r"
        t=$(now)
        wipe
        phase "$w" "$tag-r$r" wipe "$(since "$t")"
        if ! build "$w" "$b" "$j" "$d" "e2e/$w-$tag"; then failed=1; continue; fi
        status=$("${PY[@]}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["status"])' "$d/run.json")
        phase "$w" "$tag-r$r" build "$("${PY[@]}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["wall_secs"])' "$d/run.json")"
        # Content checks on the first round only, unless CHECK_ALL: later
        # rounds are timed only.
        if { [ "$r" = 1 ] || [ "$CHECK_ALL" = 1 ]; } && [ "$status" = ok ]; then
          comp=gzip; [ "$b" = buildkit ] && comp=zstd
          check "$d" "e2e/$w-$tag" "$comp" "$b" "$w" "$tag-r$r" || failed=1
        fi
      done
    done
  done
done

# --- Comparisons of round 1 -----------------------------------------------------

# A builder's round-1 directory: on W0 and W3 the ones taking --jobs have one
# per value, and the default is compared.
first_dir() {
  if has_jobs "$2" && takes_jobs "$1"; then echo "$OUT/$1/$2-jdefault-r1"; else echo "$OUT/$1/$2-r1"; fi
}

T_COMPARE=$(now)
for w in "${WORKLOADS[@]}"; do
  wd="$OUT/$w"
  [ -d "$wd" ] || continue
  rnk="$(first_dir "$w" ranikust)"
  pairs=("kaniko $wd/kaniko-r1 buildkit $wd/buildkit-r1" "ranikust $rnk kaniko $wd/kaniko-r1" "ranikust $rnk buildkit $wd/buildkit-r1")
  # kaniko mode against kaniko, and against overlay mode (ADR-0012).
  for k in rk-kaniko rk-kaniko-host; do
    pairs+=("$k $(first_dir "$w" "$k") kaniko $wd/kaniko-r1" "$k $(first_dir "$w" "$k") ranikust $rnk")
  done
  # W3's `du`-derived files differ between any two builds (Q-187).
  noise=()
  [ "$w" != w3 ] || noise=(--noise /out/a.txt --noise /out/b.txt --noise /out/c.txt --noise /out/all.txt)
  for p in "${pairs[@]}"; do
    read -r na da nb db <<<"$p"
    [ -f "$da/listing.tsv.gz" ] && [ -f "$db/listing.tsv.gz" ] || continue
    "${PY[@]}" guest/e2e/compare.py "${noise[@]}" "$na" "$da" "$nb" "$db" >"$wd/compare-$na-$nb.txt"
    log "$w $na vs $nb: $(tail -1 "$wd/compare-$na-$nb.txt")"
  done
  for b in ranikust rk-kaniko rk-kaniko-host; do
    rd="$(first_dir "$w" "$b")"
    if [ -f "$wd/kaniko-r1/metrics.json" ] && [ -f "$rd/metrics.json" ] && [ -f "$rd/digest" ]; then
      "${PY[@]}" guest/e2e/metrics.py "$wd/kaniko-r1/metrics.json" "$rd/metrics.json" >"$wd/metrics-kaniko-$b.md"
    fi
  done
  # W3 with --reproducible across --jobs (Q-187): the raw digests are
  # reported, and the images are compared by flattened content against the
  # first --jobs value. W3's stages write `du -sk node_modules` into
  # /out/{a,b,c}.txt, and /out/all.txt concatenates them: those depend on
  # disk block allocation, not on ranikust, so they are noise here. Any
  # other difference fails the suite.
  # Each builder that takes --jobs is checked on its own: overlay mode and
  # kaniko mode's per-stage directories (ADR-0059).
  if takes_jobs "$w"; then
    for b in ranikust rk-kaniko-host; do
      dg="$wd/digests-$b.txt"
      : >"$dg"
      for j in "${JOBS[@]}"; do
        [ -f "$wd/$b-j$j-r1/digest" ] && echo "jobs=$j $(cat "$wd/$b-j$j-r1/digest")" >>"$dg"
      done
      [ -s "$dg" ] || { rm -f "$dg"; continue; }
      if [ "$(cut -d' ' -f2 "$dg" | sort -u | wc -l)" = 1 ]; then
        echo "raw digests: the same at every --jobs value" >>"$dg"
      else
        echo "raw digests: differ across --jobs (compared by content below)" >>"$dg"
      fi
      du_noise=()
      [ "$w" != w3 ] || du_noise=(--noise /out/a.txt --noise /out/b.txt --noise /out/c.txt --noise /out/all.txt)
      first="${JOBS[0]}"
      for j in "${JOBS[@]:1}"; do
        da="$wd/$b-j$first-r1" db="$wd/$b-j$j-r1"
        [ -f "$da/listing.tsv.gz" ] && [ -f "$db/listing.tsv.gz" ] || continue
        "${PY[@]}" guest/e2e/compare.py "${du_noise[@]}" "jobs=$first" "$da" "jobs=$j" "$db" \
          >"$wd/compare-$b-jobs$first-jobs$j.txt"
        result="$(tail -1 "$wd/compare-$b-jobs$first-jobs$j.txt")"
        echo "content jobs=$first vs jobs=$j: $result" >>"$dg"
        if [[ "$result" != *"other=0 hardlink=0 "*"config=0" ]]; then
          echo "error: $w's content from $b depends on --jobs ($j)" | tee -a "$dg" >&2
          failed=1
        fi
      done
    done
  fi
done
phase - - compare "$(since "$T_COMPARE")"
# The suite's own wall time, from loading the images to the report.
echo "$(since "$T_SUITE")" >"$OUT/suite-seconds.txt"

"${PY[@]}" guest/e2e/report.py "$OUT" >"$OUT/report.md"
cat "$OUT/report.md"

# --- Leftovers ------------------------------------------------------------------

sudo docker rm -f "${SERVICES[@]}" >/dev/null
for s in rnk-work rnk-kwork; do
  ! grep -q "$W/$s" /proc/self/mountinfo || { echo "error: mounts left under $W/$s" >&2; failed=1; }
done
! pgrep -x ranikust >/dev/null || { echo "error: ranikust processes left" >&2; failed=1; }
[ -z "$(sudo docker ps -aq --filter name=rnk-e2e-)" ] || { echo "error: builder containers left" >&2; failed=1; }
echo "guest disk at the end: $(df -Ph / | awk 'NR == 2 { print $3 " used, " $4 " free" }')"
[ "$failed" = 0 ] && echo "e2e: ok"
exit "$failed"
