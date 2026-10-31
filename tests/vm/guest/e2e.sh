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
# Options (RANIKUST_E2E_* on the host, written to ~/rnk/e2e.env):
#   WORKLOADS  default "w1 w2 w3"         BUILDERS  default "kaniko buildkit ranikust"
#   ROUNDS     default 1                  WARMUP    default 1 (one discarded kaniko build per workload)
#   CPUS       default 4                  MEMORY    default 8g (as run-fair.sh)
#   JOBS       default "1 2 default"      ranikust's --jobs values on W3 (with --reproducible)
set -euo pipefail
cd "$HOME/rnk"
# shellcheck disable=SC1091
[ -f e2e.env ] && source e2e.env

E="$HOME/rnk/e2e"
NK="$E/nk"
OUT="$HOME/rnk/out/e2e"
W=/var/lib/rnk-e2e
BIN="$HOME/rnk/bin/ranikust"
PY=(python3 -I)
WORKLOADS=(${RANIKUST_E2E_WORKLOADS:-w1 w2 w3})
BUILDERS=(${RANIKUST_E2E_BUILDERS:-kaniko buildkit ranikust})
ROUNDS=${RANIKUST_E2E_ROUNDS:-1}
WARMUP=${RANIKUST_E2E_WARMUP:-1}
CPUS=${RANIKUST_E2E_CPUS:-4}
MEMORY=${RANIKUST_E2E_MEMORY:-8g}
JOBS=(${RANIKUST_E2E_JOBS:-1 2 default})
BASE="$(cat "$NK/bench/base-image.txt")"
KANIKO_IMAGE=localhost/kaniko-bench:stream
BK_IMAGE=rnk-e2e/buildkit:v0.33.1-rootless
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
  echo "ranikust: $("$BIN" version 2>/dev/null | head -1)"
  echo "limits: --cpus=$CPUS --memory=$MEMORY (ranikust: a systemd scope with the same CPUQuota and MemoryMax)"
  echo "workloads: ${WORKLOADS[*]}; builders: ${BUILDERS[*]}; rounds: $ROUNDS; warm-up: $WARMUP; W3 jobs: ${JOBS[*]}"
} | tee "$OUT/env.txt"

# --- One build ----------------------------------------------------------------

# Every build starts from an empty e2e/ namespace in a restarted registry, as
# run-fair.sh wipes its repositories, so nothing is "already there".
wipe() {
  sudo rm -rf "$REPOS/e2e"
  sudo docker restart nk-registry >/dev/null
  wait_up http://127.0.0.1:5000/v2/
}

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
        -v "$ctx:/workspace:ro" -v "$d:/out" -e KANIKO_METRICS_FILE=/out/metrics.json "$KANIKO_IMAGE" \
        --dockerfile=/workspace/Dockerfile --context=dir:///workspace --destination="registry:5000/$repo:latest" \
        --insecure-registry=registry:5000 --build-arg "BASE_IMAGE=$BASE" --digest-file=/out/digest.txt \
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
    ranikust)
      local extra=()
      [ "$jobs" = - ] || [ "$jobs" = default ] || extra+=("--jobs=$jobs")
      [ "$jobs" = - ] || extra+=(--reproducible)
      sudo rm -rf "$W/rnk-work" && sudo mkdir -p "$W/rnk-work"
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
  esac
  status=ok
  if [ "$rc" != 0 ]; then
    status=failed
    if [ "$b" = ranikust ] && grep -Eiq 'not implemented|not yet|comes in Phase|\[open: Q-|\(Q-[0-9]+\)' "$d/build.log"; then
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

