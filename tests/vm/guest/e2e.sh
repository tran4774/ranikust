#!/usr/bin/env bash
# Runs in the T2 VM: the Phase 1 workloads of nextjs-kaniko (W1 app, W2 cold
# app-cache, W3 app-par) built by kaniko and BuildKit as references and by
# ranikust, as nextjs-kaniko/bench/run-fair.sh builds them, with a local
# registry and the verdaccio npm proxy inside the VM. Each build is timed as
# run-fair.sh times it (start to exit of the build); every image of the first
# round is flattened and compared (path, type, mode, owner, size, sha256,
# link, xattrs, config); kaniko's and ranikust's metrics reports are put side
# by side. Results go to ~/rnk/out/e2e/ (target/vm/results/<release>/e2e/).
#
# A ranikust build that stops because something is not built yet ("not
# implemented", a flag refused until a later phase) is recorded as pending and
# does not fail the suite; any other failure does.
#
# Seed (copied in by tests/vm/vm.sh from tests/vm/e2e-seed.py): ~/rnk/e2e/
# with images/ and images.txt, registry/ (the base and tools images with
# their original digests), verdaccio/ (the npm proxy's storage) and nk/ (the
# workloads and the files of nextjs-kaniko/bench it needs).
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
#   WORKLOADS  default "w1 w2 w3"
#   BUILDERS   default "kaniko buildkit ranikust rk-kaniko rk-kaniko-host"
#   ROUNDS     default 1                  WARMUP    default 1 (one discarded kaniko build per workload)
#   CPUS       default 4                  MEMORY    default 8g (as run-fair.sh)
#   JOBS       default "1 2 default"      --jobs values on W3 (with --reproducible) of
#                                         `ranikust` and `rk-kaniko-host`
#   QUICK      1: defaults for a check that the suite works (W1 and W3, no
#              warm-up, one round, kaniko and every ranikust builder, --jobs
#              1 and the default); the variables above still win
#   MIN_FREE_GB  default 10: the guest's free disk below which no build
#              starts (after pruning the suite fails instead of filling it)
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
[ -f e2e.env ] && source e2e.env
if [ "${RANIKUST_E2E_QUICK:-0}" = 1 ]; then
  : "${RANIKUST_E2E_WORKLOADS:=w1 w3}" "${RANIKUST_E2E_BUILDERS:=kaniko ranikust rk-kaniko rk-kaniko-host}"
  : "${RANIKUST_E2E_WARMUP:=0}" "${RANIKUST_E2E_ROUNDS:=1}" "${RANIKUST_E2E_JOBS:=1 default}"
fi

E="$HOME/rnk/e2e"
NK="$E/nk"
OUT="$HOME/rnk/out/e2e"
W=/var/lib/rnk-e2e
BIN="$HOME/rnk/bin/ranikust"
PY=(python3 -I)
WORKLOADS=(${RANIKUST_E2E_WORKLOADS:-w1 w2 w3})
BUILDERS=(${RANIKUST_E2E_BUILDERS:-kaniko buildkit ranikust rk-kaniko rk-kaniko-host})
ROUNDS=${RANIKUST_E2E_ROUNDS:-1}
WARMUP=${RANIKUST_E2E_WARMUP:-1}
CPUS=${RANIKUST_E2E_CPUS:-4}
MEMORY=${RANIKUST_E2E_MEMORY:-8g}
JOBS=(${RANIKUST_E2E_JOBS:-1 2 default})
MIN_FREE_GB=${RANIKUST_E2E_MIN_FREE_GB:-10}
# The options reach sudo command lines and arithmetic: whole numbers only
# (MEMORY a number with an optional k, m or g; JOBS also `default`).
bad_opt() { echo "error: RANIKUST_E2E_$1 must be $2 (got '$3')" >&2; exit 2; }
for v in ROUNDS WARMUP CPUS MIN_FREE_GB; do
  [[ "${!v}" =~ ^[0-9]+$ ]] || bad_opt "$v" "a whole number" "${!v}"
done
[ "$ROUNDS" -ge 1 ] && [ "$CPUS" -ge 1 ] || bad_opt "ROUNDS/CPUS" "at least 1" "$ROUNDS/$CPUS"
[[ "$WARMUP" =~ ^[01]$ ]] || bad_opt WARMUP "0 or 1" "$WARMUP"
[[ "$MEMORY" =~ ^[0-9]+[kKmMgG]?$ ]] || bad_opt MEMORY "a number with k, m or g" "$MEMORY"
for j in "${JOBS[@]}"; do
  [[ "$j" =~ ^([1-9][0-9]*|default)$ ]] || bad_opt JOBS "positive whole numbers or default" "$j"
