# Red team of per-stage directories (ADR-0059). Every mode: sibling stages
# running at once reach neither each other nor a mount, a sysctl or a raw
# disk; a writer left by a RUN is gone before a stage built FROM its stage
# starts; a stage built FROM a hostile one gets its links as links, its
# setuid hardlink, its 15 TiB sparse file as a sparse file; --reproducible
# gives one digest at --jobs 1 and 4 with stages built FROM stages. Kaniko
# mode off / (the VM's `quick` with RANIKUST_QUICK_MODE=kaniko): FIFOs and
# device nodes are left out of the child, no stage directory outlives a
# build, and builds that fill a small state directory (a RUN, a copy of a
# big parent) fail and leave nothing behind.
case_run() {
  local got per_stage=
  [ "${RANIKUST_QUICK_MODE:-}" = kaniko ] && [ "$ENGINE" = "sudo docker" ] && per_stage=1
  build img --jobs 4 >/dev/null
  got="$(run img)"
  printf '%s\n' "$got" >"$(out img)/report"
  local want="size 16492674416640
sparse yes
mid mid
suid yes linked yes
links / ../../../../../.. /proc/self/root
newline nl"
  if [ -n "$per_stage" ]; then
    grep -qF 'kaniko mode with a directory per stage' "$(out img)/build.log" ||
      fail "not built with a directory per stage"
    want="fifo no
chr no
blk no
$want"
  else
    want="fifo yes
$(grep -E '^(chr|blk) ' <<<"$got")
$want"
  fi
  expect "$got" "$want" "what the stages saw (empty reports from a, b and heir, then the child's)"

  local d1 d4
  d1="$(build repro1 -f Dockerfile.repro --reproducible --jobs 1)"
  d4="$(build repro4 -f Dockerfile.repro --reproducible --jobs 4)"
  expect "$d4" "$d1" "digest at --jobs 4 and --jobs 1"
  expect "$(run repro1)" "p c s1 s2 " "the reproducible image"

  [ -z "$per_stage" ] || case_run_per_stage
}

# The VM, kaniko mode off /: nothing left in the state directory, and a
# full filesystem fails a build cleanly.
case_run_per_stage() {
  local left full="$W/full" ctx o rc log
  left="$(sudo find "$W/kstate/builds" -mindepth 1 -maxdepth 1 2>/dev/null)"
  expect "$left" "" "build directories left in the state directory"
  sudo mkdir -m 700 "$full"
  sudo mount -t tmpfs -o size=160m,mode=0700 rnk-full "$full"
  trap 'sudo umount "$full" 2>/dev/null || true' EXIT
  for f in fill amplify; do
    o="$(out "$f")"
    rm -rf "$o" && mkdir -p "$o"
    rc=0
    log="$(sudo "$RNK" --context "$SMOKE_CASES_DIR/$CASE" -f "Dockerfile.$f" --mode=kaniko \
      --state-dir "$full/state" --no-push --build-arg "BASE=$BASE" 2>&1)" || rc=$?
    printf '%s\n' "$log" >"$o/build.log"
    [ "$rc" != 0 ] || fail "$f: the build succeeded on a full filesystem"
    grep -qiE 'no space left|os error 28|exit (status|code):? ?1' <<<"$log" ||
      { echo "$log" >&2; fail "$f: no sign of the full filesystem"; }
    left="$(sudo find "$full/state/builds" -mindepth 1 -maxdepth 1 2>/dev/null)"
    expect "$left" "" "$f: build directories left"
    ! grep -qF "$full/" /proc/self/mountinfo || fail "$f: mounts left below the state directory"
    ! pgrep -x ranikust >/dev/null || fail "$f: ranikust processes left"
  done
  # The space is back: a small build works in the same state directory.
  ctx="$(out after)/ctx"
  rm -rf "$(out after)" && mkdir -p "$ctx"
  printf 'ARG BASE\nFROM ${BASE}\nRUN echo ok >/ok\n' >"$ctx/Dockerfile"
  sudo "$RNK" --context "$ctx" --mode=kaniko --state-dir "$full/state" --no-push \
    --build-arg "BASE=$BASE" >"$(out after)/build.log" 2>&1 ||
    fail "a build after the full ones failed (log in $(out after))"
  sudo umount "$full"
  trap - EXIT
}
