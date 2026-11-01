# Red team of COPY and special files (ADR-0060; T18). The context, made
# here (git keeps no FIFO or socket), holds FIFOs and Unix sockets at the
# top, nested and behind a symlinked directory. Kaniko mode on a
# container's / (RANIKUST_CASE_MODE=kaniko at T1: RUN is Direct, no nodev)
# copies none of them, nor the stage's FIFOs and device nodes, and warns
# once per copy; elsewhere (overlay mode, the VM's kaniko mode off /) the
# full sandbox keeps the FIFOs, as BuildKit. A node a RUN makes never
# opens in a later RUN (nodev, or no CAP_MKNOD for a real device in a
# rootless container).
case_run() {
  local o ctx log rc=0 got direct=
  [ "${RANIKUST_CASE_MODE:-}" = kaniko ] && [ "$ENGINE" = podman ] && direct=1
  o="$(out img)"
  ctx="$(out ctx)"
  rm -rf "$o" "$ctx" && mkdir -p "$o" "$ctx/d/e" "$ctx/real"
  cp "$SMOKE_CASES_DIR/$CASE/Dockerfile" "$SMOKE_CASES_DIR/$CASE/check.sh" "$ctx/"
  echo keep >"$ctx/keep.txt"
  echo deep >"$ctx/d/e/keep"
  mkfifo "$ctx/fifo" "$ctx/d/e/fifo" "$ctx/real/fifo"
  ln -s real "$ctx/via"
  # Bound by a relative name from its directory: a socket's absolute path
  # may exceed sun_path.
  python3 -I -c 'import os, socket, sys
for p in sys.argv[1:]:
    os.chdir(os.path.dirname(p))
    socket.socket(socket.AF_UNIX).bind(os.path.basename(p))' "$ctx/sock" "$ctx/d/e/sock"
  log="$(rnk_run "$ctx" "$o" --build-arg "BASE=$BASE" \
    --destination "localhost/rnk-case:$CASE-img" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  [ "$rc" = 0 ] || { echo "$log" >&2; fail "build exited $rc"; }
  smoke_check_no_secret_in "$SMOKE_CASE_SECRET" "$o/build.log"
  got="$(run img)"
  printf '%s\n' "$got" >"$o/report"
  grep -q '^open yes' <<<"$got" && fail "a node a RUN made opened in a later RUN: $got"
  grep -qx 'keep keep deep k ' <<<"$got" || fail "regular files: $got"
  if [ -n "$direct" ]; then
    local bad
    bad="$(grep -E '^/[^ ]+ (p|s|c|b|other)$' <<<"$got" || true)"
    [ -z "$bad" ] || fail "special files reached a root RUN enters without nodev: $bad"
    local n
    n="$(grep -c 'left out .* device node(s), FIFO(s) or socket(s)' "$o/build.log" || true)"
    [ "$n" -ge 6 ] || fail "$n warnings for left-out special files, want one per copy (6+)"
  else
    local p
    for p in /ctx/fifo /ctx/d/e/fifo /lone-fifo /via/fifo /several/fifo /from-stage/d/fifo \
      /lone-stage-fifo; do
      grep -qx "$p p" <<<"$got" || fail "the full sandbox keeps the FIFO $p: $got"
    done
    ! grep -q 'left out' "$o/build.log" || fail "the full sandbox left special files out"
  fi
}
