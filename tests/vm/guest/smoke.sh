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

# Whether each argument after the first comes in $1, after the one before.
in_order() {
  local rest="$1" part
  shift
  for part in "$@"; do
    [[ "$rest" == *"$part"* ]] || return 1
    rest="${rest#*"$part"}"
  done
}

# The guidance line of a refusal in the log $1: ranikust indents it.
advice_of() { grep '^  ' <<<"$1" || true; }

AA_SWITCH=kernel.apparmor_restrict_unprivileged_userns
AA_PROFILE=/etc/apparmor.d/ranikust
AA_BINARY=/usr/local/bin/ranikust
# Undoes apparmor_profile's changes; its EXIT trap runs it too.
apparmor_undo() {
  if [ -e "$AA_PROFILE" ]; then
    sudo apparmor_parser -R "$AA_PROFILE" >/dev/null 2>&1 || true
    sudo rm -f "$AA_PROFILE"
  fi
  sudo rm -f "$AA_BINARY"
}

# Whether root alone may change what is in the directory $1, as ranikust
# checks it, there and in every directory above: owner, mode, no symlink,
# no nosuid, a filesystem on its list.
root_only_chain() {
  local d="$1"
  while :; do
    [ ! -L "$d" ] && [ "$(stat -c %u "$d")" = 0 ] && [ $((8#$(stat -c %a "$d") & 8#022)) = 0 ] ||
      return 1
    [[ ",$(findmnt -no OPTIONS -T "$d")," != *,nosuid,* ]] || return 1
    case "$(stat -f -c %t "$d")" in
      ef53 | 58465342 | 9123683e | 73717368 | e0f5e1e2 | 1021994 | 794c7630 | 2fc12fc1) ;;
      *) return 1 ;;
    esac
    [ "$d" = / ] && return 0
    d="$(dirname "$d")"
  done
}

usr_local_bin_root_only() {
  root_only_chain /usr/local/bin
}

# #81: root's copy of the binary at /usr/local/bin/ranikust is refused as
# this user at the mount namespace, writing no state directory, with the
# profile as shipped (or, where /usr/local/bin is not root's alone, with the
# install in a directory that is first); with ranikust's profile loaded, it
# builds the rootless case in rootless overlay mode, with a fresh proc and
# root's --reproducible digest, and a RUN keeps the profile; where
# kernel.apparmor_restrict_unprivileged_unconfined is 0, aa-exec enters the
# profile. Uses rl_args, root_digest, ud and dir from the user section.
apparmor_profile() {
  local log rc=0 got aa="$ud/aa"
  command -v apparmor_parser >/dev/null ||
    { echo "error: user: no apparmor_parser where AppArmor restricts user namespaces" >&2; exit 1; }
  [ ! -e "$AA_BINARY" ] && [ ! -e "$AA_PROFILE" ] ||
    { echo "error: user: $AA_BINARY or $AA_PROFILE is there already" >&2; exit 1; }
  trap 'sudo rm -f "$MARKER"; apparmor_undo' EXIT
  sudo install -m 0755 -o root -g root "$RNK" "$AA_BINARY"
  mkdir -m 700 -p "$aa" "$aa/w" "$aa/ctx"
  # A state directory not made yet: the refusal writes nothing.
  log="$("$AA_BINARY" "${rl_args[@]}" --state-dir "$aa/new" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/apparmor-before.log"
  [ ! -e "$aa/new" ] && grep -q 'the user namespace probe stops at mount namespace' <<<"$log" ||
    { echo "error: user: expected a refusal at the mount namespace, writing nothing:" >&2; echo "$log" >&2; exit 1; }
  local advice
  advice="$(advice_of "$log")"
  if usr_local_bin_root_only; then
    [ "$rc" = 1 ] && in_order "$advice" 'run ranikust as root, with sudo; ' \
      "or, as root, copy ranikust's AppArmor profile" \
      'to /etc/apparmor.d/ranikust and run apparmor_parser -r /etc/apparmor.d/ranikust; ' \
      'or run ranikust in a container' && ! grep -q 'first' <<<"$advice" ||
      { echo "error: user: expected the profile as shipped for $AA_BINARY, got $rc:" >&2; echo "$log" >&2; exit 1; }
  else
    [ "$rc" = 1 ] && in_order "$advice" 'run ranikust as root, with sudo; ' \
      'install ranikust first in a directory only root can change, which /usr/local/bin is not here' \
      "$AA_BINARY is not a path only root can change (" 'or run ranikust in a container' ||
      { echo "error: user: expected the install in a root-only directory first, got $rc:" >&2; echo "$log" >&2; exit 1; }
  fi
  echo "user: $AA_BINARY, root's, without the profile: refused; advice: sudo, the profile, a container"
  sudo cp "$HOME/rnk/guest/apparmor-ranikust" "$AA_PROFILE"
  sudo apparmor_parser -r "$AA_PROFILE"
  rc=0
  log="$("$AA_BINARY" "${rl_args[@]}" --state-dir "$aa/w" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/apparmor.log"
  [ "$rc" = 0 ] &&
    grep -q "ranikust: overlay mode, rootless, in the build's user namespace, RUN enters its root with pivot_root, proc: a new instance" <<<"$log" ||
    { echo "error: user: expected rootless overlay mode with a fresh proc under the profile, got $rc:" >&2; echo "$log" >&2; exit 1; }
  got="$(smoke_digest "$log")"
  [ "$got" = "$root_digest" ] ||
    { echo "error: user: under the profile, --reproducible digest $got, root's $root_digest" >&2; exit 1; }
  echo "user: with the profile, $AA_BINARY builds in rootless overlay mode, with root's --reproducible digest $got"
  # shellcheck disable=SC2016 # expanded by the RUN's shell
  printf 'FROM %s\nRUN echo "label: $(cat /proc/self/attr/current)"\n' "$E2E_BASE" >"$aa/ctx/Dockerfile"
  rc=0
  log="$("$AA_BINARY" --context "$aa/ctx" --no-push --state-dir "$aa/w" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/apparmor-run.log"
  [ "$rc" = 0 ] && grep -q '^label: ranikust (unconfined)$' <<<"$log" ||
    { echo "error: user: expected a RUN under the profile, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "user: a RUN keeps the profile: $(grep '^label: ' <<<"$log")"
  # The docs say: where kernel.apparmor_restrict_unprivileged_unconfined is
  # 0, an unconfined program of this user may enter the profile without
  # being ranikust.
  local unconfined
  unconfined="$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_unconfined 2>/dev/null || echo absent)"
  if [ "$unconfined" = 0 ]; then
    command -v aa-exec >/dev/null || { echo "error: user: no aa-exec where AppArmor restricts user namespaces" >&2; exit 1; }
    got="$(aa-exec -p ranikust -- unshare --user --map-root-user --mount true 2>&1)" ||
      { echo "error: user: aa-exec -p ranikust -- unshare -Urm refused at unprivileged_unconfined=0: $got" >&2; exit 1; }
    echo "user: at kernel.apparmor_restrict_unprivileged_unconfined=0, aa-exec -p ranikust gives an unconfined program of this user a user namespace, as the docs say"
    red_team_profile_grant
  else
    echo "user: kernel.apparmor_restrict_unprivileged_unconfined=$unconfined: aa-exec not checked"
  fi
  apparmor_undo
  trap 'sudo rm -f "$MARKER"' EXIT
}

# #81: the profile is offered only for a path on a local filesystem that
# root mounted: root's copy of the binary on a nosuid tmpfs, then on ramfs (off
# the list), owned by root and written by no one else, is refused at the
# mount namespace with the install under /usr/local/bin first, naming the
# mount, and writes no state directory. Uses rl_args, ud and dir from the
# user section.
AA_MOUNT=/mnt/rnk-aa-mount
apparmor_mounts() {
  local m="$AA_MOUNT" log rc advice c fs opts why
  [ ! -L /mnt ] && [ "$(stat -c %u /mnt)" = 0 ] && [ $((8#$(stat -c %a /mnt) & 8#022)) = 0 ] ||
    { echo "error: user: /mnt is not root's alone here" >&2; exit 1; }
  sudo mkdir -m 755 -p "$m"
  trap 'sudo rm -f "$MARKER"; sudo umount "$AA_MOUNT" 2>/dev/null || true; sudo rmdir "$AA_MOUNT" 2>/dev/null || true' EXIT
  for c in "tmpfs|nosuid,mode=755|is on a nosuid mount" \
    "ramfs|mode=755|is not on a local filesystem ranikust knows"; do
    IFS='|' read -r fs opts why <<<"$c"
    sudo mount -t "$fs" -o "$opts" "rnk-$fs" "$m"
    sudo install -m 0755 -o root -g root "$RNK" "$m/ranikust"
    rc=0
    log="$("$m/ranikust" "${rl_args[@]}" --state-dir "$ud/on-$fs" 2>&1)" || rc=$?
    sudo umount "$m"
    printf '%s\n' "$log" >"$dir/apparmor-$fs.log"
    advice="$(advice_of "$log")"
    [ "$rc" = 1 ] && [ ! -e "$ud/on-$fs" ] &&
      grep -q 'the user namespace probe stops at mount namespace' <<<"$log" &&
      in_order "$advice" 'run ranikust as root, with sudo; ' 'or, as root, install ranikust ' \
        "$m/ranikust is not a path only root can change ($m $why" 'or run ranikust in a container' ||
      { echo "error: user: expected no profile for a binary on $fs ($opts), got $rc:" >&2; echo "$log" >&2; exit 1; }
    echo "user: root's binary on $fs ($opts): the profile only after an install elsewhere ($m $why)"
  done
  sudo rmdir "$m"
  trap 'sudo rm -f "$MARKER"' EXIT
}

# #81: a user other than root in a default `docker run`, whose seccomp
# filter refuses unshare(CLONE_NEWUSER) with EPERM, is refused there and
# advised sudo then a container, with no profile whatever the switch reads:
# no profile lifts a seccomp refusal.
seccomp_user_advice() {
  local d="$W/default-user" rc=0 log advice
  sudo mkdir -m 755 -p "$d/ctx"
  printf 'FROM %s\nRUN true\n' "$E2E_BASE" | sudo tee "$d/ctx/Dockerfile" >/dev/null
  log="$(sudo docker run --rm --user 1000:1000 -v "$RNK:/ranikust/bin/ranikust:ro" \
    -v "$d/ctx:/ranikust/ctx:ro" "$IMAGE" /ranikust/bin/ranikust --context /ranikust/ctx \
    --no-push --state-dir /tmp/rnk-state 2>&1)" || rc=$?
  echo "$log" | sudo tee "$d/build.log" >/dev/null
  advice="$(advice_of "$log")"
  [ "$rc" = 1 ] && grep -q 'the user namespace probe stops at unshare' <<<"$log" &&
    grep -q 'unshare(CLONE_NEWUSER): EPERM' <<<"$log" &&
    in_order "$advice" 'run ranikust as root, with sudo; ' 'or run ranikust in a container' &&
    ! grep -qi 'apparmor' <<<"$advice" ||
    { echo "error: default-user: expected a refusal at unshare (EPERM), advising sudo then a container and no profile, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "default-user: a user in a default docker run: refused at unshare (EPERM); advice: sudo, a container, no profile ($AA_SWITCH: $(cat "/proc/sys/${AA_SWITCH//.//}" 2>/dev/null || echo absent))"
}

# #81: where the switch is 1, a user other than root in a privileged
# container (no seccomp filter, no AppArmor confinement, the host's id map)
# is refused at the mount namespace and advised sudo then a container, with
# no profile: a path checked in a container is not the one a profile on the
# host attaches to.
privileged_user_advice() {
  local d="$W/privileged-user" rc=0 log advice
  if [ "$(cat "/proc/sys/${AA_SWITCH//.//}" 2>/dev/null)" != 1 ]; then
    echo "privileged-user: $AA_SWITCH is not 1: not checked"
    return
  fi
  sudo mkdir -m 755 -p "$d/ctx"
  printf 'FROM %s\nRUN true\n' "$E2E_BASE" | sudo tee "$d/ctx/Dockerfile" >/dev/null
  log="$(sudo docker run --rm --privileged --user 1000:1000 -v "$RNK:/ranikust/bin/ranikust:ro" \
    -v "$d/ctx:/ranikust/ctx:ro" "$IMAGE" /ranikust/bin/ranikust --context /ranikust/ctx \
    --no-push --state-dir /tmp/rnk-state 2>&1)" || rc=$?
  echo "$log" | sudo tee "$d/build.log" >/dev/null
  advice="$(advice_of "$log")"
  [ "$rc" = 1 ] && grep -q 'the user namespace probe stops at mount namespace' <<<"$log" &&
    in_order "$advice" 'run ranikust as root, with sudo; ' 'or run ranikust in a container' &&
    ! grep -qi 'apparmor' <<<"$advice" ||
    { echo "error: privileged-user: expected a refusal at the mount namespace, advising sudo then a container and no profile, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "privileged-user: a user in a privileged docker run: refused at the mount namespace; advice: sudo, a container, no profile"
}

# #81: where the switch is 0 (Ubuntu 22.04), or absent, the advice for no
# user namespace (the test hook's) names no profile: sudo, then a container.
# Where it is 1 it is set to 0 for the run, then back.
advice_without_the_switch() {
  local log rc=0 advice before
  before="$(cat "/proc/sys/${AA_SWITCH//.//}" 2>/dev/null || true)"
  if [ "$before" = 1 ]; then
    trap 'sudo rm -f "$MARKER"; sudo sysctl -qw "$AA_SWITCH=1"' EXIT
    sudo sysctl -qw "$AA_SWITCH=0"
  fi
  log="$(RANIKUST_DEBUG_FAULT=no-userns "$RNK" "${rl_args[@]}" --state-dir "$ud/hook" 2>&1)" || rc=$?
  if [ "$before" = 1 ]; then
    sudo sysctl -qw "$AA_SWITCH=1"
    trap 'sudo rm -f "$MARKER"' EXIT
  fi
  advice="$(advice_of "$log")"
  [ "$rc" = 1 ] && grep -q 'refused by RANIKUST_DEBUG_FAULT=no-userns' <<<"$log" &&
    in_order "$advice" 'run ranikust as root, with sudo; ' 'or run ranikust in a container' &&
    ! grep -qi 'apparmor' <<<"$advice" && [ ! -e "$ud/hook" ] ||
    { echo "error: user: expected sudo then a container and no profile at $AA_SWITCH=0, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "user: at $AA_SWITCH=0 (${before:-absent} here) the advice names no profile"
}

# Every node under $1: path, kind, size, mode, owner, inode, times, target.
rt_tree() { find "$1" -printf '%p %y %s %m %U %i %T@ %C@ %l\n' | sort; }

# Kills ranikust ($1, run in $2 with the arguments after $4) $4 times as soon
# as the capability check's child shows: the parent or the child ($3). Fails
# where a process of the run is left, where a killed parent made the state
# directory, or where no child was caught.
RT_KILL='import os, signal, subprocess, sys, time
binary, work, what, tries, args = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5:]
caught, left = 0, []
for i in range(tries):
    state = os.path.join(work, "kill-%s-%d" % (what, i))
    p = subprocess.Popen([binary, *args, "--state-dir", state], cwd=work,
                         env={"PATH": "/usr/bin:/bin", "HOME": work},
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, process_group=0)
    kids, deadline = [], time.monotonic() + 10
    while not kids and time.monotonic() < deadline and p.poll() is None:
        try:
            with open("/proc/%d/task/%d/children" % (p.pid, p.pid)) as f:
                kids = f.read().split()
        except OSError:
            break
    hit = False
    if kids:
        try:
            os.killpg(p.pid, signal.SIGSTOP)
            for pid in [p.pid] if what == "parent" else kids:
                os.kill(int(pid), signal.SIGKILL)
            hit = True
        except ProcessLookupError:
            pass
        try:
            os.killpg(p.pid, signal.SIGCONT)
        except ProcessLookupError:
            pass
    p.wait()
    time.sleep(0.5)
    if hit:
        caught += 1
        if what == "parent" and os.path.exists(state):
            sys.exit("a parent killed during the check made %s" % state)
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open("/proc/%s/stat" % pid) as f:
                stat = f.read()
        except OSError:
            continue
        if int(stat[stat.rindex(")") + 2:].split()[2]) == p.pid:
            left.append((i, pid, stat))
if left or not caught:
    sys.exit("caught %d of %d, left: %r" % (caught, tries, left))
print("%s killed in %d of %d runs, nothing left" % (what, caught, tries))'

# Red team (#81): the real capability check refuses at the mount namespace
# and writes nothing, whatever the state directory is or points to; a check
# killed halfway, in the parent or the child, leaves no process, and a fork
# refused by RLIMIT_NPROC stops the run (exit 1). Uses rl_args, ud and dir.
red_team_no_userns() {
  local r="$ud/red" form log rc before
  mkdir -m 700 "$r"
  mkdir -m 700 "$r/full" "$r/target" "$r/home" "$r/kill"
  echo x >"$r/full/keep"
  echo y >"$r/file"
  ln -s "$r/target" "$r/to-dir"
  ln -s "$r/absent" "$r/dangling"
  before="$(rt_tree "$r")"
  for form in full to-dir dangling a/b/c file default; do
    rc=0
    if [ "$form" = default ]; then
      log="$(cd "$r" && env -u XDG_DATA_HOME HOME="$r/home" "$RNK" "${rl_args[@]}" 2>&1)" || rc=$?
    else
      log="$(cd "$r" && "$RNK" "${rl_args[@]}" --state-dir "$form" 2>&1)" || rc=$?
    fi
    printf '%s\n' "$log" >"$dir/red-team-state-${form//\//-}.log"
    [ "$rc" = 1 ] && grep -q 'the user namespace probe stops at mount namespace' <<<"$log" &&
      [ "$(rt_tree "$r")" = "$before" ] ||
      { echo "error: red team: --state-dir $form: expected a refusal that writes nothing, got $rc:" >&2; echo "$log" >&2; diff <(echo "$before") <(rt_tree "$r") >&2 || true; exit 1; }
  done
  echo "red team: the real check refuses and writes nothing for a state directory full, a symlink to a directory, a dangling symlink, missing parents, a file, the default"
  for form in parent child; do
    python3 -I -c "$RT_KILL" "$RNK" "$r/kill" "$form" 20 "${rl_args[@]}" ||
      { echo "error: red team: killing the check's $form" >&2; exit 1; }
  done
  rc=0
  log="$(prlimit --nproc=1:1 "$RNK" "${rl_args[@]}" --state-dir "$r/nproc" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/red-team-nproc.log"
  [ "$rc" = 1 ] && grep -q "fork the probe's child: Resource temporarily unavailable" <<<"$log" ||
    { echo "error: red team: expected the probe's fork refused under RLIMIT_NPROC=1, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "red team: under RLIMIT_NPROC=1 the check cannot fork and the run stops"
}

# Writes an access ACL on $1 (as root): this user rwx, the mask $2 (octal).
RT_ACL='import os, struct, sys
path, uid, mask = sys.argv[1], int(sys.argv[2]), int(sys.argv[3], 8)
undef = 0xFFFFFFFF
entries = [(0x01, 7, undef), (0x02, 7, uid), (0x04, 5, undef), (0x10, mask, undef), (0x20, 5, undef)]
os.setxattr(path, "system.posix_acl_access",
            struct.pack("<I", 2) + b"".join(struct.pack("<HHI", t, p, i) for t, p, i in entries))'

# Red team (#81): root's binary in a root-only directory outside
# /usr/local/bin gets the profile with its path changed; an ACL that lets
# this user write the directory or the file, or a sticky world-writable
# directory, takes it away; an ACL whose mask keeps writing out does not;
# this user's symlink to it names root's path; a hardlink in this user's
# directory is refused by the kernel or gets no profile. Uses rl_args, ud
# and dir. The directory goes under the first of RT_PARENTS that root alone
# may change: any user may write /opt and /usr/local/bin on CI's runners.
RT_PARENTS="/usr/lib /var/lib"
RT_DIR=
red_team_acl() {
  local parent d b c n=0 setup program expect log rc advice what at mask
  local writable="is writable by its group or others"
  for parent in $RT_PARENTS ""; do
    [ -z "$parent" ] || ! root_only_chain "$parent" || break
  done
  [ -n "$parent" ] || { echo "error: red team: none of $RT_PARENTS is root's alone here" >&2; exit 1; }
  RT_DIR="$parent/rnk-red-team" d="$parent/rnk-red-team" b="$parent/rnk-red-team/ranikust"
  [ ! -e "$d" ] || { echo "error: red team: $d is there already" >&2; exit 1; }
  trap 'sudo rm -f "$MARKER"; sudo rm -rf "$RT_DIR"' EXIT
  ln -s "$b" "$ud/red-link"
  for c in "none|$b|changed to $b, and run" \
    "acl $d 7|$b|$d $writable" \
    "acl $b 7|$b|$b $writable" \
    "acl $d 5|$b|changed to $b, and run" \
    "sticky|$b|$d $writable" \
    "none|$ud/red-link|changed to $b, and run"; do
    IFS='|' read -r setup program expect <<<"$c"
    sudo rm -rf "$d"
    sudo mkdir -m 755 -p "$d"
    sudo install -m 0755 -o root -g root "$RNK" "$b"
    read -r what at mask <<<"$setup"
    case "$what" in
      acl) sudo python3 -I -c "$RT_ACL" "$at" "$(id -u)" "$mask" ;;
      sticky) sudo chmod 1777 "$d" ;;
    esac
    n=$((n + 1)) rc=0
    log="$("$program" "${rl_args[@]}" --state-dir "$ud/red-acl-$n" 2>&1)" || rc=$?
    printf '%s\n' "$log" >"$dir/red-team-acl-$n.log"
    advice="$(advice_of "$log")"
    [ "$rc" = 1 ] && [ ! -e "$ud/red-acl-$n" ] && grep -qF -- "$expect" <<<"$advice" ||
      { echo "error: red team: after '$setup', $program: expected '$expect', got $rc:" >&2; echo "$log" >&2; exit 1; }
    echo "red team: $program after '$setup': $expect"
  done
  if ln "$b" "$ud/red-hard" 2>/dev/null; then
    rc=0
    log="$("$ud/red-hard" "${rl_args[@]}" --state-dir "$ud/red-hard-state" 2>&1)" || rc=$?
    [ "$rc" = 1 ] && ! grep -q 'changed to' <<<"$(advice_of "$log")" ||
      { echo "error: red team: a hardlink in this user's directory got the profile, $rc:" >&2; echo "$log" >&2; exit 1; }
    echo "red team: a hardlink to root's binary in this user's directory gets no profile"
  else
    echo "red team: the kernel refuses this user a hardlink to root's binary"
  fi
  sudo rm -rf "$d"
  rm -f "$ud/red-link" "$ud/red-hard"
  trap 'sudo rm -f "$MARKER"' EXIT
}

# Red team (#81): an unconfined program of this user that enters ranikust's
# profile (aa-exec, where kernel.apparmor_restrict_unprivileged_unconfined
# is 0) gets a user namespace and nothing else it lacks: the same
# capabilities; no command that fails without the profile works with it;
# io_uring stays refused where AppArmor refuses it to the unconfined.
RT_URING='import ctypes, sys
params = ctypes.create_string_buffer(120)
sys.exit(0 if ctypes.CDLL(None, use_errno=True).syscall(425, 1, params) >= 0 else 1)'
red_team_profile_grant() {
  local c caps under switch=kernel.apparmor_restrict_unprivileged_io_uring before
  [ "$(aa-exec -p ranikust -- cat /proc/self/attr/current)" = 'ranikust (unconfined)' ] ||
    { echo "error: red team: aa-exec -p ranikust did not enter the profile" >&2; exit 1; }
  caps="$(grep '^Cap' /proc/self/status)"
  under="$(aa-exec -p ranikust -- grep '^Cap' /proc/self/status)"
  [ "$caps" = "$under" ] ||
    { echo "error: red team: the profile changes the capabilities: $caps / $under" >&2; exit 1; }
  for c in 'cat /etc/shadow' 'touch /etc/rnk-red-team' 'cat /proc/1/environ' \
    'head -c1 /proc/1/mem' 'dmesg' 'ls /root' 'cat /sys/kernel/security/apparmor/profiles' \
    'unshare --user --map-root-user cat /etc/shadow' \
    'unshare --user --map-root-user touch /etc/rnk-red-team'; do
    if ! sh -c "$c" >/dev/null 2>&1 && aa-exec -p ranikust -- sh -c "$c" >/dev/null 2>&1; then
      sudo rm -f /etc/rnk-red-team
      echo "error: red team: '$c' fails without the profile and works with it" >&2
      exit 1
    fi
  done
  echo "red team: the profile adds no capability, and no command that fails without it works with it"
  if before="$(sudo sysctl -n "$switch" 2>/dev/null)"; then
    trap 'sudo rm -f "$MARKER"; apparmor_undo; sudo sysctl -qw "'"$switch=$before"'"' EXIT
    sudo sysctl -qw "$switch=1"
    if python3 -I -c "$RT_URING"; then
      echo "red team: io_uring works at $switch=1 (io_uring_disabled: $(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo absent)): not checked"
    elif aa-exec -p ranikust -- python3 -I -c "$RT_URING"; then
      sudo sysctl -qw "$switch=$before"
      echo "error: red team: at $switch=1 the profile gives io_uring an unconfined program is refused" >&2
      exit 1
    else
      echo "red team: at $switch=1 io_uring stays refused under the profile"
    fi
    sudo sysctl -qw "$switch=$before"
    trap 'sudo rm -f "$MARKER"; apparmor_undo' EXIT
  else
    echo "red team: no $switch here: io_uring not checked"
  fi
}

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
# restricts the user namespace, it refuses from this user's path with sudo,
# the profile after an install under /usr/local/bin, then a container, and
# builds from there once root loaded the profile (#81).
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
  [ "$rc" = 1 ] && grep -q 'rootless overlay mode cannot run here: the user namespace probe stops at mount namespace' <<<"$log" &&
    in_order "$(advice_of "$log")" 'where kernel.apparmor_restrict_unprivileged_userns is 1, as here' \
      'run ranikust as root, with sudo; ' 'or, as root, install ranikust ' \
      "$(realpath "$RNK") is not a path only root can change (" '; or run ranikust in a container' ||
    { echo "error: user: expected rootless overlay mode refused, with sudo, the profile after an install under /usr/local/bin, then a container, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "user: ranikust refuses rootless overlay mode where AppArmor restricts it; advice: sudo, the profile after an install under /usr/local/bin, a container"
  red_team_no_userns
  apparmor_profile
  apparmor_mounts
  red_team_acl
else
  [ "$rc" = 0 ] &&
    grep -q "ranikust: overlay mode, rootless, in the build's user namespace, RUN enters its root with pivot_root, proc: a new instance" <<<"$log" ||
    { echo "error: user: expected rootless overlay mode with a fresh proc, got $rc:" >&2; echo "$log" >&2; exit 1; }
  got="$(smoke_digest "$log")"
  [ "$got" = "$root_digest" ] ||
    { echo "error: user: --reproducible digest $got, root's $root_digest" >&2; exit 1; }
  echo "user: ranikust builds in rootless overlay mode, with root's --reproducible digest $got"
  ue="$ud/e2e" rc=0
  mkdir -m 700 -p "$ue" "$ue/w" "$ue/out"
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
  # A set-up failing in the build's user namespace never falls back to
  # kaniko mode for a user other than root (ADR-0136).
  rc=0
  log="$(RANIKUST_DEBUG_FAULT=rootless-broken "$RNK" "${rl_args[@]}" --state-dir "$ud/w" 2>&1)" ||
    rc=$?
  printf '%s\n' "$log" >"$dir/rootless-broken.log"
  [ "$rc" = 1 ] &&
    grep -q 'rootless overlay mode: refused by RANIKUST_DEBUG_FAULT=rootless-broken' <<<"$log" &&
    ! grep -q 'kaniko mode' <<<"$log" ||
    { echo "error: user: expected the set-up's error, got $rc:" >&2; echo "$log" >&2; exit 1; }
  echo "user: a set-up failing in the build's user namespace is an error, with no fallback"
  # Red team (#79): a RUN's fresh proc shows no process outside it, and it
  # writes no kernel, cgroup or sysfs file, where /sys/fs/cgroup is the
  # host's, writable by this user, nor, with --network=none, sees a network
  # device outside its own; a tree deeper than PATH_MAX it leaves
  # in a cache mount, partly a mapped user's, goes with `prune --all` as
  # this user, and a build in the emptied directory is not refused (T15,
  # T17, T24).
  ur="$ud/reach" rc=0
  mkdir -m 700 -p "$ur" "$ur/w"
  rr=(--context "$HOME/rnk/guest/cases/rootless-reach" --no-push --build-arg "BASE=$E2E_BASE"
    --destination localhost/rnk-reach:latest --state-dir "$ur/w")
  log="$("$RNK" "${rr[@]}" --target writes --tar-path "$ur/writes.tar" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/rootless-reach.log"
  [ "$rc" = 0 ] || { echo "error: user: the rootless-reach writes failed ($rc):" >&2; echo "$log" >&2; exit 1; }
  loaded="$(sudo docker load -i "$ur/writes.tar" | sed -nE 's/^Loaded image( ID)?: //p' | tail -1)"
  report="$(sudo docker run --rm "$loaded" cat /report)"
  sudo docker rmi -f "$loaded" >/dev/null
  printf '%s\n' "$report" >"$dir/rootless-reach.report"
  ! grep -Eq '^(write [^/]*/(proc|sys)/.*|mkdir cgroup|mount proc)=done$|^reached |^seen: [0-9]+ ranikust$' \
    <<<"$report" && [ "$(grep -c '=refused$' <<<"$report")" = 12 ] &&
    grep -q '^control reach=done ' <<<"$report" && grep -qx 'write /tmp/control=done' <<<"$report" &&
    grep -Eqx 'none netdevs=(lo )?' <<<"$report" &&
    grep -Eqx 'none addresses=(00:00:00:00:00:00 )?' <<<"$report" ||
    { echo "error: user: a RUN wrote, saw or reached outside it:" >&2; echo "$report" >&2; exit 1; }
  echo "user: a RUN writes no kernel, cgroup or sysfs file and sees no process outside it"
  rc=0
  log="$("$RNK" "${rr[@]}" --target deep-cache 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$dir/rootless-deep.log"
  [ "$rc" = 0 ] || { echo "error: user: the deep cache tree's build failed ($rc):" >&2; echo "$log" >&2; exit 1; }
  rc=0
  log="$("$RNK" prune --all --state-dir "$ur/w" 2>&1)" || rc=$?
  [ "$rc" = 0 ] && [ -z "$(ls -A "$ur/w")" ] ||
    { echo "error: user: prune --all after a deep cache tree ($rc): $log; left: $(ls -A "$ur/w")" >&2; exit 1; }
  rc=0
  log="$("$RNK" "${rr[@]}" --target userns 2>&1)" || rc=$?
  [ "$rc" = 0 ] || { echo "error: user: a build after prune --all ($rc):" >&2; echo "$log" >&2; exit 1; }
  echo "user: prune --all empties a deep cache tree a RUN left; the next build runs"
fi
advice_without_the_switch
seccomp_user_advice
privileged_user_advice
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
