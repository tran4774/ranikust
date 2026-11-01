case_run() {
  build img >/dev/null
  expect "$(run img)" "ok" "checks inside the image"
  # RUN true, RUN true, WORKDIR /tmp, WORKDIR /tmp change nothing: no layer,
  # only their history entries with empty_layer (ADR-0058); every other
  # entry has its layer.
  expect "$(config img | python3 -I -c '
import json, sys
c = json.load(sys.stdin)
h, d = c["history"], c["rootfs"]["diff_ids"]
print([e.get("empty_layer", False) for e in h[-6:-2]],
      sum(not e.get("empty_layer", False) for e in h) == len(d))')" \
    "[True, True, True, True] True" "the no-op steps' history and layers"
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
