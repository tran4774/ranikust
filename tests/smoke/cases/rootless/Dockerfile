# Rootless overlay mode end to end (#79): ownership by a mapped user, a
# setgid directory, a symlink and a whiteout (`content`); a file capability
# set by a RUN (`caps`); a RUN to kill ranikust during (`slow`).
ARG BASE
FROM ${BASE} AS content
RUN echo 'u:x:1234:1234::/home/u:/bin/sh' >>/etc/passwd && echo 'u:x:1234:' >>/etc/group \
 && mkdir -p /home/u /d && chown 1234:1234 /home/u && chmod 2750 /d && echo root >/d/r \
 && su u -c 'echo user >/home/u/f && ln -s f /home/u/l' && rm /etc/motd

FROM content AS caps
RUN apk add --no-cache -q libcap-utils && cp /bin/busybox /bin/pinger \
 && setcap cap_net_raw+ep /bin/pinger

FROM content AS slow
RUN sleep 60
