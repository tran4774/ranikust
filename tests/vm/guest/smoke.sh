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
# default. Each build's user-namespace probe line is printed, and a default
# `docker run` checks where that probe stops (#74). smoke-build runs as the
# VM's regular user too, in the build's user namespace (#78), and so does
# the ranikust binary, in rootless overlay mode (#79).
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
# state directory of a container is a volume: overlay refuses an upper
# directory on the container's own overlay root (ADR-0050).
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
    "$IMAGE" /ranikust --context /ctx --no-push --state-dir /tmp/w 2>&1)" || rc=$?
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
  # ADR-0033's filter stacks on Docker's default profile (ADR-0109), and
  # stays without it: the checks pass both ways, one filter apart.
  local seccomp n default_n=
  for seccomp in default unconfined; do
    local flag=()
    [ "$seccomp" = unconfined ] && flag=(--seccomp=unconfined)
    rc=0
    # shellcheck disable=SC2046
    log="$(sudo docker run --rm $(docker_args sysadmin) -v "$RNK:/ranikust:ro" \
      -v "$d/ctx:/ctx:ro" -v "$d/w:/tmp/w" -v "$d/out:/out" \
      "$IMAGE" /ranikust --context /ctx --no-push --state-dir /tmp/w \
      --allow network.none-userns ${flag[@]+"${flag[@]}"} 2>&1)" || rc=$?
    echo "$log" | sudo tee "$d/allowed-$seccomp.log" >/dev/null
    if [ "$rc" != 0 ] || ! grep -q '^netns checks: ok' <<<"$log"; then
      echo "error: sysadmin: the user-namespace RUN checks failed, $seccomp profile ($rc):" >&2
      echo "$log" >&2
      exit 1
    fi
    n="$(sed -n 's/^seccomp filters: \([0-9]*\)$/\1/p' <<<"$log" | head -1)"
    if [ -z "$n" ]; then
      echo "error: sysadmin: no seccomp filter count, $seccomp profile:" >&2
      echo "$log" >&2
      exit 1
    fi
    if [ "$seccomp" = default ]; then
      default_n="$n"
    elif [ "$n" != "$((default_n - 1))" ]; then
      echo "error: sysadmin: $n seccomp filters with --seccomp=unconfined, $default_n without: want one fewer" >&2
      exit 1
    fi
  done
  grep -E '^NS_GET_USERNS ' <<<"$log" || true
  echo "sysadmin: with --allow network.none-userns, loopback is up, the filter refuses what it closes over the default profile ($default_n filters) and without it, and useradd works"
}

# #74: `--mode=auto` also probes the user-namespace path of a `RUN`,
# reported only, once per build. Prints the line of variant $1 from its
# build log $2: on the VM itself (real root) it must pass; a default
# `docker run` must stop at `unshare`, which Docker's seccomp refuses
# without CAP_SYS_ADMIN.
userns_line() {
  local line
  line="$(grep -o 'user namespace probe (reported only).*' <<<"$2")" ||
    { echo "error: $1: no user namespace probe in the build log" >&2; exit 1; }
  [ "$(wc -l <<<"$line")" = 1 ] ||
    { echo "error: $1: more than one user namespace probe line" >&2; exit 1; }
  echo "$1: $line"
  case "$1" in
    host) [[ "$line" == *"): passes; "* ]] ;;
    default) [[ "$line" == *"): stops at unshare; unshare(CLONE_NEWUSER): EPERM; "* ]] ;;
  esac || { echo "error: $1: the user namespace probe did not end as expected" >&2; exit 1; }
}

