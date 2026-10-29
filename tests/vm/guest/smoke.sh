#!/usr/bin/env bash
# Runs in the T2 VM: the smoke-build example (pull, extract, sandboxed RUN,
# one-layer OCI layout) as real root, in each environment below, with the
# checks of `just smoke` plus those only real root can make: writes to the
# host's /proc/sys (T17), device nodes (T18) and the chroot escapes of
# Q-077, with a plain chroot as their control, and the refusal when
# ranikust itself runs in a plain chroot. Docker then runs each image, and
# the VM is checked for leftovers. The `ranikust` binary's set-up (namespace,
# probes, outcome, spawner) runs in each environment too.
set -euo pipefail
cd "$HOME/rnk"
source guest/smoke-lib.bash

IMAGE=docker.io/library/python:3.13-alpine
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

sudo rm -rf "$W" && sudo mkdir -p "$W"
sudo touch "$MARKER" "$W/marker"
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
  # The real binary's set-up in the same environment.
  case "$entry" in PivotRoot) syscall=pivot_root ;; MoveMount) syscall=MS_MOVE ;; esac
  rc=0
  if [ "$name" = host ]; then
    log="$(sudo "$RNK" --no-push --workdir "$dir/rnk" 2>&1)" || rc=$?
  else
    # shellcheck disable=SC2046
    log="$(sudo docker run --rm $(docker_args "$name") -v "$RNK:/ranikust:ro" "$IMAGE" \
      /ranikust --no-push --workdir /tmp/w 2>&1)" || rc=$?
  fi
  echo "$log"
  smoke_check_setup "$log" "$syscall" "$rc"
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
sudo rm -rf "$W"
echo "smoke: ok"
