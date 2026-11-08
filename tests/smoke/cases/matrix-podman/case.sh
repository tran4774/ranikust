# Row 5 of ADR-0100's privilege matrix (#84), Podman only (T1, both legs):
# rootless Podman with its default options and a volume at the state
# directory. The container is checked against the row (root there another
# uid outside, with a range of ids; a seccomp filter; no CAP_SYS_ADMIN;
# /proc/sys read-only; the state directory a mount of its own), from what
# it prints before ranikust starts. A build without --mode, with
# --allow proc.rbind, must run in
# rootless overlay mode with a read-only rbind of /proc: one user namespace
# probe line, passing, each step up to proc as expected (proc's own mount
# refused with EPERM), a RUN that sees the container's PID namespace
# through the rbind, and the owner it set in the image. What the row does
# not decide (sysfs, mknod, the overlay API, AppArmor's switch, the
# container's AppArmor label) is printed as `measured:`. CI runs it on a
# GitHub-hosted runner (tests/vm/runner.sh matrix-podman).

# The script run in the container: what it got, then ranikust ($0, with its
# arguments).
env_script() {
  cat <<'EOF'
st() { awk -v k="$1:" '$1 == k { print $2 }' /proc/self/status; }
echo "env: uid $(id -u)"
echo "env: root outside $(awk '$1 == 0 { print $2 }' /proc/self/uid_map)"
echo "env: uid_map $(awk '{ printf "%s %s %s;", $1, $2, $3 }' /proc/self/uid_map)"
echo "env: ids mapped $(awk '{ n += $3 } END { print n }' /proc/self/uid_map)"
echo "env: seccomp $(st Seccomp)"
echo "env: CapEff $(st CapEff)"
echo "env: apparmor $(tr -d '\000' </proc/self/attr/current 2>/dev/null || echo none)"
echo "env: /proc/sys $(awk '$5 == "/proc/sys" { o = $6 } END { print o ? o : "none" }' /proc/self/mountinfo)"
echo "env: state directory $(awk '$5 == "/var/lib/ranikust" { for (i = 7; i < NF; i++) if ($i == "-") t = $(i + 1) } END { print t ? t : "none" }' /proc/self/mountinfo)"
exec "$0" "$@"
EOF
}

# env_line KEY: what the container printed for KEY.
env_line() { sed -n "s|^env: $1 ||p" "$(out row-5)/build.log" | head -1; }

case_run() {
  [ "$ENGINE" = podman ] || return 0
  # Without --mode: --mode=auto decides. The kaniko leg keeps its paths.
  local m=() a
  for a in ${MODE_ARGS[@]+"${MODE_ARGS[@]}"}; do [ "$a" = --mode=kaniko ] || m+=("$a"); done
  MODE_ARGS=(${m[@]+"${m[@]}"})
  PODMAN_ARGS=()
  # Kept for the next run: files of mapped ids this user cannot delete.
  WORK="$CASES_OUT/.matrix-podman"
  mkdir -p "$WORK" && chmod 700 "$WORK"
  local o log rc=0
  o="$(out row-5)"
  rm -rf "$o" && mkdir -p "$o"
  log="$(rnk_run_sh "$(env_script)" "$SMOKE_CASES_DIR/$CASE" "$o" --build-arg "BASE=$BASE" \
    --destination "localhost/rnk-case:$CASE-row-5" --allow proc.rbind 2>&1)" || rc=$?
  printf '%s\n' "$log" >"$o/build.log"
  grep '^env: ' <<<"$log" | sed "s/^/  $CASE: /" >&2 || true

  expect "$(env_line uid)" 0 "row 5: root in the container"
  [[ "$(env_line 'root outside')" =~ ^[1-9][0-9]*$ ]] ||
    fail "row 5: root in the container is root outside it, not rootless Podman: $(env_line uid_map)"
  [ "$(env_line 'ids mapped')" -gt 1 ] 2>/dev/null ||
    fail "row 5: a range of ids mapped, got $(env_line uid_map)"
  expect "$(env_line seccomp)" 2 "row 5: Podman's seccomp filter"
  local cap
  cap="$(env_line CapEff)"
  [[ "$cap" =~ ^[0-9a-f]{16}$ ]] && (((16#$cap >> 21 & 1) == 0)) ||
    fail "row 5: root in the container holds CAP_SYS_ADMIN, or no CapEff: '$cap'"
  [[ "$(env_line /proc/sys)" == ro,* ]] ||
    fail "row 5: /proc/sys is not a read-only mount: $(env_line /proc/sys)"
  case "$(env_line 'state directory')" in
    none | overlay) fail "row 5: the state directory is not a volume: $(env_line 'state directory')" ;;
  esac

  [ "$rc" = 0 ] || { echo "$log" >&2; fail "row 5: the build exited $rc"; }
  smoke_check_no_stub_leak "$o/oci"
  smoke_check_no_secret "$o/oci" "$SMOKE_CASE_SECRET"
  smoke_check_no_secret_in "$SMOKE_CASE_SECRET" "$o/build.log"
  local n line
  n="$(grep -Ec 'user namespace probe( \(reported only\))?: ' <<<"$log" || true)"
  expect "$n" 1 "row 5: user namespace probe lines"
  line="$(grep -Eo 'user namespace probe( \(reported only\))?: .*' <<<"$log")"
  echo "measured: row 5: $line" >&2
  [[ "$line" == "user namespace probe: passes; unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok; unshare(CLONE_NEWNS): ok; proc: rbind of /proc (mount proc: EPERM); sysfs: "* &&
    "$line" == *"; overlay: ok, "* ]] ||
    fail "row 5: the probe should pass with an rbind of /proc, a new one refused with EPERM: $line"
  grep -q "ranikust: overlay mode, rootless, in the build's user namespace, RUN enters its root with pivot_root, proc: a read-only rbind, " <<<"$log" ||
    { echo "$log" >&2; fail "row 5: not rootless overlay mode with a read-only rbind of /proc"; }
  # Through the rbind, PID 1 is the container's and the RUN sees its own
  # PID namespace below the container's.
  local seen re='^pid 1 (.+), NSpid levels ([0-9]+)$'
  seen="$(sed -n 's/^proc: //p' <<<"$log" | head -1)"
  [[ "$seen" =~ $re ]] && [ "${BASH_REMATCH[1]}" != ranikust-init ] && [ "${BASH_REMATCH[2]}" -ge 2 ] ||
    fail "row 5: the RUN's proc is not the container's: '$seen'"
  echo "measured: row 5: the RUN's proc: $seen; the container's AppArmor label: $(env_line apparmor)" >&2
  expect "$(run row-5 stat -c '%u:%g' /d)" 1234:1234 "row 5: the owner a RUN set"
}