# A default `docker run` with no `--mode`: kaniko mode on its `/`.
default_userns() {
  local d="$W/default" rc=0 log
  sudo mkdir -m 700 -p "$d/ctx"
  printf 'FROM %s\nRUN true\n' "$E2E_BASE" | sudo tee "$d/ctx/Dockerfile" >/dev/null
  log="$(sudo docker run --rm -v "$RNK:/ranikust/bin/ranikust:ro" -v "$d/ctx:/ranikust/ctx:ro" \
    "$IMAGE" /ranikust/bin/ranikust --context /ranikust/ctx --no-push 2>&1)" || rc=$?
  echo "$log" | sudo tee "$d/build.log" >/dev/null
  if [ "$rc" != 0 ] || ! grep -q 'kaniko mode, because root may not mount here' <<<"$log"; then
    echo "error: default: --mode=auto in a default docker run: exit $rc, want 0 in kaniko mode" >&2
    echo "$log" >&2
    exit 1
  fi
  userns_line default "$log"
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
  grep -q "^probe: Caps { root_entry: $entry, proc: Fresh, sysfs: Fresh }" "$dir/log" ||
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
  userns_line "$name" "$(sudo cat "$W/$name/e2e.log")"
  [ "$name" = sysadmin ] && netns_checks
  sudo bash -c "$(declare -f smoke_check_no_stub_leak smoke_check_no_secret smoke_check_no_empty_layer); \
    smoke_check_no_stub_leak '$dir/e2e/out/oci' && \
    smoke_check_no_secret '$dir/e2e/out/oci' '$SMOKE_E2E_SECRET' && \
    smoke_check_no_empty_layer '$dir/e2e/out/oci'"
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
      sudo "$RNK" --context "$p/$1" --no-push --state-dir "$p/$2" --build-arg "NONCE=$nonce" \
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
      sudo "$RNK" --context "$c/ctx" --no-push --state-dir "$c/w" \
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
    # T8 (ADR-0080): a RUN that clears its own death signal still dies with
    # a ranikust killed by SIGKILL. Its init, PID 1 of the RUN's namespace,
    # keeps the signal; the command runs as PID 2 under it.
    d="$W/host/deathsig"
    sudo mkdir -m 700 -p "$d/ctx" "$d/w"
    sudo tee "$d/ctx/Dockerfile" >/dev/null <<EOF
FROM $IMAGE
RUN python3 -c 'import ctypes, time; c = ctypes.CDLL(None); c.prctl(1, 0, 0, 0, 0); c.prctl(15, b"rnkdeath", 0, 0, 0); time.sleep(300)'
EOF
    dlog="$(mktemp)"
    sudo "$RNK" --context "$d/ctx" --no-push --state-dir "$d/w" >"$dlog" 2>&1 &
    for _ in $(seq 120); do pgrep -x rnkdeath >/dev/null && break; sleep 1; done
    pgrep -x rnkdeath >/dev/null ||
      { echo "error: the death-signal RUN never started" >&2; cat "$dlog" >&2; exit 1; }
    [ "$(sudo awk '/^NSpid:/ {print $NF}' "/proc/$(pgrep -x rnkdeath)/status")" = 2 ] ||
      { echo "error: the RUN's command is not PID 2 of its namespace" >&2; exit 1; }
    sudo pkill -KILL -x ranikust
    wait $! || true
    for _ in $(seq 10); do pgrep -x rnkdeath >/dev/null || break; sleep 1; done
    ! pgrep -x rnkdeath >/dev/null ||
      { echo "error: a RUN that cleared its death signal outlived ranikust" >&2; sudo pkill -KILL -x rnkdeath; exit 1; }
    ! pgrep -x ranikust-init >/dev/null ||
      { echo "error: the RUN's init outlived ranikust" >&2; exit 1; }
    echo "host: a RUN that cleared its death signal dies with ranikust (SIGKILL)"
    # ADR-0101: a build runs from a sealed copy of its binary, which every
    # process it forks shares: a sealed memfd (overlay mode, and kaniko
    # mode with a directory per stage), the O_TMPFILE copy when the memfd
    # is skipped. While the RUN waits, root on the host may write neither
    # ranikust's nor its init's /proc/<pid>/exe (guest/sealed-exe.py): the
    # memfd's open or its seal refuses, the O_TMPFILE copy's open (ETXTBSY,
    # its only guard). The RUN cannot even open its init's: the init is not
    # dumpable (ADR-0051), the barrier before the seal; the RUN reports the
    # errno. The RUN still gets the core-file limit ranikust started with.
    s="$W/host/sealed"
    sudo mkdir -m 700 -p "$s/ctx"
    sudo tee "$s/ctx/Dockerfile" >/dev/null <<DOCKERFILE
FROM $IMAGE
RUN <<PY
#!/usr/local/bin/python3
import ctypes, errno, os, resource, signal, sys
ctypes.CDLL(None).prctl(15, b"rnksealed", 0, 0, 0)
core = resource.getrlimit(resource.RLIMIT_CORE)[0]
if core != 1048576:
    sys.exit(f"RUN: RLIMIT_CORE is {core}, want 1048576")
for flags in (os.O_WRONLY, os.O_RDWR):
    try:
        os.close(os.open("/proc/1/exe", flags))
    except OSError as e:
        print(f"RUN: its init's /proc/1/exe for writing: {errno.errorcode[e.errno]}", flush=True)
    else:
        sys.exit("RUN: opened its init's /proc/1/exe for writing")
signal.signal(signal.SIGUSR1, lambda *a: sys.exit(0))
signal.alarm(120)
signal.pause()
PY
DOCKERFILE
    for variant in overlay:memfd kaniko:memfd seal-tmpfile:tmpfile; do
      how="${variant%%:*}" way="${variant#*:}"
      args=(--context "$s/ctx" --no-push --state-dir "$s/w-$how" --log-format=text -v debug)
      fault=()
      case "$how" in
        kaniko) args+=(--mode=kaniko) ;;
        seal-tmpfile) fault=(RANIKUST_DEBUG_FAULT=seal-tmpfile) ;;
      esac
      slog="$(mktemp)"
      sudo env "${fault[@]}" prlimit --core=1048576:unlimited "$RNK" "${args[@]}" >"$slog" 2>&1 &
      # Kept now: the process substitution below changes $!.
      build=$!
      for _ in $(seq 120); do pgrep -x rnksealed >/dev/null && break; sleep 1; done
      pgrep -x rnksealed >/dev/null || {
        echo "error: sealed ($how): the RUN never started" >&2; cat "$slog" >&2
        sudo pkill -KILL -x ranikust; exit 1
      }
      mapfile -t pids < <(pgrep -x ranikust; pgrep -x ranikust-init)
      [ "${#pids[@]}" = 2 ] || {
        echo "error: sealed ($how): want one ranikust and one init, found ${pids[*]}" >&2
        sudo pkill -KILL -x ranikust; exit 1
      }
      sudo python3 -I guest/sealed-exe.py "$way" "$RNK" "${pids[@]}" ||
        { sudo pkill -KILL -x ranikust; exit 1; }
      sudo pkill -USR1 -x rnksealed
      wait "$build" || { echo "error: sealed ($how): the build failed" >&2; cat "$slog" >&2; exit 1; }
      # The first way tried, so none passed over.
      grep -qE "msg=\"running from a sealed copy of the binary \(ADR-0101\)\".* way=$way\$" "$slog" &&
        ! grep -q "passed_over=" "$slog" ||
        { echo "error: sealed ($how): no $way copy in the log" >&2; cat "$slog" >&2; exit 1; }
      [ "$(grep -c "^RUN: its init" "$slog")" = 2 ] ||
        { echo "error: sealed ($how): the RUN did not report both opens" >&2; cat "$slog" >&2; exit 1; }
      grep "^RUN: its init" "$slog" | sed "s/^/host: $how: /"
      echo "host: $how builds from a sealed $way copy no one may write"
    done
  fi
  echo "$name: ok"
