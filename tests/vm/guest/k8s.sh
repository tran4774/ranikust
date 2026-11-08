#!/usr/bin/env bash
# Runs in the T2 VM, and as root on a GitHub-hosted runner
# (tests/vm/runner.sh, ADR-0090): the Kubernetes rows of the privilege matrix
# (ADR-0100 rows 7a-7d, #86) as pods of a one-node k3s cluster, the release
# of guest/k8s/pin.env, run as the systemd unit rnk-k3s and stopped at the
# end. Each pod runs the ranikust binary with --mode=auto after
# guest/k8s/pod.sh prints its environment, which must be its row's; the
# suite asserts the mode or the refusal with step and errno, and prints as
# `measured:` what it does not assert. The rows, their outcomes and why 7b
# is not ADR-0100's expectation: docs/testing.md#privilege-matrix-in-kubernetes.
#
# Writes out/k8s/: env.txt (the versions), <pod>.log, <pod>.describe.txt,
# and, when the suite fails, events.txt and k3s.log (k3s's journal).
set -euo pipefail
# `pod_yaml ... | run_pod ...`: run_pod sets LOG and RC in this shell.
shopt -s lastpipe
cd "$HOME/rnk"
# shellcheck source=k8s/pin.env
source guest/k8s/pin.env

OUT="$HOME/rnk/out/k8s"
IMAGE=localhost/rnk-k8s:1
NS=rnk-k8s
NS_RESTRICTED=rnk-k8s-restricted
K3S=/usr/local/bin/k3s
mkdir -p "$OUT"

kc() { sudo "$K3S" kubectl "$@"; }
FAILED=()
fail() { echo "FAIL: $*" >&2; FAILED+=("$*"); }

restrict_was=
teardown() {
  local rc=$?
  set +e
  [ -z "$restrict_was" ] || sudo sysctl -q -w "kernel.apparmor_restrict_unprivileged_userns=$restrict_was"
  if systemctl is-active -q rnk-k3s 2>/dev/null; then
    if [ "$rc" != 0 ] || [ "${#FAILED[@]}" != 0 ]; then
      kc get events -A --sort-by=.lastTimestamp >"$OUT/events.txt" 2>&1
      sudo journalctl -u rnk-k3s --no-pager >"$OUT/k3s.log" 2>&1
    fi
    kc delete namespace "$NS" "$NS_RESTRICTED" --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1
    sudo systemctl stop rnk-k3s
  fi
}

# --- k3s ---------------------------------------------------------------------

[ "$(sha256sum bin/k3s | cut -d' ' -f1)" = "$K3S_SHA256" ] ||
  { echo "error: bin/k3s is not k3s $K3S_VERSION ($K3S_SHA256)" >&2; exit 1; }
! systemctl is-active -q rnk-k3s 2>/dev/null || { echo "error: rnk-k3s is already running" >&2; exit 1; }
# Only now: the teardown stops rnk-k3s, which until here is not this run's.
trap teardown EXIT
sudo install -m 0755 bin/k3s "$K3S"
echo "== k3s $K3S_VERSION"
sudo systemd-run --unit=rnk-k3s --collect -p Delegate=yes -p LimitNOFILE=1048576 -p TasksMax=infinity \
  "$K3S" server --node-name=rnk-k3s --write-kubeconfig-mode=0600 \
  --disable=traefik,servicelb,metrics-server,local-storage,coredns --disable-helm-controller \
  --pause-image="$K3S_PAUSE_IMAGE"
deadline=$((SECONDS + 300))
until kc get node rnk-k3s >/dev/null 2>&1; do
  [ "$SECONDS" -lt "$deadline" ] || { fail "k3s: no node after 300 s"; sudo journalctl -u rnk-k3s --no-pager >"$OUT/k3s.log" 2>&1; exit 1; }
  sleep 2
done
kc wait --for=condition=Ready node/rnk-k3s --timeout=240s >/dev/null ||
  { fail "k3s: the node is not Ready"; sudo journalctl -u rnk-k3s --no-pager >"$OUT/k3s.log" 2>&1; exit 1; }
{
  echo "kernel: $(uname -r)"
  echo "k3s: $("$K3S" --version | head -1)"
  echo "kubernetes: $(kc version | sed -n 's/^Server Version: //p')"
  echo "containerd: $(sudo "$K3S" ctr version | awk '/^Server:/ { s = 1 } s && /Version:/ { print $2; exit }')"
  echo "runc: $(sudo /var/lib/rancher/k3s/data/current/bin/runc --version 2>&1 | head -1)"
  echo "apparmor_restrict_unprivileged_userns: $(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo n/a)"
  echo "pause image: $K3S_PAUSE_IMAGE"
} | tee "$OUT/env.txt"

