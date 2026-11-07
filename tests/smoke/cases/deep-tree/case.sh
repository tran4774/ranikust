# A stage that leaves a tree 3000 levels deep (T15; its path is past
# PATH_MAX): the build succeeds, the next stage's root holds nothing of it,
# and the build leaves nothing of its own in the state directory. At T1
# ranikust runs under 1024 descriptors, a third of the tree's depth.
case_run() {
  local st before after left
  # The leg's state directory: run-cases.sh's volume at T1, the VM's own
  # otherwise; none before the first build.
  if [ "$ENGINE" = podman ]; then
    st="$WORK"
    PODMAN_ARGS+=(--ulimit nofile=1024:1024)
    entries() { { find "$st/builds" "$st/tmp" -mindepth 1 -maxdepth 1 2>/dev/null || true; } | sort; }
  else
    st="$W/state"
    [ "${RANIKUST_QUICK_MODE:-}" = kaniko ] && st="$W/kstate"
    entries() { { sudo find "$st/builds" "$st/tmp" -mindepth 1 -maxdepth 1 2>/dev/null || true; } | sort; }
  fi
  before="$(entries)"
  build img >/dev/null
  expect "$(run img)" 3000 "the levels the deep stage made"
  after="$(entries)"
  left="$(comm -13 <(echo "$before") <(echo "$after"))"
  expect "$left" "" "what the build left in $st"
}
