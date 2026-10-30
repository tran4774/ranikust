#!/usr/bin/env bash
# Runs in the T2 VM: the smoke-build example (pull, extract, sandboxed RUN,
# one-layer OCI layout) as real root, in each environment below, with the
# checks of `just smoke` plus those only real root can make: writes to the
# host's /proc/sys (T17), device nodes (T18) and the chroot escapes of
# Q-077, with a plain chroot as their control, and the refusal when
# ranikust itself runs in a plain chroot. Docker then runs each image, and
# the VM is checked for leftovers. The `ranikust` binary builds
# tests/smoke/e2e in each environment too; Docker runs that image, and on
# the VM itself `--reproducible` must give one digest with `--jobs 1` and the
# default.
set -euo pipefail
cd "$HOME/rnk"
source guest/smoke-lib.bash

# Docker Hub images come through RANIKUST_VM_IMAGE_REGISTRY (tests/vm/vm.sh),
# by default Google's mirror, which has no pull rate limit.
REG="${RANIKUST_VM_IMAGE_REGISTRY:-mirror.gcr.io}"
IMAGE="$REG/library/python:3.13-alpine"
E2E_BASE="$REG/library/alpine:3.20"
BIN="$HOME/rnk/bin/smoke-build"
RNK="$HOME/rnk/bin/ranikust"
W=/var/tmp/rnk-smoke
MARKER=/rnk-escape-marker

# Escapes from a RUN's root (Q-077), one mode per argument. The marker
# exists only outside the stage root, so seeing it means the RUN left it.
#   dotdot  chroot into a subdirectory while the working directory stays
#           above it, climb with "..", chroot there (the classic escape)
#   fd      the same from a descriptor of / opened before the chroot
#   nested  two chroots deep, then climb
# No single quote inside: it goes into `python3 -c '...'`.
ESCAPE='import os, sys
mode = sys.argv[1]
os.makedirs("/tmp/esc/inner", exist_ok=True)
if mode == "fd":
    fd = os.open("/", os.O_PATH)
    os.chroot("/tmp/esc")
    os.fchdir(fd)
elif mode == "nested":
    os.chroot("/tmp/esc")
    os.chdir("/")
    os.chroot("/inner")
else:
    os.chroot("/tmp/esc")
for _ in range(64):
    os.chdir("..")
os.chroot(".")
sys.exit("escaped the stage root (" + mode + ")" if os.path.exists("'"$MARKER"'") else 0)'
ESCAPE_MODES=(dotdot fd nested)

# Environment name, expected root entry, how to run smoke-build there.
VARIANTS=(
  "host PivotRoot"                # real root on the VM, no container
  "privileged PivotRoot"          # docker run --privileged
  "sysadmin MoveMount"            # CAP_SYS_ADMIN, Docker's seccomp: no pivot_root
)
docker_args() {
  case "$1" in
    privileged) echo --privileged ;;
    sysadmin) echo --cap-add SYS_ADMIN --security-opt apparmor=unconfined ;;
  esac
}

runs=(--run 'echo "hello from ranikust" > /hello' --run 'apk add --no-cache jq')
for c in "${smoke_checks[@]}"; do runs+=(--run "$c"); done
runs+=(
  --run "! test -e $MARKER"
  --run "! test -e /proc/1/root$MARKER && ! test -e /proc/self/root/..$MARKER" # Q-077
  --run '! sh -c "echo rnk-escape > /proc/sys/kernel/domainname" 2>/dev/null' # T17
  # T18: a node the RUN makes does not open (nodev), even /dev/null's 1:3.
  --run 'if mknod /tmp/n c 1 3 2>/dev/null; then ! cat /tmp/n 2>/dev/null; rc=$?; rm -f /tmp/n; exit $rc; fi'
)

for m in "${ESCAPE_MODES[@]}"; do runs+=(--run "python3 -c '$ESCAPE' $m"); done # Q-077

# Docker's own pull, retried: Docker Hub's CDN resets connections now and
# then, and `docker run` gives up on the first.
for i in 1 2 3 4 5; do
  sudo docker pull -q "$IMAGE" && break
  [ "$i" = 5 ] && { echo "error: docker pull $IMAGE failed 5 times" >&2; exit 1; }
  sleep $((i * 10))