kc create namespace "$NS" >/dev/null
kc create namespace "$NS_RESTRICTED" >/dev/null
kc label namespace "$NS_RESTRICTED" pod-security.kubernetes.io/enforce=restricted \
  pod-security.kubernetes.io/enforce-version=latest >/dev/null
# A pod waits for its namespace's default service account.
for ns in "$NS" "$NS_RESTRICTED"; do
  deadline=$((SECONDS + 60))
  until kc get serviceaccount default -n "$ns" >/dev/null 2>&1; do
    [ "$SECONDS" -lt "$deadline" ] || { fail "k3s: no default service account in $ns"; exit 1; }
    sleep 1
  done
done

# --- The pods' image -----------------------------------------------------------

# Static: the image has no libc.
! ldd /bin/busybox >/dev/null 2>&1 || { echo "error: /bin/busybox is not static" >&2; exit 1; }
root="$HOME/rnk/k8s-image"
rm -rf "$root"
mkdir -p "$root/bin" "$root/ranikust/bin" "$root/ctx" "$root/tmp" "$root/var/lib" "$root/work"
cp /bin/busybox "$root/bin/busybox"
cp bin/ranikust "$root/ranikust/bin/ranikust"
cp guest/k8s/pod.sh "$root/pod.sh"
cp /bin/busybox "$root/ctx/busybox"
printf 'FROM scratch\nCOPY busybox /bin/busybox\nRUN ["/bin/busybox", "true"]\n' >"$root/ctx/Dockerfile"
# Whatever this user's umask: ranikust refuses a state directory below a
# directory others may write (/var/lib here, ADR-0050).
chmod -R u=rwX,go=rX "$root"
chmod 1777 "$root/tmp"
tar -C "$root" --owner=0 --group=0 --numeric-owner -cf - . | sudo docker import - "$IMAGE" >/dev/null
sudo docker save "$IMAGE" | sudo "$K3S" ctr -n k8s.io images import - >/dev/null
sudo docker rmi "$IMAGE" >/dev/null
rm -rf "$root"

# --- Pods ----------------------------------------------------------------------

# pod_yaml <name> <namespace> <pod securityContext and hostUsers, YAML at 2>
#   <container securityContext, YAML at 6> <env, YAML at 6> <volumes>
#   <ranikust arguments as a JSON list>: the manifest of one row's pod.
# <volumes> is "work" (the context's volume) or "work state" (and one at
# the state directory, /var/lib/ranikust).
pod_yaml() {
  local name=$1 ns=$2 podsec=$3 ctrsec=$4 env=$5 vols=$6 args=$7
  cat <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  namespace: $ns
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  enableServiceLinks: false
  terminationGracePeriodSeconds: 5
$podsec
  containers:
    - name: ranikust
      image: $IMAGE
      imagePullPolicy: Never
      command: ["/bin/busybox", "sh", "/pod.sh"]
      args: $args
$ctrsec
$env
      volumeMounts:
        - { name: work, mountPath: /work }
EOF
  if [ "$vols" = "work state" ]; then
    echo "        - { name: state, mountPath: /var/lib/ranikust }"
  fi
  echo "  volumes:"
  echo "    - { name: work, emptyDir: {} }"
  if [ "$vols" = "work state" ]; then
    echo "    - { name: state, emptyDir: {} }"
  fi
}

# run_pod <name> <namespace>: creates the pod of the manifest on stdin,
# waits for it to end, and sets LOG (its log) and RC (its exit code, or
# "none" when it never ran: the reason is in out/k8s/<name>.describe.txt).
run_pod() {
  local name=$1 ns=$2 phase waiting deadline=$((SECONDS + 240))
  LOG='' RC=none
  kc apply -f - >/dev/null || return 0
  while :; do
    phase="$(kc get pod -n "$ns" "$name" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
    case "$phase" in Succeeded | Failed) break ;; esac
    waiting="$(kc get pod -n "$ns" "$name" -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)"
    case "$waiting" in
      ErrImageNeverPull | CreateContainerConfigError | CreateContainerError | RunContainerError | InvalidImageName) break ;;
    esac
    [ "$SECONDS" -lt "$deadline" ] || break
    sleep 2
  done
  kc describe pod -n "$ns" "$name" >"$OUT/$name.describe.txt" 2>&1 || true
  case "$phase" in Succeeded | Failed) ;; *) return 0 ;; esac
  LOG="$(kc logs -n "$ns" "$name" 2>&1)" || true
  printf '%s\n' "$LOG" >"$OUT/$name.log"
  RC="$(kc get pod -n "$ns" "$name" -o jsonpath='{.status.containerStatuses[0].state.terminated.exitCode}' || true)"
}

