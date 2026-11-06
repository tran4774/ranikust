ARG BASE
# What every profile builds alike: a RUN that makes no call any of them
# refuses.
FROM ${BASE} AS same
RUN echo a > /a && mkdir -p /d/e && echo b > /d/e/f && chmod 0750 /d && ln -s /a /l

FROM ${BASE}
# Every process of the RUN's namespace, its init and processes left in the
# background or in a session of their own included: NoNewPrivs, Seccomp
# and Seccomp_filters, one line per process.
RUN sleep 60 & setsid sleep 60 & sleep 1; \
    for s in /proc/[0-9]*/status; do \
      awk '/^(NoNewPrivs|Seccomp|Seccomp_filters):/ { printf "%s%s ", $1, $2 } END { print "" }' "$s"; \
    done | grep . | sort -u > /procs; \
    ls -d /proc/[0-9]* | wc -l > /nprocs
# A setuid-root program run by another user: its ids, capabilities and
# filters.
RUN mkdir /s && cp /usr/bin/iconv /s/iconv && chmod 4755 /s/iconv && adduser -D -H u
USER u
RUN /s/iconv -f UTF-8 -t UTF-8 /proc/self/status | \
    grep -E '^(Uid|CapPrm|CapEff|CapBnd|NoNewPrivs|Seccomp|Seccomp_filters):' | \
    tr -s '\t' ' ' | tr '\n' ';' > /tmp/suid
USER root