done

sudo rm -rf "$W" && sudo mkdir -p "$W"
sudo touch "$MARKER" "$W/marker"
printf %s "$SMOKE_E2E_SECRET" | sudo tee "$W/token" >/dev/null
CTX="$HOME/rnk/guest/smoke-e2e"

# Builds tests/smoke/e2e with the ranikust binary in variant $1, whose RUN
# enters its root with $2, extra flags after; prints the digest. The
# workdir of a container is a volume: overlay refuses an upper directory on
# the container's own overlay root (that environment needs kaniko mode).
e2e() {
  local name="$1" syscall="$2" d="$W/$1/e2e" rc=0 log
  shift 2
  sudo mkdir -m 700 -p "$d/w" "$d/out"
  if [ "$name" = host ]; then
    smoke_e2e_args "$CTX" "$d/w" "$d/out" "$W/token"
    e2e_args+=(--build-arg "BASE=$E2E_BASE")
    log="$(sudo "$RNK" "${e2e_args[@]}" "$@" 2>&1)" || rc=$?
  else
    smoke_e2e_args /ctx /tmp/w /out /token
    e2e_args+=(--build-arg "BASE=$E2E_BASE")
    # No CAP_NET_ADMIN in `sysadmin`: `RUN --network=none` gets loopback
    # through a network namespace in a user namespace, an opt-in (ADR-0033).
    local allow=()
    [ "$name" = sysadmin ] && allow=(--allow network.none-userns)
    # shellcheck disable=SC2046
    log="$(sudo docker run --rm $(docker_args "$name") -v "$RNK:/ranikust:ro" \
      -v "$CTX:/ctx:ro" -v "$d/w:/tmp/w" -v "$d/out:/out" -v "$W/token:/token:ro" \
      "$IMAGE" /ranikust "${e2e_args[@]}" "${allow[@]}" "$@" 2>&1)" || rc=$?
  fi
  echo "$log" | sudo tee -a "$W/$name/e2e.log" >&2
  smoke_check_build "$log" "$syscall" "$rc" >&2 || return 1
  smoke_digest "$log"
}
# `sysadmin` lacks CAP_NET_ADMIN (ADR-0033). Without `--allow
# network.none-userns`, a `RUN --network=none` is refused, naming the flag.
# With it, the RUN's root cannot join the user namespace that owns its
# network namespace or create one, and is refused x_tables, PF_KEY, TCP
# repair and nf_tables or xfrm netlink (tests/smoke/netns-check.py), while
# loopback, rtnetlink and audit netlink work (shadow's useradd).
netns_checks() {
  local d="$W/sysadmin/netns" rc=0 log
  # x_tables and PF_KEY loaded, so that their checks reach the kernel's
  # code rather than a missing module.
  sudo modprobe -a ip_tables ip6_tables af_key || true
  sudo rm -rf "$d" && sudo mkdir -m 700 -p "$d/ctx" "$d/w" "$d/out"
  printf 'FROM %s\nRUN --network=none true\n' "$IMAGE" | sudo tee "$d/ctx/Dockerfile" >/dev/null
  # shellcheck disable=SC2046
  log="$(sudo docker run --rm $(docker_args sysadmin) -v "$RNK:/ranikust:ro" \
    -v "$d/ctx:/ctx:ro" -v "$d/w:/tmp/w" -v "$d/out:/out" \
    "$IMAGE" /ranikust --context /ctx --no-push --workdir /tmp/w 2>&1)" || rc=$?
  echo "$log" | sudo tee "$d/refused.log" >/dev/null
  if [ "$rc" = 0 ] || ! grep -q -- '--allow network.none-userns' <<<"$log"; then
    echo "error: sysadmin: RUN --network=none without the opt-in: expected a refusal naming it, got $rc:" >&2
    echo "$log" >&2
    exit 1
  fi
  echo "sysadmin: RUN --network=none refused without --allow network.none-userns, as expected"
  sudo cp "$HOME/rnk/guest/netns-check.py" "$d/ctx/check.py"
  printf 'FROM %s\nRUN apk add --no-cache shadow\nCOPY check.py /check.py\nRUN --network=none python3 /check.py\nRUN --network=none useradd -m tester && id tester\n' "$IMAGE" |
    sudo tee "$d/ctx/Dockerfile" >/dev/null
  rc=0
  # shellcheck disable=SC2046
  log="$(sudo docker run --rm $(docker_args sysadmin) -v "$RNK:/ranikust:ro" \
    -v "$d/ctx:/ctx:ro" -v "$d/w:/tmp/w" -v "$d/out:/out" \
    "$IMAGE" /ranikust --context /ctx --no-push --workdir /tmp/w \
    --allow network.none-userns 2>&1)" || rc=$?
  echo "$log" | sudo tee "$d/allowed.log" >/dev/null
  if [ "$rc" != 0 ] || ! grep -q '^netns checks: ok' <<<"$log"; then
    echo "error: sysadmin: the user-namespace RUN checks failed ($rc):" >&2
    echo "$log" >&2
    exit 1
  fi
  grep -E '^NS_GET_USERNS ' <<<"$log" || true
  echo "sysadmin: with --allow network.none-userns, loopback is up, the filter refuses what it closes, and useradd works"
}

