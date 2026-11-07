# Red team of rootless overlay mode (#79): what a RUN writes and reaches
# (`writes`), what it leaves running (`left`), an exit status it chooses
# (`exit75`), its user namespace (`userns`), a tree deeper than PATH_MAX
# in its layer (`deep`) or a cache mount (`deep-cache`, the VM's), and what
# a kaniko-mode RUN on / plants in the state directory (`plant`).
ARG BASE
FROM ${BASE} AS writes
# One line per attempt: a kernel, cgroup or sysfs file written, a cgroup
# made, /proc mounted over; each process the RUN sees, and the root or
# environment of one outside its PID namespace (fewer NSpid than its own)
# read; then the controls: one of its own reached so, a file written.
RUN w() { if (printf 1 >"$1") 2>/dev/null; then echo "write $1=done"; else echo "write $1=refused"; fi; }; \
    { for f in /proc/sys/kernel/core_pattern /proc/sys/kernel/modprobe /proc/sysrq-trigger \
        /proc/sys/vm/drop_caches /sys/kernel/uevent_helper /sys/fs/cgroup/cgroup.procs \
        /sys/fs/cgroup/cgroup.subtree_control /sys/fs/cgroup/cgroup.max.descendants \
        /sys/class/net/lo/mtu; do w "$f"; done; \
      if mkdir /sys/fs/cgroup/rnk-escape 2>/dev/null; then echo "mkdir cgroup=done"; else echo "mkdir cgroup=refused"; fi; \
      if mount -t tmpfs none /proc 2>/dev/null; then echo "mount proc=done"; else echo "mount proc=refused"; fi; \
      echo "cgroup $(cat /proc/self/cgroup)"; \
      set -- $(grep '^NSpid:' /proc/self/status); own=$#; \
      for p in /proc/[0-9]*; do \
        set -- $(grep '^NSpid:' "$p/status" 2>/dev/null); [ $# -gt 1 ] || continue; \
        r="${p#/proc/} $(cat "$p/comm" 2>/dev/null)"; echo "seen: $r"; \
        if [ $# -ge "$own" ]; then ls "$p/root/" >/dev/null 2>&1 && cat "$p/environ" >/dev/null 2>&1 && echo "control reach=done $r"; continue; fi; \
        ls "$p/root/" >/dev/null 2>&1 && echo "reached $r root"; \
        cat "$p/environ" >/dev/null 2>&1 && echo "reached $r environ"; \
      done; \
      w /tmp/control; } >/report 2>&1; cat /report
RUN --network=none { if (printf 1 >/sys/class/net/lo/mtu) 2>/dev/null; then echo "write none /sys/class/net/lo/mtu=done"; else echo "write none /sys/class/net/lo/mtu=refused"; fi; } >>/report

FROM ${BASE} AS left
# Processes a RUN leaves (a new session, a double fork, nohup); one appends
# to a cache mount, which must stop growing once the RUN ends.
RUN --mount=type=cache,target=/c,id=rootless-reach-beat \
    rm -f /c/beat && \
    setsid sh -c 'while :; do echo x >>/c/beat; sleep 0.02; done' </dev/null >/dev/null 2>&1 & \
    (sleep 301 </dev/null >/dev/null 2>&1 &) ; \
    nohup sleep 302 </dev/null >/dev/null 2>&1 & \
    sleep 0.3
RUN --mount=type=cache,target=/c,id=rootless-reach-beat \
    a="$(wc -l </c/beat)" && sleep 0.5 && b="$(wc -l </c/beat)" && echo "beat $a $b" | tee /beat && \
    [ "$a" -gt 0 ] && [ "$a" = "$b" ]

FROM ${BASE} AS exit75
RUN exit 75

FROM ${BASE} AS userns
RUN { cat /proc/self/uid_map; readlink /proc/self/ns/user; } >/userns

FROM ${BASE} AS deep-sh
COPY deep.sh /deep.sh

FROM deep-sh AS deep
RUN mkdir /r && sh /deep.sh /r

FROM deep-sh AS deep-cache
RUN --mount=type=cache,target=/c,id=rootless-reach-deep sh /deep.sh "$(mktemp -d /c/t.XXXXXX)"

FROM ${BASE} AS plant
# A probe directory planted in the state directory with links out to the
# outputs and to /, the build's own marker deleted; then a RUN to kill.
RUN p=/var/lib/ranikust/.ranikust-anon-userns-probe-1-1 && mkdir -p "$p/x" && \
    ln -s /ranikust/out "$p/x/out" && ln -s /ranikust/out "$p/out" && ln -s / "$p/root" && \
    echo planted >"$p/f" && rm -f /var/lib/ranikust/.ranikust-building && sleep 120
