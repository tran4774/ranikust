# Red team of --userns as root (#83, ADR-0104, ADR-0139): what a RUN in the
# build's user namespace writes and reaches (`writes`), what it leaves
# running (`left`), and a RUN that waits while the case looks at the
# build's own processes from the host, or kills them (`wait`).
ARG BASE
FROM ${BASE} AS writes
# The descriptors a RUN gets (ls's own: 0 to 2 and the one it lists
# with); one line per attempt: a kernel, cgroup or sysfs file written, a
# cgroup made, /proc mounted over; each process the RUN sees, and the root
# or environment of one outside its PID namespace read; then the controls.
RUN ls /proc/self/fd >/fds && ls -l /proc/self/fd >/fds-l 2>&1; \
    w() { if (printf 1 >"$1") 2>/dev/null; then echo "write $1=done"; else echo "write $1=refused"; fi; }; \
    { echo "fds $(tr '\n' ' ' </fds)"; \
      for f in /proc/sys/kernel/core_pattern /proc/sys/kernel/modprobe /proc/sysrq-trigger \
        /proc/sys/vm/drop_caches /proc/sys/fs/protected_hardlinks /sys/kernel/uevent_helper \
        /sys/fs/cgroup/cgroup.procs /sys/fs/cgroup/cgroup.subtree_control /sys/class/net/lo/mtu; do w "$f"; done; \
      if mkdir /sys/fs/cgroup/rnk-escape 2>/dev/null; then echo "mkdir cgroup=done"; else echo "mkdir cgroup=refused"; fi; \
      if mount -t tmpfs none /proc 2>/dev/null; then echo "mount proc=done"; else echo "mount proc=refused"; fi; \
      echo "cgroup $(cat /proc/self/cgroup)"; \
      echo "uid_map $(cat /proc/self/uid_map | tr -s ' ' | tr '\n' ' ')"; \
      set -- $(grep '^NSpid:' /proc/self/status); own=$#; \
      for p in /proc/[0-9]*; do \
        set -- $(grep '^NSpid:' "$p/status" 2>/dev/null); [ $# -gt 1 ] || continue; \
        r="${p#/proc/} $(cat "$p/comm" 2>/dev/null)"; echo "seen: $r"; \
        if [ $# -ge "$own" ]; then ls "$p/root/" >/dev/null 2>&1 && cat "$p/environ" >/dev/null 2>&1 && echo "control reach=done $r"; continue; fi; \
        ls "$p/root/" >/dev/null 2>&1 && echo "reached $r root"; \
        cat "$p/environ" >/dev/null 2>&1 && echo "reached $r environ"; \
      done; \
      w /tmp/control; } >/report 2>&1; cat /report /fds-l

FROM ${BASE} AS left
# Processes a RUN leaves (a new session, a double fork, nohup); one appends
# to a cache mount, which must stop growing once the RUN ends.
RUN --mount=type=cache,target=/c,id=userns-reach-beat \
    rm -f /c/beat && \
    setsid sh -c 'while :; do echo x >>/c/beat; sleep 0.02; done' </dev/null >/dev/null 2>&1 & \
    (sleep 311 </dev/null >/dev/null 2>&1 &) ; \
    nohup sleep 312 </dev/null >/dev/null 2>&1 & \
    sleep 0.3
RUN --mount=type=cache,target=/c,id=userns-reach-beat \
    a="$(wc -l </c/beat)" && sleep 0.5 && b="$(wc -l </c/beat)" && echo "beat $a $b" | tee /beat && \
    [ "$a" -gt 0 ] && [ "$a" = "$b" ]

FROM ${BASE} AS wait
RUN : rnk-userns-reach-wait; touch /tmp/wait-reach && i=0 \
 && while [ -e /tmp/wait-reach ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i + 1)); done \
 && [ ! -e /tmp/wait-reach ]