done
for w in "${WORKLOADS[@]}"; do
  case "$w" in w1 | w2 | w3) ;; *) bad_opt WORKLOADS "w1, w2 or w3" "$w" ;; esac
done
for b in "${BUILDERS[@]}"; do
  case "$b" in kaniko | buildkit | ranikust | rk-kaniko | rk-kaniko-host) ;; *) bad_opt BUILDERS "known builders" "$b" ;; esac
done

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
BASE="$(cat "$NK/bench/base-image.txt")"
KANIKO_IMAGE=localhost/kaniko-bench:stream
BK_IMAGE=rnk-e2e/buildkit:v0.33.1-rootless
# An empty image for rk-kaniko: ranikust is static and builds over its `/`.
EMPTY_IMAGE=rnk-e2e/empty:latest
REPOS="$W/registry/docker/registry/v2/repositories"

declare -A CTX=([w1]=app [w2]=app-cache [w3]=app-par)

log() { printf '[e2e] %s\n' "$*"; }
now() { date +%s.%N; }
mkdir -p "$OUT"

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
sudo mv "$E/registry" "$W/registry"
sudo mv "$E/verdaccio" "$W/verdaccio"
sudo chown -R 10001:65533 "$W/verdaccio" # the verdaccio image's user
sudo docker network create --internal nk-internal >/dev/null
sudo docker network create nk-egress >/dev/null
sudo docker run -d --name nk-registry --network nk-egress -p 127.0.0.1:5000:5000 \
  -e REGISTRY_STORAGE_DELETE_ENABLED=true -v "$W/registry:/var/lib/registry" registry:2 >/dev/null
sudo docker run -d --name nk-verdaccio --network nk-egress -p 127.0.0.1:4873:4873 \
  -v "$NK/verdaccio/config.yaml:/verdaccio/conf/config.yaml:ro" -v "$W/verdaccio:/verdaccio/storage" \
  verdaccio/verdaccio:6.1.6 >/dev/null
sudo docker network connect --alias registry nk-internal nk-registry
sudo docker network connect --alias verdaccio nk-internal nk-verdaccio
# ranikust runs on the VM itself and its RUN shares the VM's network (ADR-0010).
echo "127.0.0.1 registry verdaccio" | sudo tee -a /etc/hosts >/dev/null
wait_up() { for _ in $(seq 120); do curl -fs -o /dev/null "$1" && return 0; sleep 0.5; done; echo "error: $1 is not up" >&2; exit 1; }
wait_up http://127.0.0.1:5000/v2/
wait_up http://127.0.0.1:4873/-/ping

TOML="$OUT/buildkitd.toml"
printf '[registry."registry:5000"]\n  http = true\n  insecure = true\n[dns]\n  nameservers = ["127.0.0.11"]\n' >"$TOML"
chmod 644 "$TOML"

{
  echo "base image: $BASE"
  echo "kaniko: $KANIKO_IMAGE ($(sudo docker image inspect -f '{{.Id}}' "$KANIKO_IMAGE")), cfg-final-opt.env + FF_KANIKO_STREAM_TAR_COMPRESS=1"
  echo "buildkit: $BK_IMAGE ($(sudo docker image inspect -f '{{.Id}}' "$BK_IMAGE")), zstd"
  echo "ranikust: $("$BIN" version 2>/dev/null | head -1) (sha256 $(sha256sum "$BIN" | cut -d' ' -f1))"
  echo "kernel: $(uname -r); cpus: $(nproc); mem: $(awk '/MemTotal/ {print $2}' /proc/meminfo) kB; disk free: $(df -Ph / | awk 'NR == 2 { print $4 }')"
  echo "limits: --cpus=$CPUS --memory=$MEMORY (ranikust: a systemd scope with the same CPUQuota and MemoryMax)"
  echo "workloads: ${WORKLOADS[*]}; builders: ${BUILDERS[*]}; rounds: $ROUNDS; warm-up: $WARMUP; W3 jobs: ${JOBS[*]}"
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

is_rnk() { case "$1" in ranikust | rk-*) return 0 ;; *) return 1 ;; esac; }
# The builders that take --jobs on W3 (their stages may run side by side).
has_jobs() { case "$1" in ranikust | rk-kaniko-host) return 0 ;; *) return 1 ;; esac; }

