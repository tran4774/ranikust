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

# The `ranikust` binary's process set-up: with `--no-push --workdir`, it
# must reach the outcome, overlay mode as root entering roots with $2
# (`pivot_root` or `MS_MOVE`) after a real overlay probe (either mount
# API), and stop before building with exit code 2. $1 is its stderr, $3 its
# exit code.
smoke_check_setup() {
  if [ "$3" != 2 ] ||
    ! grep -Eq "set up for overlay mode as root, RUN enters its root with $2, overlay mounts with (fsconfig\(lowerdir\+\)|mount\(2\))" <<<"$1"; then
    echo "error: ranikust's set-up: expected $2 and exit code 2, got $3:" >&2
    echo "$1" >&2
    return 1
  fi
}
