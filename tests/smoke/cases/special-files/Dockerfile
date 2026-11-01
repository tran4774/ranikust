# Red team of COPY and special files (ADR-0060; T18). The context case.sh
# builds holds FIFOs and Unix sockets at the top, nested and behind a
# symlinked directory; the stage `src` leaves FIFOs and, where root may
# make one, a device node. With RUN entered without nodev (kaniko mode on
# a container's /, Isolation::Direct) COPY leaves them all out, with one
# warning per copy; with the full sandbox they are kept, as BuildKit, and
# never open. A RUN's own mknod never opens in a later RUN where the root
# is entered nodev.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS src
RUN mkdir -p /s/d && mkfifo /s-fifo /s/d/fifo && echo k > /s/keep \
 && { mknod /s/chr c 1 3 2>/dev/null || true; } && { mknod /s-chr c 1 3 2>/dev/null || mkfifo /s-chr; }

FROM ${BASE}
COPY . /ctx/
COPY fifo /lone-fifo
COPY sock /lone-sock
COPY via/ /via/
COPY d/e/fifo d/e/sock /several/
COPY --from=src /s /from-stage/
COPY --from=src /s-fifo /lone-stage-fifo
COPY --from=src /s-chr /lone-stage-chr
RUN if mknod /made c 1 3 2>/dev/null; then echo "mknod yes"; else echo "mknod no"; fi > /mknod.txt
RUN if [ -c /made ]; then if (echo x > /made) 2>/dev/null; then echo "open yes"; else echo "open no"; fi; \
    else echo "open none"; fi > /open.txt
COPY check.sh /check.sh
CMD ["sh", "/check.sh"]
