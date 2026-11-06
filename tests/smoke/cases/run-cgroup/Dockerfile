# What a RUN reads of its cgroups goes into a layer: two stages built at
# once, the first after one more RUN, so their RUNs start in another order
# and with other numbers at --jobs 1 and 4.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS s1
# A root RUN holds no CAP_SYS_ADMIN (bit 21) in any set, cgroups or not,
# whatever the container's own set (kaniko mode runs it on its /).
RUN for s in CapPrm CapEff CapInh CapAmb CapBnd; do \
      v=$(sed -n "s/^$s:[[:space:]]*//p" /proc/self/status); \
      [ "$((0x$v & 0x200000))" = 0 ] || exit 1; \
    done
RUN cat /proc/self/cgroup >/cg && cat /proc/1/cgroup >/init

FROM ${BASE} AS s2
RUN cat /proc/self/cgroup >/cg && cat /proc/1/cgroup >/init

FROM ${BASE}
COPY --from=s1 /cg /s1
COPY --from=s1 /init /s1-init
COPY --from=s2 /cg /s2
COPY --from=s2 /init /s2-init
CMD ["cat", "/s1", "/s1-init", "/s2", "/s2-init"]