# build <workload> <builder> <jobs|-> <dir> <repository>; writes <dir>/run.json
build() {
  local w=$1 b=$2 jobs=$3 d=$4 repo=$5 ctx="$NK/${CTX[$1]}" name="rnk-e2e-$RANDOM" vol= t0 t1 rc status reason=
  mkdir -p "$d"
  sync
  case "$b" in
    kaniko)
      # shellcheck disable=SC2046
      t0=$(now)
      sudo docker run -d --name "$name" --network nk-internal --cpus="$CPUS" --memory="$MEMORY" \
        $(cat "$NK/bench/cfg-final-opt.env") -e FF_KANIKO_STREAM_TAR_COMPRESS=1 \
        -v "$ctx:/workspace:ro" -v "$d:/bench-out" -e KANIKO_METRICS_FILE=/bench-out/metrics.json "$KANIKO_IMAGE" \
        --dockerfile=/workspace/Dockerfile --context=dir:///workspace --destination="registry:5000/$repo:latest" \
        --insecure-registry=registry:5000 --build-arg "BASE_IMAGE=$BASE" --digest-file=/bench-out/digest.txt \
        --verbosity=info --log-timestamp=true >/dev/null
      rc=$(sudo docker wait "$name"); t1=$(now)
      sudo docker logs "$name" >"$d/build.log" 2>&1 || true
      sudo docker rm -f "$name" >/dev/null
      ;;
    buildkit)
      vol="rnk-e2e-bk-$RANDOM"; sudo docker volume create "$vol" >/dev/null
      t0=$(now)
      sudo docker run -d --name "$name" --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
        --network nk-internal --cpus="$CPUS" --memory="$MEMORY" \
        -e BUILDKITD_FLAGS="--oci-worker-no-process-sandbox --config /home/user/.config/buildkit/buildkitd.toml" \
        -v "$vol:/home/user/.local/share/buildkit" -v "$ctx:/workspace:ro" \
        -v "$TOML:/home/user/.config/buildkit/buildkitd.toml:ro" --entrypoint buildctl-daemonless.sh "$BK_IMAGE" \
        build --progress plain --frontend dockerfile.v0 --local context=/workspace --local dockerfile=/workspace \
        --opt "build-arg:BASE_IMAGE=$BASE" \
        --output "type=image,\"name=registry:5000/$repo:latest\",push=true,compression=zstd" >/dev/null
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
      sudo docker run -d --name "$name" --network nk-internal --cpus="$CPUS" --memory="$MEMORY" \
        -v "$BIN:/ranikust/bin/ranikust:ro" -v "$ctx:/workspace:ro" -v "$d:/rnk-out" \
        -v /etc/ssl/certs:/etc/ssl/certs:ro "$EMPTY_IMAGE" \
        /ranikust/bin/ranikust build --mode=kaniko --context=/workspace --dockerfile=/workspace/Dockerfile \
        --destination="registry:5000/$repo:latest" --insecure-registry=registry:5000 \
        --build-arg="BASE_IMAGE=$BASE" --metrics-file=/rnk-out/metrics.json --digest-file=/rnk-out/digest.txt \
        "${extra[@]}" >/dev/null
      rc=$(sudo docker wait "$name"); t1=$(now)
      sudo docker logs "$name" >"$d/build.log" 2>&1 || true
      sudo docker rm -f "$name" >/dev/null
      sudo chown -R "$(id -u):$(id -g)" "$d"
      ;;
    rk-kaniko-host)
      local extra=()
      [ "$jobs" = - ] || [ "$jobs" = default ] || extra+=("--jobs=$jobs")
      [ "$jobs" = - ] || extra+=(--reproducible)
      reset_state rnk-kwork || exit 1
      sudo mkdir -m 700 -p "$W/rnk-kwork"
      t0=$(now)
      set +e
      sudo systemd-run --quiet --scope -p CPUQuota="${CPUS}00%" -p MemoryMax="${MEMORY^^}" -- \
        "$BIN" build --mode=kaniko --context="$ctx" --dockerfile="$ctx/Dockerfile" \
        --destination="registry:5000/$repo:latest" --insecure-registry=registry:5000 \
        --build-arg="BASE_IMAGE=$BASE" --state-dir="$W/rnk-kwork" \
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
      sudo systemd-run --quiet --scope -p CPUQuota="${CPUS}00%" -p MemoryMax="${MEMORY^^}" -- \
        "$BIN" build --context="$ctx" --dockerfile="$ctx/Dockerfile" --destination="registry:5000/$repo:latest" \
        --insecure-registry=registry:5000 --build-arg="BASE_IMAGE=$BASE" --state-dir="$W/rnk-work" \
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
import json, sys
out, w, b, rc, t0, t1, status, reason, repo = sys.argv[1:10]
json.dump({"workload": w, "builder": b, "exit_code": int(rc), "wall_secs": round(float(t1) - float(t0), 3),
           "status": status, "reason": reason, "repository": repo}, open(out, "w"), indent=2)
PY
  log "$w $label: $status (exit $rc, $(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }') s)"
  [ "$status" != failed ] || { tail -20 "$d/build.log" >&2; return 1; }
}