trap 'sudo rm -f "$MARKER"' EXIT
domainname_before="$(cat /proc/sys/kernel/domainname)"

for v in "${VARIANTS[@]}"; do
  read -r name entry <<<"$v"
  echo "=== $name (expect $entry)"
  dir="$W/$name"
  sudo mkdir -p "$dir/out" "$dir/work"
  tag="localhost/rnk-smoke-$name:latest"
  if [ "$name" = host ]; then
    sudo "$BIN" --image "$IMAGE" --work "$dir/work" --out "$dir/out" --tag "$tag" "${runs[@]}" |
      sudo tee "$dir/log"
  else
    # shellcheck disable=SC2046
    sudo docker run --rm $(docker_args "$name") \
      -v "$BIN:/smoke-build:ro" -v "$dir/out:/out" -v "$W/marker:$MARKER:ro" \
      "$IMAGE" /smoke-build --image "$IMAGE" --out /out --tag "$tag" "${runs[@]}" |
      sudo tee "$dir/log"
  fi
  grep -q "^probe: Caps { root_entry: $entry }" "$dir/log" ||
    { echo "error: $name: expected root entry $entry" >&2; exit 1; }
  sudo bash -c "$(declare -f smoke_check_no_stub_leak); smoke_check_no_stub_leak '$dir/out'"
  sudo tar -C "$dir/out" -cf "$dir/oci.tar" .
  loaded="$(sudo docker load -i "$dir/oci.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
  got="$(sudo docker run --rm "$loaded" sh -c 'cat /hello && jq --version')"
  [[ "$got" == "hello from ranikust"$'\n'jq-* ]] ||
    { echo "error: $name: the image does not run as built: $got" >&2; exit 1; }
  sudo docker rmi -f "$loaded" >/dev/null
  # The real binary builds tests/smoke/e2e in the same environment.
  case "$entry" in PivotRoot) syscall=pivot_root ;; MoveMount) syscall=MS_MOVE ;; esac
  e2e "$name" "$syscall" >/dev/null
  [ "$name" = sysadmin ] && netns_checks
  sudo bash -c "$(declare -f smoke_check_no_stub_leak smoke_check_no_secret smoke_check_empty_layer); \
    smoke_check_no_stub_leak '$dir/e2e/out/oci' && \
    smoke_check_no_secret '$dir/e2e/out/oci' '$SMOKE_E2E_SECRET' && \
    smoke_check_empty_layer '$dir/e2e/out/oci'"
  sudo docker load -q -i "$dir/e2e/out/image.tar" >/dev/null
  got="$(sudo docker run --rm localhost/rnk-e2e:latest)"
  [ "$got" = "$SMOKE_E2E_OUTPUT" ] ||
    { echo "error: $name: the e2e image printed: $got" >&2; exit 1; }
  [ "$(sudo docker run --rm localhost/rnk-e2e:latest id -un)" = "$SMOKE_E2E_USER" ] ||
    { echo "error: $name: the e2e image does not run as $SMOKE_E2E_USER" >&2; exit 1; }
  sudo docker rmi -f localhost/rnk-e2e:latest >/dev/null
  if [ "$name" = host ]; then
    # Phase 1 acceptance, --reproducible: one digest whatever --jobs, for
    # builds at different times, and with metrics on and off.
    a="$(e2e "$name" "$syscall" --reproducible)"
    b="$(e2e "$name" "$syscall" --reproducible --jobs 1)"
    sleep 2
    m="$W/host/e2e/metrics.json"
    c="$(e2e "$name" "$syscall" --reproducible --metrics-file "$m")"
    [ "$a" = "$b" ] && [ "$a" = "$c" ] ||
      { echo "error: --reproducible digests differ: $a (default jobs), $b (--jobs 1), $c (later, with metrics)" >&2; exit 1; }
    sudo test -s "$m" || { echo "error: no metrics report at $m" >&2; exit 1; }
    echo "host: --reproducible gives $a with --jobs 1 and the default, 2 s later, and with metrics on"
    # Phase 1 acceptance: the secret reaches no build log and no metrics
    # report (the blobs are checked above, the RUN environments in the
    # Dockerfile).
    sudo bash -c "$(declare -f smoke_check_no_secret_in); \
      smoke_check_no_secret_in '$SMOKE_E2E_SECRET' '$W/host/e2e.log' '$m'"
    echo "host: the secret is in no log and no metrics report"
    # Phase 1 acceptance: a cache mount persists across builds with the
    # same state directory, and only through it.
    p="$W/host/persist"
    nonce="$(od -An -N8 -tx8 /dev/urandom | tr -d ' ')"
    sudo mkdir -m 700 -p "$p/w" "$p/w2" "$p/write" "$p/read"
    printf 'FROM %s\nARG NONCE\nRUN --mount=type=cache,id=persist,target=/c echo "$NONCE" > /c/n\n' "$IMAGE" |
      sudo tee "$p/write/Dockerfile" >/dev/null
    printf 'FROM %s\nARG NONCE\nRUN --mount=type=cache,id=persist,target=/c test "$(cat /c/n 2>/dev/null)" = "$NONCE"\n' "$IMAGE" |
      sudo tee "$p/read/Dockerfile" >/dev/null
    plog="$(mktemp)"
    persist() {
      sudo "$RNK" --context "$p/$1" --no-push --workdir "$p/$2" --build-arg "NONCE=$nonce" \
        >>"$plog" 2>&1
    }
    persist write w || { echo "error: the cache-writing build failed" >&2; cat "$plog" >&2; exit 1; }
    persist read w || { echo "error: the cache did not persist into the next build" >&2; cat "$plog" >&2; exit 1; }
    ! persist read w2 || { echo "error: a fresh state directory saw the cache" >&2; exit 1; }
    echo "host: a cache mount persists across builds sharing a state directory, and only there"
    # A cancel kills the running RUN, as BuildKit (ADR-0032): SIGINT while
    # a RUN sleeps ends the build at once, and no RUN process is left. The
    # RUN mounts the secret, which lives only on the RUN's own tmpfs: it is
    # in no file of the host, during the RUN or after, even when ranikust
    # is killed with SIGKILL.
    c="$W/host/cancel"
    sudo mkdir -m 700 -p "$c/ctx"
    printf 'FROM %s\nRUN --mount=type=secret,id=token sleep 300\n' "$IMAGE" |
      sudo tee "$c/ctx/Dockerfile" >/dev/null
    no_secret_on_disk() {
      ! sudo grep -rqa -- "$SMOKE_E2E_SECRET" "$c/w" ||
        { echo "error: the secret is in a file under $c/w ($1)" >&2; exit 1; }
    }
    for sig in INT KILL; do
      sudo rm -rf "$c/w" && sudo mkdir -m 700 "$c/w"
      clog="$(mktemp)"
      sudo "$RNK" --context "$c/ctx" --no-push --workdir "$c/w" \
        --secret "id=token,src=$W/token" >"$clog" 2>&1 &
      for _ in $(seq 120); do pgrep -f '^sleep 300$' >/dev/null && break; sleep 1; done
      pgrep -f '^sleep 300$' >/dev/null ||
        { echo "error: the $sig test's RUN never started" >&2; cat "$clog" >&2; exit 1; }
      no_secret_on_disk "during the RUN"
      start=$SECONDS
      sudo pkill "-$sig" -x ranikust
      rc=0; wait $! || rc=$?
      [ "$rc" != 0 ] && [ $((SECONDS - start)) -lt 30 ] ||
        { echo "error: SIG$sig: the build took $((SECONDS - start)) s, exit $rc" >&2; cat "$clog" >&2; exit 1; }
      for _ in $(seq 10); do pgrep -f '^sleep 300$' >/dev/null || break; sleep 1; done
      ! pgrep -f '^sleep 300$' >/dev/null || { echo "error: SIG$sig: the RUN is still running" >&2; exit 1; }
      no_secret_on_disk "after SIG$sig"
      echo "host: SIG$sig kills the running RUN ($((SECONDS - start)) s); the secret never reached a disk"
    done
  fi
  echo "$name: ok"
