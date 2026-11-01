# Red team of per-stage directories (ADR-0059). Every mode: sibling stages
# running at once reach neither each other nor a mount, a sysctl or a raw
# disk; a writer left by a RUN is gone before a stage built FROM its stage
# starts; a stage built FROM a hostile one gets its links as links, its
# setuid hardlink, its 15 TiB sparse file as a sparse file; --reproducible
# gives one digest at --jobs 1 and 4 with stages built FROM stages (copied,
# or handed over to the only stage that reads them, ADR-0061, ADR-0062),
# also where a copy and a directory handed over differ in a directory's
# size. Kaniko
# mode off / (the VM's `quick` with RANIKUST_QUICK_MODE=kaniko): FIFOs and
# device nodes are left out of the child, no stage directory outlives a
# build, builds that fill a small state directory (a RUN, a copy of a
# big parent) fail and leave nothing behind, and two SIGINTs during a
# hand-over stop the build and leave nothing outside it.
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

  local d1 d4 metrics=()
  # Kaniko mode off /: the metrics show the hand-over (ADR-0061).
  [ -z "$per_stage" ] || metrics=(--metrics-file "$(out repro4)/metrics.json")
  d1="$(build repro1 -f Dockerfile.repro --reproducible --jobs 1)"
  d4="$(build repro4 -f Dockerfile.repro --reproducible --jobs 4 "${metrics[@]}")"
  expect "$d4" "$d1" "digest at --jobs 4 and --jobs 1"
  expect "$(run repro1)" "p c s1 s2 q " "the reproducible image"
  if [ -n "$per_stage" ]; then
    grep -q 'rnk_snapshot.hand_over' "$(out repro4)/metrics.json" ||
      fail "no stage took its parent's directory over (metrics in $(out repro4))"
  fi

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
  case_run_signal
}

# Red team (ADR-0061, T4): SIGINT twice while a stage takes its parent's
# directory over. The build stops at once (130); nothing outside its
# `builds/<id>` and `tmp/<id>` changes (the store a first build filled, a
# sentinel beside the state directory); no mount or process is left; the
# next build in the same state directory cleans what it left and works.
case_run_signal() {
  local s="$W/sigstate" o rc bg phase before after
  o="$(out signal)"
  rm -rf "$o" && mkdir -p "$o"
  sudo mkdir -m 700 "$s" "$W/sentinel"
  sudo sh -c "echo host >'$W/sentinel/file'"
  sig_build() {
    sudo "$RNK" --context "$SMOKE_CASES_DIR/$CASE" -f Dockerfile.signal --mode=kaniko \
      --state-dir "$s" --no-push --build-arg "BASE=$BASE"
  }
  sig_build >"$o/warm.log" 2>&1 || fail "the first build failed (log in $o)" || return 1
  before="$(sudo find "$s" \( -path "$s/builds" -o -path "$s/tmp" \) -prune -o -printf '%y %p\n' | sort)"
  sig_build >"$o/build.log" 2>&1 &
  bg=$!
  # Root waits for the child's stage directory, made just before the
  # hand-over (stages/1), signals twice, waits for the process to end and
  # reports which stage directories hold a root then.
  phase="$(sudo S="$s" sh -c '
    n=0
    until pgrep -x ranikust >/dev/null; do
      n=$((n + 1)); [ "$n" -lt 100000 ] || { echo "no build started"; exit 0; }
    done
    while :; do
      for d in "$S"/builds/*/stages/1; do [ -e "$d" ] && break 2; done
      pgrep -x ranikust >/dev/null || { echo "build ended first"; exit 0; }
    done
    pid="$(pgrep -o -x ranikust)"
    kill -INT "$pid"; sleep 0.02; kill -INT "$pid"
    while kill -0 "$pid" 2>/dev/null; do :; done
    echo "roots in stages:" $(cd "$S"/builds/*/stages 2>/dev/null && ls -d */rootfs 2>/dev/null)
  ')"
  rc=0
  wait "$bg" || rc=$?
  printf '%s\n' "$phase" >"$o/phase"
  echo "  signal: $phase, exit $rc"
  expect "$rc" 130 "exit status after two SIGINTs ($phase; log in $o)" || return 1
  after="$(sudo find "$s" \( -path "$s/builds" -o -path "$s/tmp" \) -prune -o -printf '%y %p\n' | sort)"
  expect "$after" "$before" "the state directory outside builds/ and tmp/" || return 1
  expect "$(sudo cat "$W/sentinel/file")" host "the sentinel beside the state directory" || return 1
  expect "$(sudo ls -A "$W/sentinel")" file "the sentinel directory" || return 1
  ! grep -qF "$W/" /proc/self/mountinfo || fail "mounts left under $W" || return 1
  ! pgrep -x ranikust >/dev/null || fail "ranikust processes left" || return 1
  sig_build >"$o/after.log" 2>&1 || fail "the build after the killed one failed (log in $o)" || return 1
  expect "$(sudo find "$s/builds" "$s/tmp" -mindepth 1 -maxdepth 1 2>/dev/null)" "" \
    "what the killed build left, after the next one" || return 1
  sudo rm -rf "$s" "$W/sentinel"
}
