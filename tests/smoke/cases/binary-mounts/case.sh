# Red team (ADR-0101, T21): binds that reach kaniko mode's binary on `/`
# (hostile names, a grandparent, the same file twice, a directory root's
# links and parent) are refused, with or without a sealed copy.
case_run() {
  local d t sib g mp nl
  d="$(out binds)"
  rm -rf "$d" && mkdir -p "$d/ctx"
  printf 'FROM scratch\nCOPY Dockerfile /d\n' >"$d/ctx/Dockerfile"
  nl=$'\n'
  t="$d/t a"$'\t'"b${nl}c\\040"
  sib="$d/t a"$'\t'"b${nl}c "
  g="$d/g"
  mkdir -p "$t/sub" "$sib" "$g/p/q" "$g/p/other"
  install -m 0755 "$RNK" "$t/ranikust"
  install -m 0755 "$RNK" "$g/p/q/ranikust"
  mp="/ranikust/b in${nl}x"
  on_slash reached "escaped tree bound rw" "$mp/ranikust" \
    -v "$t/ranikust:$mp/ranikust:ro" -v "$t:/src"
  on_slash built "escaped sibling bound rw" "$mp/ranikust" \
    -v "$t/ranikust:$mp/ranikust:ro" -v "$sib:/src"
  on_slash reached "escaped directory bound ro, its file rw" "$mp/ranikust" \
    -v "$t:$mp:ro" -v "$t/ranikust:/x"
  on_slash built "a directory below the binary's bound rw" "$mp/ranikust" \
    -v "$t:$mp:ro" -v "$t/sub:/s"
  on_slash reached "grandparent bound rw" /ranikust/bin/ranikust \
    -v "$g/p/q/ranikust:/ranikust/bin/ranikust:ro" -v "$g:/g"
  on_slash reached "the same file bound rw at a second path" /ranikust/bin/ranikust \
    -v "$g/p/q/ranikust:/ranikust/bin/ranikust:ro" -v "$g/p/q/ranikust:/x"
  on_slash built "a sibling directory bound rw" /ranikust/bin/ranikust \
    -v "$g/p/q/ranikust:/ranikust/bin/ranikust:ro" -v "$g/p/other:/o"
  local env
  for env in RANIKUST_DEBUG_FAULT=seal-tmpfile RANIKUST_DEBUG_FAULT=seal-none \
    "RANIKUST_SEALED_COPY=memfd root"; do
    on_slash "writable mount other than /'s" "$env, the binary bound rw" /ranikust/bin/ranikust \
      -e "$env" -v "$g/p/q/ranikust:/ranikust/bin/ranikust"
  done
  # Docker has no directory root.
  [ "$ENGINE" = podman ] || return 0
  mkdir -p "$d/root/usr/local/bin" && chmod 755 "$d/root"
  install -m 0755 "$RNK" "$d/root/usr/local/bin/ranikust"
  in_root built "one link" 1
  ln "$d/root/usr/local/bin/ranikust" "$d/alias"
  in_root 'other hard links' "a hard link outside the root" 2
  unlink "$d/alias"
  in_root 'a writable mount also reaches' "the root's parent bound rw" 3 -v "$d:/up"
}

# in_root WANT WHAT N ARG...: ranikust in the directory root of the binds
# case, with ARG, builds (WANT `built`) or is refused with WANT in its
# message before its state directory, the N-th, exists.
in_root() {
  local want="$1" what="$2" state="/var/lib/rnk-binds-$3" got rc=0
  shift 3
  got="$($ENGINE run --rm "$@" -v "$(out binds)/ctx:/ranikust/ctx:ro" --rootfs "$(out binds)/root" \
    /usr/local/bin/ranikust --mode=kaniko --context /ranikust/ctx --no-push \
    --state-dir "$state" 2>&1)" || rc=$?
  if [ "$want" = built ]; then
    [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got"
  else
    [ "$rc" = 1 ] && grep -q "$want" <<<"$got" && [ ! -e "$(out binds)/root$state" ]
  fi || {
    echo "$got" >&2
    fail "directory root, $what: exit $rc, want $want"
  }
}

# on_slash WANT WHAT BIN ARG...: ranikust at BIN, in a default container
# with ARG (binds), builds on `/` (WANT `built`), or is refused before its
# state directory exists, because a writable mount reaches its binary
# (`reached`) or with WANT in its message.
on_slash() {
  local want="$1" what="$2" bin="$3" got rc=0
  shift 3
  got="$($ENGINE run --rm "$@" -v "$(out binds)/ctx:/ranikust/ctx:ro" "$BASE" sh -c '
    "$0" --mode=kaniko --context /ranikust/ctx --no-push --state-dir /var/lib/rnk-binds
    rc=$?; test -e /var/lib/rnk-binds && echo STATE-MADE; exit $rc' "$bin" 2>&1)" || rc=$?
  [ "$want" = reached ] && want='a writable mount also reaches'
  if [ "$want" = built ]; then
    [ "$rc" = 0 ] && grep -q '^sha256:' <<<"$got"
  else
    [ "$rc" = 1 ] && grep -qF -- "$want" <<<"$got" && ! grep -q STATE-MADE <<<"$got"
  fi || {
    echo "$got" >&2
    fail "$what: exit $rc, want $want"
  }
}
