# A daemon left by a RUN changes the stage root while kaniko mode keeps it
# and copies it back (kaniko#1697). The build may fail closed; a built image
# must hold nothing of the state directory, /ranikust or the secret.
case_run() {
  local o rc=0
  o="$(out img)"
  rm -rf "$o" && mkdir -p "$o"
  rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
    --destination "localhost/rnk-case:$CASE-img" >"$o/build.log" 2>&1 || rc=$?
  smoke_check_no_secret_in "$SMOKE_CASE_SECRET" "$o/build.log"
  if [ "$rc" != 0 ]; then
    echo "$CASE: the build failed closed: $(tail -1 "$o/build.log")" >&2
    return 0
  fi
  smoke_check_no_stub_leak "$o/oci"
  smoke_check_no_secret "$o/oci" "$SMOKE_CASE_SECRET"
  expect "$(run img sh /check.sh "$SMOKE_CASE_SECRET")" "ok" "checks inside the image"
}
