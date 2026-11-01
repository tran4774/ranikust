# Red team. Every mode: Dockerfile.layers (ADR-0058). Kaniko mode only
# (rnk_run_twice): see the Dockerfile. Checks on
# the host that both volumes hold exactly what they held, that the second
# build found the first's marker, and that no layer of either image holds a
# volume's file, the runtime's markers or /ranikust.
case_run() {
  build layers -f Dockerfile.layers >/dev/null
  expect "$(config layers | python3 -I -c '
import json, sys
c = json.load(sys.stdin)
h, d = c["history"], c["rootfs"]["diff_ids"]
print([e.get("empty_layer", False) for e in h[-6:]],
      sum(not e.get("empty_layer", False) for e in h) == len(d))')" \
    "[False, False, False, False, False, True] True" "the layers of deleting and metadata-only steps"
  declare -F rnk_run_twice >/dev/null || return 0
  local o vol srv log i blob names
  o="$(out twice)"
  rm -rf "$o" && mkdir -p "$o/vol" "$o/srv"
  vol="$o/vol" srv="$o/srv"
  printf VOL-TOKEN-7f3a >"$vol/token"
  printf SRC-9c1e >"$srv/src"
  RNK_TWICE_PODMAN_ARGS=(-v "$vol:/run/vol" -v "$srv:/srv/vol")
  log="$(rnk_run_twice "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" 2>&1)" || {
    echo "$log" >&2
    fail "two builds in one container"
  }
  printf '%s\n' "$log" >"$o/build.log"
  grep -q 'an earlier build ran on /' <<<"$log" || fail "the second build did not find the marker"
  expect "$(grep -Ec '^sha256:[0-9a-f]{64}$' <<<"$log")" 2 "image digests printed"
  expect "$(cat "$vol/token")" VOL-TOKEN-7f3a "the volume below /run"
  expect "$(ls -A "$vol")" token "the names in the volume below /run"
  expect "$(cat "$srv/src")" SRC-9c1e "the volume on /srv"
  expect "$(ls -A "$srv")" src "the names in the volume on /srv"
  for i in 1 2; do
    smoke_check_no_secret "$o/$i/oci" VOL-TOKEN-7f3a
    smoke_check_no_secret "$o/$i/oci" SRC-9c1e
    smoke_check_no_empty_layer "$o/$i/oci"
    # ADR-0058: `RUN rm -f /etc/motd` leaves only a whiteout and keeps its
    # layer; `RUN true` adds none; every other history entry has its layer.
    expect "$(python3 -I "$SMOKE_CASES_DIR/../oci-config.py" "$o/$i/oci" | python3 -I -c '
import json, sys
c = json.load(sys.stdin)
h, d = c["history"], c["rootfs"]["diff_ids"]
print([e.get("empty_layer", False) for e in h[-3:]],
      sum(not e.get("empty_layer", False) for e in h) == len(d))')" \
      "[False, False, True] True" "build $i: the whiteout-only step's layer"
    for blob in "$o/$i"/oci/blobs/sha256/*; do
      gzip -t "$blob" 2>/dev/null || continue
      names="$(tar -tzf "$blob")"
      if grep -Eq '^(\./)?(run/vol|srv/vol|run/\.containerenv|\.dockerenv|ranikust)(/|$)' <<<"$names"; then
        fail "build $i: a layer holds a volume, a runtime marker or /ranikust: $blob"
      fi
    done
  done
}