# check <dir> <repository> <gzip|zstd> <builder>: flatten the image and check its blobs
check() {
  "${PY[@]}" guest/e2e/flatten.py 127.0.0.1:5000 "$2" latest "$1/listing.tsv.gz" "$1/config.json" | tee "$1/flatten.txt"
  # What the builders mount or keep (context, results, state) never reaches
  # a ranikust layer; the runtime's and the harness's mounted files are
  # judged against kaniko's image of the workload when it was built first
  # (round 1 starts with the first builder, kaniko by default).
  if is_rnk "$4"; then
    local ref=() kref
    kref="$(dirname "$1")/kaniko-r1/listing.tsv.gz"
    [ ! -f "$kref" ] || ref=(--reference "$kref")
    "${PY[@]}" guest/e2e/leaks.py "${ref[@]}" "$1/listing.tsv.gz" /workspace /rnk-out /bench-out "$W" >"$1/leaks.txt" ||
      { cat "$1/leaks.txt" >&2; echo "error: $2 holds files that must not be in a layer" >&2; return 1; }
  fi
  curl -fsI -H 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    "http://127.0.0.1:5000/v2/$2/manifests/latest" | tr -d '\r' | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: //p' >"$1/digest"
  # nextjs-kaniko's digest-check: every layer blob describes itself (gzip only).
  if [ "$3" = gzip ]; then
    "${PY[@]}" "$NK/bench/digest-check/check-blobs.py" registry 127.0.0.1:5000 "$2" latest >"$1/check-blobs.txt" 2>&1 ||
      { cat "$1/check-blobs.txt" >&2; echo "error: blob check failed for $2" >&2; return 1; }
  fi
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
      has_jobs "$b" && [ "$w" = w3 ] && variants=("${JOBS[@]}")
      for j in "${variants[@]}"; do
        tag="$b"; [ "$j" = - ] || tag="$b-j$j"
        d="$OUT/$w/$tag-r$r"
        wipe
        if ! build "$w" "$b" "$j" "$d" "e2e/$w-$tag"; then failed=1; continue; fi
        status=$("${PY[@]}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["status"])' "$d/run.json")
        if [ "$r" = 1 ] && [ "$status" = ok ]; then
          comp=gzip; [ "$b" = buildkit ] && comp=zstd
          check "$d" "e2e/$w-$tag" "$comp" "$b" || failed=1
        fi
      done
    done
  done
done

# --- Comparisons of round 1 -----------------------------------------------------

# A builder's round-1 directory: on W3 the ones taking --jobs have one per
# value, and the default is compared.
first_dir() {
  if has_jobs "$2" && [ "$1" = w3 ]; then echo "$OUT/$1/$2-jdefault-r1"; else echo "$OUT/$1/$2-r1"; fi
}

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
  if [ "$w" = w3 ]; then
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
      du_noise=(--noise /out/a.txt --noise /out/b.txt --noise /out/c.txt --noise /out/all.txt)
      first="${JOBS[0]}"
      for j in "${JOBS[@]:1}"; do
        da="$wd/$b-j$first-r1" db="$wd/$b-j$j-r1"
        [ -f "$da/listing.tsv.gz" ] && [ -f "$db/listing.tsv.gz" ] || continue
        "${PY[@]}" guest/e2e/compare.py "${du_noise[@]}" "jobs=$first" "$da" "jobs=$j" "$db" \
          >"$wd/compare-$b-jobs$first-jobs$j.txt"
        result="$(tail -1 "$wd/compare-$b-jobs$first-jobs$j.txt")"
        echo "content jobs=$first vs jobs=$j: $result" >>"$dg"
        if [[ "$result" != *"other=0 hardlink=0 "*"config=0" ]]; then
          echo "error: W3's content from $b depends on --jobs ($j)" | tee -a "$dg" >&2
          failed=1
        fi
      done
    done
  fi
done

"${PY[@]}" guest/e2e/report.py "$OUT" >"$OUT/report.md"
cat "$OUT/report.md"

# --- Leftovers ------------------------------------------------------------------

sudo docker rm -f nk-registry nk-verdaccio >/dev/null
for s in rnk-work rnk-kwork; do
  ! grep -q "$W/$s" /proc/self/mountinfo || { echo "error: mounts left under $W/$s" >&2; failed=1; }
done
! pgrep -x ranikust >/dev/null || { echo "error: ranikust processes left" >&2; failed=1; }
[ -z "$(sudo docker ps -aq --filter name=rnk-e2e-)" ] || { echo "error: builder containers left" >&2; failed=1; }
echo "guest disk at the end: $(df -Ph / | awk 'NR == 2 { print $3 " used, " $4 " free" }')"
[ "$failed" = 0 ] && echo "e2e: ok"
exit "$failed"
