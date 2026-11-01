# Red team of per-stage directories (ADR-0059; T1, T3, T15, T17, T18).
# `a` and `b` are independent and run at once (--jobs 4): each looks for
# the other's marker through /proc, its own tree and pid 1's root, tries a
# mount, a sysctl and the raw disks through mknod; probe.sh records what
# got through. `hostile` leaves links out of its root, a FIFO, device
# nodes, a setuid file with a hardlink, a 15 TiB sparse file and a name
# with a newline; `child` is built FROM it and records what it got.
# `daemon` leaves a setsid writer behind; `heir`, built FROM it, records
# its file growing (the writer outlived its RUN).
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS a
COPY probe.sh /probe.sh
RUN sh /probe.sh a b

FROM ${BASE} AS b
COPY probe.sh /probe.sh
RUN sh /probe.sh b a

FROM ${BASE} AS hostile
RUN ln -s / /etc-root && ln -s ../../../../../.. /up && ln -s /proc/self/root /pr \
 && mkfifo /fifo && cp /bin/busybox /su && chmod 4755 /su && ln /su /su2 \
 && { mknod /left-chr c 1 3 2>/dev/null || true; } \
 && { tail -n +3 /proc/partitions | { read -r maj min _ && mknod /left-blk b "$maj" "$min"; } 2>/dev/null || true; } \
 && dd if=/dev/null of=/sparse bs=1 seek=16492674416640 count=0 2>/dev/null \
 && printf mid | dd of=/sparse bs=1 seek=1099511627776 conv=notrunc 2>/dev/null \
 && mkdir "/$(printf 'n\nl')" && echo nl > "/$(printf 'n\nl')/f" \
 && { ls -l /left-chr /left-blk /fifo >&2 || true; }

FROM ${BASE} AS daemon
RUN setsid sh -c 'while :; do echo x >>/grow; sleep 0.01; done' </dev/null >/dev/null 2>&1 & sleep 0.3

FROM daemon AS heir
RUN a="$(wc -c </grow)"; sleep 1; b="$(wc -c </grow)"; : >/heir-report; \
    [ "$a" = "$b" ] || echo "grow: $a -> $b bytes" >>/heir-report; \
    ! pgrep -f 'echo [x]' >/dev/null || echo "the writer runs in the heir" >>/heir-report

FROM hostile AS child
COPY child.sh /child.sh
RUN sh /child.sh > /child-report

FROM ${BASE}
COPY --from=a /report /report-a
COPY --from=b /report /report-b
COPY --from=heir /heir-report /heir-report
COPY --from=child /child-report /child-report
CMD ["cat", "/report-a", "/report-b", "/heir-report", "/child-report"]