# expect <row> <fixed string>: the row's log holds the string.
expect() { grep -qF -- "$2" <<<"$LOG" || fail "$1: no line with '$2'"; }
expect_rc() { [ "$RC" = "$2" ] || fail "$1: exit $RC, want $2"; }
# The pod line of key $2, without its prefix.
pod_line() { sed -n "s|^pod: $1 ||p" <<<"$LOG" | head -1; }
probe_line() { grep -o 'user namespace probe[^:]*: .*' <<<"$LOG" | head -1; }
show() {
  echo "--- $1: exit $RC"
  sed 's/\x1b\[[0-9;]*m//g' <<<"$LOG"
}

ROOT_ENV='      env:
        - { name: RNK_STATE, value: /var/lib/ranikust }
        - { name: RNK_MEASURE_PROC_SYS, value: "1" }'
USERNS_UNMASKED='      securityContext:
        procMount: Unmasked'
NO_ARGS='["--context", "/work/ctx", "--no-push"]'

echo "== 7a: a default pod"
pod_yaml row-7a "$NS" "" "" "" work "$NO_ARGS" | run_pod row-7a "$NS"
show 7a
[ "$(pod_line apparmor)" = "cri-containerd.apparmor.d (enforce)" ] || fail "7a: AppArmor label '$(pod_line apparmor)', not containerd's profile"
[[ "$(pod_line /proc/sys)" == "on /proc/sys ro,"* ]] || fail "7a: /proc/sys is not a read-only mount: $(pod_line /proc/sys)"
[ "$(pod_line uid_map)" = "0 0 4294967295;" ] || fail "7a: not the host's users: $(pod_line uid_map)"
# No seccomp filter: k3s's kubelet sets no default profile (seccompDefault).
[ "$(pod_line seccomp)" = "0 no_new_privs 0 CapEff 00000000a80425fb" ] || fail "7a: seccomp or capabilities: $(pod_line seccomp)"
expect_rc 7a 0
expect 7a "kaniko mode, because root may not mount here: a private mount namespace needs privileges this process does not have"
expect 7a "user namespace probe (reported only): stops at proc; unshare(CLONE_NEWUSER): ok;"
expect 7a "proc: make / a recursive slave mount: EACCES;"
expect 7a "ranikust: kaniko mode on /, RUN directly, as kaniko"
expect 7a "built image sha256:"

echo "== 7b: hostUsers: false, procMount: Unmasked, AppArmor unconfined"
pod_yaml row-7b "$NS" "  hostUsers: false" "$USERNS_UNMASKED
        appArmorProfile: { type: Unconfined }" "$ROOT_ENV" "work state" "$NO_ARGS" | run_pod row-7b "$NS"
show 7b
[ "$(pod_line apparmor)" = unconfined ] || fail "7b: AppArmor label '$(pod_line apparmor)', not unconfined"
[[ "$(pod_line /proc/sys)" == "on /proc rw,"* ]] || fail "7b: /proc/sys is not on a writable /proc: $(pod_line /proc/sys)"
[[ "$(pod_line uid_map)" =~ ^0\ [1-9][0-9]*\ 65536\;$ ]] || fail "7b: not a user namespace of the pod's own: $(pod_line uid_map)"
expect_rc 7b 1
expect 7b "ranikust: rootless overlay mode is not tried as root where /proc/sys is writable (ADR-0107)"
expect 7b "kaniko mode, because root may not mount here: a private mount namespace needs privileges this process does not have"
expect 7b "ranikust: error: --mode=auto: kaniko mode on / with a writable /proc/sys would let RUN write kernel/core_pattern"
echo "measured: 7b: $(pod_line "a write of kernel/core_pattern's own value:")"

