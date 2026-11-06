ARG BASE
# What every profile builds alike: a RUN that makes no call any of them
# refuses.
FROM ${BASE} AS same
RUN echo a > /a && mkdir -p /d/e && echo b > /d/e/f && chmod 0750 /d && ln -s /a /l

FROM ${BASE}
# Every process of the RUN's namespace, its init and processes left in the
# background or in a session of their own included: pid, session,
# NoNewPrivs, Seccomp and Seccomp_filters, one line per process; and the
# pids of the shell and the two it leaves running, waited for at most 10 s.
RUN echo $$ > /sh; \
    sh -c 'echo $$ > /bg; exec sleep 60' & \
    setsid sh -c 'echo $$ > /sid; exec sleep 60' & \
    i=0; \
    while [ "$i" -lt 100 ] && ! { [ -s /bg ] && [ -s /sid ]; }; do sleep 0.1; i=$((i + 1)); done; \
    for s in /proc/[0-9]*/status; do \
      awk '/^Pid:/ { p = $2 } /^NSsid:/ { i = $2 } /^(NoNewPrivs|Seccomp|Seccomp_filters):/ { f = f $1 $2 " " } END { if (p != "") print p, i, f }' "$s"; \
    done | sort -n > /procs
# A setuid-root program run by another user: its ids, capabilities and
# filters.
RUN mkdir /s && cp /usr/bin/iconv /s/iconv && chmod 4755 /s/iconv && adduser -D -H u
USER u
RUN /s/iconv -f UTF-8 -t UTF-8 /proc/self/status | \
    grep -E '^(Uid|CapPrm|CapEff|CapBnd|NoNewPrivs|Seccomp|Seccomp_filters):' | \
    tr -s '\t' ' ' | tr '\n' ';' > /tmp/suid
USER root
