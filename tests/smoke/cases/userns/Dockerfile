# --userns as root (#83, ADR-0104): ownership by a mapped user, a setgid
# directory, a setuid file and a symlink (`content`, built with and without
# --userns); RUNs that wait while the case looks at them from the host
# (`seen`), as root, then as USER 1234:1234, each until the case removes
# the file it made.
ARG BASE
FROM ${BASE} AS content
RUN echo 'u:x:1234:1234::/home/u:/bin/sh' >>/etc/passwd && echo 'u:x:1234:' >>/etc/group \
 && mkdir -p /home/u /d && chown 1234:1234 /home/u && chmod 2750 /d && echo root >/d/r \
 && su u -c 'echo user >/home/u/f && ln -s f /home/u/l' \
 && cp /bin/busybox /bin/suid && chmod 4755 /bin/suid && rm /etc/motd

FROM content AS seen
RUN : rnk-userns-wait-root; touch /made-by-run /tmp/wait-root && i=0 \
 && while [ -e /tmp/wait-root ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i + 1)); done \
 && [ ! -e /tmp/wait-root ] && rm /made-by-run
USER 1234:1234
RUN : rnk-userns-wait-user; touch /tmp/wait-user && i=0 \
 && while [ -e /tmp/wait-user ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i + 1)); done \
 && [ ! -e /tmp/wait-user ]