done

echo "=== default: a default docker run, --mode=auto"
default_userns
echo "default: ok"

# #78: smoke-build as this regular user, in the build's user namespace
# through newuidmap. Where AppArmor restricts unprivileged user namespaces
# (Ubuntu 24.04) it is refused before anything runs; elsewhere (22.04) a
# RUN gets a fresh proc in its own PID namespace, so every check applies,
# and an rbind of /sys, whose network namespace the user does not own;
# neither needs --allow proc.rbind.
echo "=== user: $(id -un), smoke-build --userns"
dir="$W/user" rc=0
sudo mkdir -p "$dir" && sudo chown "$(id -u):$(id -g)" "$dir"
mkdir -p "$dir/out" "$dir/work"
tag=localhost/rnk-smoke-user:latest
"$BIN" --userns --image "$IMAGE" --work "$dir/work" --out "$dir/out" --tag "$tag" "${runs[@]}" \
  >"$dir/log" 2>&1 || rc=$?
grep -E '^(userns|probe)' "$dir/log" || true
if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ]; then
  [ "$rc" != 0 ] && grep -q '^smoke-build: no user namespace' "$dir/log" ||
    { echo "error: user: expected the refusal of a restricted user namespace, got $rc:" >&2; cat "$dir/log" >&2; exit 1; }
  echo "user: refused where AppArmor restricts unprivileged user namespaces, as expected"
