case_run() {
  if [ "${RANIKUST_CASE_MODE:-}" = kaniko ]; then
    case_run_direct
    return
  fi
  build first >/dev/null
  expect "$(run first)" "shell exec heredoc shebang " "output"
  smoke_check_empty_layer "$(out first)/oci"
  # The second build sees what the first left in the cache mount; the
  # first saw at most marks of earlier runs on the same workdir.
  local before after
  before="$(run first cat /seen | wc -l)"
  build second >/dev/null
  after="$(run second cat /seen | wc -l)"
  expect "$after" "$((before + 1))" "marks the second build saw in the cache"
}

# T1 kaniko mode, on the container's `/` with RUN `Direct` (ADR-0057): what
# needs no mount is built, a cache mount is left out with a warning; a
# secret file and --network=none (in the Dockerfile or as the flag) are
# refused, never run without.
case_run_direct() {
  build direct --dockerfile "$IN/ctx/direct.Dockerfile" >/dev/null
  expect "$(run direct)" "shell exec heredoc shebang " "output"
  smoke_check_empty_layer "$(out direct)/oci"
  grep -qF 'RUN --mount=type=cache,target=/cache: ignored' "$(out direct)/build.log" ||
    fail "no warning for the ignored cache mount"
  refused secret 'RUN --mount=type=secret as a file' "RUN --mount=type=secret,id=tok true"
  refused network 'RUN --network=none' "RUN --network=none true"
  refused flag '--network=none' "RUN true" --network=none
}

# refused NAME MESSAGE INSTRUCTION [ARG...]: a build of FROM BASE and
# INSTRUCTION fails, saying MESSAGE.
refused() {
  local name="$1" msg="$2" line="$3" o ctx rc=0 log
  shift 3
  o="$(out "$name")"
  ctx="$o/ctx"
  rm -rf "$o" && mkdir -p "$ctx"
  printf 'ARG BASE\nFROM ${BASE}\n%s\n' "$line" >"$ctx/Dockerfile"
  log="$(rnk_run "$ctx" "$o" --build-arg "BASE=$BASE" "$@" 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  [ "$rc" != 0 ] || fail "$name: the build succeeded" || return 1
  grep -qF -- "$msg" <<<"$log" || { echo "$log" >&2; fail "$name: no refusal"; }
}
