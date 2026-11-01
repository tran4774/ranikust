case_run() {
  build img >/dev/null
  expect "$(run img)" "ok" "checks inside the image"
  # RUN true, RUN true, WORKDIR /tmp, WORKDIR /tmp: four layers, each the
  # same empty one (Q-131) [open: Q-385].
  expect "$(config img | python3 -I -c '
import json, sys
d = json.load(sys.stdin)["rootfs"]["diff_ids"]
print(len(d[-5:-1]), len(set(d[-5:-1])), d[-5] != d[-1])')" "4 1 True" "the no-op steps' layers"
  # Kaniko mode: a second build in the same container starts from an
  # emptied / (ADR-0052, D1).
  if declare -F rnk_run_twice >/dev/null; then
    local o log
    o="$(out twice)"
    rm -rf "$o" && mkdir -p "$o"
    log="$(rnk_run_twice "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
      --dockerfile /ranikust/ctx/Dockerfile.twice 2>&1)" || {
      echo "$log" >&2
      fail "two builds in one container"
    }
    printf '%s\n' "$log" >"$o/build.log"
    grep -q 'an earlier build ran on /' <<<"$log" || fail "the second build did not find the marker"
    expect "$(grep -Ec '^sha256:[0-9a-f]{64}$' <<<"$log")" 2 "image digests printed"
  fi
}
