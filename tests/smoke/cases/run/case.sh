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
  case_run_direct_attacks
}

# Red team (T7, T11, T21, ADR-0057): direct-attack.Dockerfile's RUNs fail
# the build when a defence gives way; then what must be refused is, and no
# layer names the script.
case_run_direct_attacks() {
  local o blob
  build attack --dockerfile "$IN/ctx/direct-attack.Dockerfile" >/dev/null
  o="$(out attack)"
  expect "$(run attack)" "chomped /dev/fd env first second closed " "attack output"
  [ "$(grep -c rnk-flood "$o/build.log")" -ge 104000 ] || fail "the flood was not relayed whole"
  for blob in "$o"/oci/blobs/sha256/*; do
    if gzip -t "$blob" 2>/dev/null && tar -tzf "$blob" | grep -Eq '(^|/)(dev/pipes|EOF|memfd)'; then
      fail "a layer names the heredoc script: $blob"
    fi
  done
  # Option spellings that would make a secret a file, or run without a
  # network namespace, are refused, never run without.
  refused dst 'RUN --mount=type=secret as a file' "RUN --mount=type=secret,id=tok,env=TOK,dst=/x true"
  refused destination 'RUN --mount=type=secret as a file' \
    "RUN --mount=type=SECRET,id=tok,env=TOK,Destination=/x true"
  refused emptyenv 'is not a variable name' $'ARG E=\nRUN --mount=type=secret,id=tok,env=$E true'
  refused typebind 'type=bind' "RUN --mount=type=secret,id=tok,env=TOK,type=bind true"
  refused argnet 'network' $'ARG N=none\nRUN --network=$N true'
  refused uppernet 'network' "RUN --network=NONE true"
  refused heredocnet '--network=none' $'RUN --network=none <<EOF\n#!/bin/sh\ntrue\nEOF'
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
