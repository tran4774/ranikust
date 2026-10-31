# Shared by `just smoke` (T1, rootless podman) and the VM smoke suite (T2,
# tests/vm/guest/smoke.sh). Sourced, not run.

# Checked inside the sandbox, one `RUN` each so that a failure names it.
# Mount flags come from mountinfo: busybox `test -w` is always true for root.
smoke_checks=(
  'test "$$" = 1'                                                    # PID 1 of its namespace
  'test "$(hostname)" = ranikust'
  'grep -q "^CapBnd:[[:space:]]*00000000a80425fb$" /proc/self/status' # Docker's 14 capabilities
  'grep -q "^CapInh:[[:space:]]*0*$" /proc/self/status'
  'grep -Eq "^([^ ]+ ){4}/ [^ ]*nodev" /proc/self/mountinfo'          # T18
  'grep -Eq "^([^ ]+ ){4}/dev/null ro," /proc/self/mountinfo'         # T18
  'grep -Eq "^([^ ]+ ){4}/proc/sys ro," /proc/self/mountinfo'         # T17
  '! test -s /proc/kcore'                                            # masked
  '! test -t 0'                                                      # T19
  'test "$(ls /proc/self/fd | wc -l)" -le 4'                          # stdio and ls's own, T7
)

# The sandbox's stubs must not reach a layer of the OCI layout in $1:
# /etc/resolv.conf, which alpine lacks, not at all; /etc/hosts and
# /etc/hostname, which alpine has, not with the host name `ranikust` the
# stubs carry.
smoke_check_no_stub_leak() {
  local blob f
  for blob in "$1"/blobs/sha256/*; do
    gzip -t "$blob" 2>/dev/null || continue
    if tar -tzf "$blob" | grep -Eq '^(\./)?etc/resolv\.conf$'; then
      echo "error: a stub leaked into the layer: etc/resolv.conf" >&2
      return 1
    fi
    for f in etc/hosts etc/hostname; do
      if tar -xzOf "$blob" --wildcards "$f" "./$f" 2>/dev/null | grep -qw ranikust; then
        echo "error: a stub leaked into the layer: $f" >&2
        return 1
      fi
    done
  done
}

# The end-to-end build of tests/smoke/e2e with the `ranikust` binary.
# Sets `e2e_args` for a context $1, state directory $2, output directory $3 (an OCI
# layout `oci` and a Docker tar `image.tar` go there) and secret file $4,
# paths as the binary sees them.
smoke_e2e_args() {
  e2e_args=(--context "$1" --no-push --state-dir "$2" --oci-layout-path "$3/oci"
    --tar-path "$3/image.tar" --destination localhost/rnk-e2e:latest
    --secret "id=token,src=$4")
}

# The secret the e2e Dockerfile reads through `RUN --mount=type=secret`.
SMOKE_E2E_SECRET=s3cr3t

# What the e2e image prints when run, and as whom it runs.
SMOKE_E2E_OUTPUT=$'hello from ranikust\napp.txt from the build context'
SMOKE_E2E_USER=app

# The build's log $1 and exit code $3: the set-up reached overlay mode as
# root entering roots with $2 (`pivot_root` or `MS_MOVE`) after a real
# overlay probe (either mount API), and the build printed a digest.
smoke_check_build() {
  if [ "$3" != 0 ] ||
    ! grep -Eq "set up for overlay mode as root, RUN enters its root with $2, overlay mounts with (fsconfig\(lowerdir\+\)|mount\(2\))" <<<"$1" ||
    ! grep -Eq '^sha256:[0-9a-f]{64}$' <<<"$1"; then
    echo "error: ranikust build: expected $2, a digest and exit code 0, got $3:" >&2
    echo "$1" >&2
    return 1
  fi
}

# The digest a build printed, from its log $1.
smoke_digest() {
  grep -Eo '^sha256:[0-9a-f]{64}$' <<<"$1" | tail -1
}

# The OCI layout $1 must hold an empty layer: the e2e Dockerfile's `RUN
# true`, which changes nothing, still gets one.
smoke_check_empty_layer() {
  local blob
  for blob in "$1"/blobs/sha256/*; do
    if gzip -t "$blob" 2>/dev/null && [ -z "$(tar -tzf "$blob")" ]; then
      return 0
    fi
  done
  echo "error: no empty layer in $1" >&2
  return 1
}

# The secret $2 must not appear in any blob of the OCI layout $1: layers
# (decompressed), config and manifests.
smoke_check_no_secret() {
  local blob
  for blob in "$1"/blobs/sha256/*; do
    if { gzip -t "$blob" 2>/dev/null && gzip -dc "$blob"; } | grep -aq "$2" ||
      grep -aq "$2" "$blob"; then
      echo "error: the secret leaked into $blob" >&2
      return 1
    fi
  done
}

# The secret $1 must not appear in the files after it: build logs, the
# metrics report.
smoke_check_no_secret_in() {
  local secret="$1" f
  shift
  for f in "$@"; do
    if grep -aq -- "$secret" "$f"; then
      echo "error: the secret leaked into $f" >&2
      return 1
    fi
  done
}