else
  [ "$rc" = 0 ] || { echo "error: user: smoke-build failed ($rc):" >&2; cat "$dir/log" >&2; exit 1; }
  grep -qx 'probe: Caps { root_entry: PivotRoot, proc: Fresh, sysfs: Rbind }' "$dir/log" ||
    { echo "error: user: expected a fresh proc and an rbind of /sys" >&2; cat "$dir/log" >&2; exit 1; }
  smoke_check_no_stub_leak "$dir/out"
  tar -C "$dir/out" -cf "$dir/oci.tar" .
  loaded="$(sudo docker load -i "$dir/oci.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
  got="$(sudo docker run --rm "$loaded" sh -c 'cat /hello && jq --version')"
  [[ "$got" == "hello from ranikust"$'\n'jq-* ]] ||
    { echo "error: user: the image does not run as built: $got" >&2; exit 1; }
  sudo docker rmi -f "$loaded" >/dev/null
fi
# #79: the ranikust binary as this user builds the rootless smoke case's
# `content` stage (a mapped owner, a setgid directory, a symlink, a
# whiteout) in rootless overlay mode, with a fresh proc, and the
# --reproducible digest root's build of it gets, then tests/smoke/e2e,
# whose `RUN --network=none` reads /sys/class/net: a new sysfs for the
# network namespace the build owns, not the rbind of /sys; where AppArmor
# restricts the user namespace, it refuses, naming the switch.
ud="$dir/rootless" rc=0
# The user's umask may leave a new directory group-writable, which a state
# directory and every directory above it may not be (ADR-0050).
mkdir -m 700 -p "$ud" "$ud/w"
sudo mkdir -m 700 -p "$W/rootless-root"
rl_args=(--context "$HOME/rnk/guest/cases/rootless" --no-push --target content --reproducible
  --build-arg "BASE=$E2E_BASE" --destination localhost/rnk-rootless:latest)
log="$(sudo "$RNK" "${rl_args[@]}" --state-dir "$W/rootless-root" 2>&1)" ||
  { echo "error: user: root's build of the rootless case failed:" >&2; echo "$log" >&2; exit 1; }
root_digest="$(smoke_digest "$log")"
log="$("$RNK" "${rl_args[@]}" --state-dir "$ud/w" 2>&1)" || rc=$?
printf '%s\n' "$log" >"$dir/rootless.log"
grep -E 'user namespace probe|rootless' <<<"$log" || true
if [ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ]; then
  [ "$rc" = 1 ] && grep -q 'rootless overlay mode cannot run here: the user namespace probe stops at' <<<"$log" &&
    grep -q 'where kernel.apparmor_restrict_unprivileged_userns is 1' <<<"$log" ||
    { echo "error: user: expected rootless overlay mode refused, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "user: ranikust refuses rootless overlay mode where AppArmor restricts it, as expected"
else
  [ "$rc" = 0 ] &&
    grep -q "ranikust: overlay mode, rootless, in the build's user namespace, RUN enters its root with pivot_root, proc: a new instance" <<<"$log" ||
    { echo "error: user: expected rootless overlay mode with a fresh proc, got $rc:" >&2; echo "$log" >&2; exit 1; }
  got="$(smoke_digest "$log")"
  [ "$got" = "$root_digest" ] ||
    { echo "error: user: --reproducible digest $got, root's $root_digest" >&2; exit 1; }
  echo "user: ranikust builds in rootless overlay mode, with root's --reproducible digest $got"
  ue="$ud/e2e" rc=0
  mkdir -m 700 -p "$ue/w" "$ue/out"
  printf %s "$SMOKE_E2E_SECRET" >"$ue/token"
  smoke_e2e_args "$CTX" "$ue/w" "$ue/out" "$ue/token"
  e2e_args+=(--build-arg "BASE=$E2E_BASE")
  log="$("$RNK" "${e2e_args[@]}" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/rootless-e2e.log"
  [ "$rc" = 0 ] || { echo "error: user: tests/smoke/e2e failed ($rc):" >&2; echo "$log" >&2; exit 1; }
  smoke_check_no_stub_leak "$ue/out/oci"
  smoke_check_no_secret "$ue/out/oci" "$SMOKE_E2E_SECRET"
  loaded="$(sudo docker load -i "$ue/out/image.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
  got="$(sudo docker run --rm "$loaded")"
  [ "$got" = "$SMOKE_E2E_OUTPUT" ] ||
    { echo "error: user: the e2e image printed: $got" >&2; exit 1; }
  sudo docker rmi -f "$loaded" >/dev/null
  echo "user: ranikust builds tests/smoke/e2e in rootless overlay mode; RUN --network=none sees lo alone"
fi
echo "user: ok"

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
      ranikust) args="--no-push --state-dir /tmp/w" ;;
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