echo "== 7c: hostUsers: false, procMount: Unmasked, the default AppArmor profile"
pod_yaml row-7c "$NS" "  hostUsers: false" "$USERNS_UNMASKED" "$ROOT_ENV" "work state" "$NO_ARGS" | run_pod row-7c "$NS"
show 7c
[ "$(pod_line apparmor)" = "cri-containerd.apparmor.d (enforce)" ] || fail "7c: AppArmor label '$(pod_line apparmor)', not containerd's profile"
[[ "$(pod_line /proc/sys)" == "on /proc rw,"* ]] || fail "7c: /proc/sys is not on a writable /proc: $(pod_line /proc/sys)"
[[ "$(pod_line uid_map)" =~ ^0\ [1-9][0-9]*\ 65536\;$ ]] || fail "7c: not a user namespace of the pod's own: $(pod_line uid_map)"
expect_rc 7c 1
expect 7c "ranikust: rootless overlay mode is not tried as root where /proc/sys is writable (ADR-0107)"
expect 7c "ranikust: error: --mode=auto: kaniko mode on / with a writable /proc/sys would let RUN write kernel/core_pattern"
echo "measured: 7c: $(pod_line "a write of kernel/core_pattern's own value:")"

echo "== 7d: the restricted Pod Security Standard"
RESTRICTED_POD='  securityContext:
    runAsNonRoot: true
    runAsUser: 1000
    runAsGroup: 1000
    seccompProfile: { type: RuntimeDefault }'
RESTRICTED_CTR='      securityContext:
        allowPrivilegeEscalation: false
        capabilities: { drop: [ALL] }'
# The control: a root pod is refused there before it is made.
if err="$(pod_yaml root-in-restricted "$NS_RESTRICTED" "" "" "" work "$NO_ARGS" | kc apply -f - 2>&1)"; then
  fail "7d: the restricted namespace admitted a root pod"
else
  grep -qF 'violates PodSecurity "restricted:latest"' <<<"$err" ||
    fail "7d: a root pod refused for another reason: $err"
fi
pod_yaml row-7d "$NS_RESTRICTED" "$RESTRICTED_POD" "$RESTRICTED_CTR" "" work \
  '["--context", "/work/ctx", "--no-push", "--state-dir", "/dev/shm/rnk-state"]' | run_pod row-7d "$NS_RESTRICTED"
show 7d
[[ "$(pod_line uid)" == "1000 gid 1000" ]] || fail "7d: not uid 1000: $(pod_line uid)"
[ "$(pod_line seccomp)" = "2 no_new_privs 1 CapEff 0000000000000000" ] || fail "7d: seccomp, no_new_privs or capabilities: $(pod_line seccomp)"
expect_rc 7d 1
expect 7d "user namespace probe: stops at unshare; unshare(CLONE_NEWUSER): EPERM"
expect 7d "ranikust: error: rootless overlay mode cannot run here: the user namespace probe stops at unshare"

# --- Measured: the user namespace probe in a pod of its own users ---------------

# Not a row: hostUsers: false and AppArmor unconfined, but a masked /proc,
# so that root's --mode=auto tries rootless overlay mode and the probe line
# says where it stops; at the node's AppArmor switch, and at 0 where it is 1.
masked() {
  pod_yaml "$1" "$NS" "  hostUsers: false" "      securityContext:
        appArmorProfile: { type: Unconfined }" "$ROOT_ENV" "work state" "$NO_ARGS" | run_pod "$1" "$NS"
  show "$1"
  echo "measured: $2: exit $RC; $(probe_line)"
}
switch="$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo n/a)"
echo "== measured: hostUsers: false, AppArmor unconfined, a masked /proc"
masked userns-masked "apparmor_restrict_unprivileged_userns=$switch"
if [ "$switch" = 1 ]; then
  restrict_was=1
  sudo sysctl -q -w kernel.apparmor_restrict_unprivileged_userns=0
  masked userns-masked-0 "apparmor_restrict_unprivileged_userns=0"
  sudo sysctl -q -w kernel.apparmor_restrict_unprivileged_userns=1
  restrict_was=
fi

if [ "${#FAILED[@]}" -gt 0 ]; then
  echo "k8s: ${#FAILED[@]} failure(s):"
  printf '  %s\n' "${FAILED[@]}"
  exit 1
fi
echo "k8s: rows 7a, 7b, 7c and 7d as expected"