done

echo "=== escape control: a plain chroot into the host variant's root"
# Each escape must work there, or its check in the sandbox proves nothing.
for m in "${ESCAPE_MODES[@]}"; do
  if sudo chroot "$W/host/work/rootfs" /usr/local/bin/python3 -c "$ESCAPE" "$m" 2>/dev/null; then
    echo "error: the $m escape does not work in a plain chroot" >&2
    exit 1
  fi
  echo "control $m: escaped, as expected"
done

# ranikust itself in a plain chroot, where / is not a mount root: it must
# refuse before any RUN (Q-077: MS_MOVE would leave ".." climbing out), and
# say why. /proc comes in for the thread count and the probe. Both the
# smoke-build example and the binary.
for name in privileged sysadmin; do
  for prog in smoke-build ranikust; do
    echo "=== $name, $prog inside a plain chroot (expect a refusal)"
    case "$prog" in
      smoke-build) args="--image $IMAGE --work /tmp/work --out /tmp/out --run true" ;;
      ranikust) args="--no-push --workdir /tmp/w" ;;
    esac
    # shellcheck disable=SC2046
    set +e
    out="$(sudo docker run --rm $(docker_args "$name") -v "$HOME/rnk/bin/$prog:/$prog:ro" "$IMAGE" sh -c '
      mkdir -p /outer/proc /outer/tmp && cp /'"$prog"' /outer/ &&
      { mount -t proc proc /outer/proc || mount --rbind /proc /outer/proc; } &&
      exec chroot /outer /'"$prog $args" 2>&1)"
    rc=$?
    set -e
    echo "$out"
    if [ "$rc" = 0 ] || grep -q '^probe: Caps' <<<"$out" || grep -q '^RUN ' <<<"$out" ||
      grep -q 'set up for' <<<"$out"; then
      echo "error: $name: $prog did not refuse inside a plain chroot" >&2
      exit 1
    fi
    # Refused for / not being a mount, before the build namespace
    # changes anything (`rnk_sandbox::enter_build_namespace`).
    grep -q '^'"$prog"': .*/ is not a mount point: ranikust runs inside a plain chroot' <<<"$out" ||
      { echo "error: $name: $prog refused for another reason" >&2; exit 1; }
    echo "$name, $prog in a chroot: refused, as expected"
  done
done

echo "=== the VM after the runs"
[ "$(cat /proc/sys/kernel/domainname)" = "$domainname_before" ] ||
  { echo "error: the VM's domainname changed" >&2; exit 1; }
! grep -q "$W" /proc/self/mountinfo || { echo "error: mounts left under $W" >&2; exit 1; }
! pgrep -f smoke-build >/dev/null || { echo "error: smoke-build processes left" >&2; exit 1; }
! pgrep -x ranikust >/dev/null || { echo "error: ranikust processes left" >&2; exit 1; }
sudo rm -rf "$W"
echo "smoke: ok"