# check <dir> <repository> <gzip|zstd>: flatten the image and check its blobs
check() {
  "${PY[@]}" guest/e2e/flatten.py 127.0.0.1:5000 "$2" latest "$1/listing.tsv.gz" "$1/config.json" | tee "$1/flatten.txt"
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
      [ "$b" = ranikust ] && [ "$w" = w3 ] && variants=("${JOBS[@]}")
      for j in "${variants[@]}"; do
        tag="$b"; [ "$j" = - ] || tag="$b-j$j"
        d="$OUT/$w/$tag-r$r"
        wipe
        if ! build "$w" "$b" "$j" "$d" "e2e/$w-$tag"; then failed=1; continue; fi
        status=$("${PY[@]}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["status"])' "$d/run.json")
        if [ "$r" = 1 ] && [ "$status" = ok ]; then
          comp=gzip; [ "$b" = buildkit ] && comp=zstd
          check "$d" "e2e/$w-$tag" "$comp" || failed=1
        fi
      done
    done
  done
done

# --- Comparisons of round 1 -----------------------------------------------------

for w in "${WORKLOADS[@]}"; do
  wd="$OUT/$w"
  [ -d "$wd" ] || continue
  rnk="$wd/ranikust-r1"; [ "$w" = w3 ] && rnk="$wd/ranikust-jdefault-r1"
  pairs=("kaniko $wd/kaniko-r1 buildkit $wd/buildkit-r1" "ranikust $rnk kaniko $wd/kaniko-r1" "ranikust $rnk buildkit $wd/buildkit-r1")
  for p in "${pairs[@]}"; do
    read -r na da nb db <<<"$p"
    [ -f "$da/listing.tsv.gz" ] && [ -f "$db/listing.tsv.gz" ] || continue
    "${PY[@]}" guest/e2e/compare.py "$na" "$da" "$nb" "$db" >"$wd/compare-$na-$nb.txt"
    log "$w $na vs $nb: $(tail -1 "$wd/compare-$na-$nb.txt")"
  done
  if [ -f "$wd/kaniko-r1/metrics.json" ] && [ -f "$rnk/metrics.json" ] && [ -f "$rnk/digest" ]; then
    "${PY[@]}" guest/e2e/metrics.py "$wd/kaniko-r1/metrics.json" "$rnk/metrics.json" >"$wd/metrics-kaniko-ranikust.md"
  fi
  # W3 with --reproducible across --jobs (Q-187): the raw digests are
  # reported, and the images are compared by flattened content against the
  # first --jobs value. W3's stages write `du -sk node_modules` into
  # /out/{a,b,c}.txt, and /out/all.txt concatenates them: those depend on
  # disk block allocation, not on ranikust, so they are noise here. Any
  # other difference fails the suite.
  if [ "$w" = w3 ]; then
    : >"$wd/digests.txt"
    for j in "${JOBS[@]}"; do
      [ -f "$wd/ranikust-j$j-r1/digest" ] && echo "jobs=$j $(cat "$wd/ranikust-j$j-r1/digest")" >>"$wd/digests.txt"
    done
    if [ -s "$wd/digests.txt" ]; then
      if [ "$(cut -d' ' -f2 "$wd/digests.txt" | sort -u | wc -l)" = 1 ]; then
        echo "raw digests: the same at every --jobs value" >>"$wd/digests.txt"
      else
        echo "raw digests: differ across --jobs (compared by content below)" >>"$wd/digests.txt"
      fi
      du_noise=(--noise /out/a.txt --noise /out/b.txt --noise /out/c.txt --noise /out/all.txt)
      first="${JOBS[0]}"
      for j in "${JOBS[@]:1}"; do
        da="$wd/ranikust-j$first-r1" db="$wd/ranikust-j$j-r1"
        [ -f "$da/listing.tsv.gz" ] && [ -f "$db/listing.tsv.gz" ] || continue
        "${PY[@]}" guest/e2e/compare.py "${du_noise[@]}" "jobs=$first" "$da" "jobs=$j" "$db" \
          >"$wd/compare-jobs$first-jobs$j.txt"
        result="$(tail -1 "$wd/compare-jobs$first-jobs$j.txt")"
        echo "content jobs=$first vs jobs=$j: $result" >>"$wd/digests.txt"
        if [[ "$result" != *"other=0 hardlink=0 "*"config=0" ]]; then
          echo "error: W3's content depends on --jobs ($j)" | tee -a "$wd/digests.txt" >&2
          failed=1
        fi
      done
    fi
  fi
done

"${PY[@]}" guest/e2e/report.py "$OUT" >"$OUT/report.md"
cat "$OUT/report.md"

# --- Leftovers ------------------------------------------------------------------

sudo docker rm -f nk-registry nk-verdaccio >/dev/null
! grep -q "$W/rnk-work" /proc/self/mountinfo || { echo "error: mounts left under $W/rnk-work" >&2; failed=1; }
! pgrep -x ranikust >/dev/null || { echo "error: ranikust processes left" >&2; failed=1; }
[ "$failed" = 0 ] && echo "e2e: ok"
exit "$failed"
