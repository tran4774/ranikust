# COPY --from and FROM a stage after another stage reused the root: what
# kaniko mode keeps (ADR-0051). A hostile stage's links, special files and
# metadata come back as they were; sources reaching the state directory,
# /ranikust or /proc fail, never copying them.
case_run() {
  build img >/dev/null
  expect "$(run img)" "ok" "checks inside the image"
  local n
  for n in state rnk environ store; do
    refused "$n" -f "Dockerfile.$n"
  done
}

# refused NAME ARG...: the build NAME (as `build`) must fail.
refused() {
  local name="$1" o
  shift
  o="$(out "$name")"
  rm -rf "$o" && mkdir -p "$o"
  if rnk_run "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
    --destination "localhost/rnk-case:$CASE-$name" "$@" >"$o/build.log" 2>&1; then
    fail "build $name succeeded: it read what is never kept"
  fi
  [ ! -e "$o/image.tar" ] || fail "build $name wrote an image"
  smoke_check_no_secret_in "$SMOKE_CASE_SECRET" "$o/build.log"
}
