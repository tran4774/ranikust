ARG BASE
FROM ${BASE}
# How many seccomp filters a RUN runs under, whether setpriority(2) works
# (busybox's nice dies when it fails, its uname ignores the error), and
# whether personality(ADDR_NO_RANDOMIZE) does, which Docker's default
# profile refuses (busybox's linux64 -R).
RUN sed -n 's/^Seccomp_filters:[[:space:]]*//p' /proc/self/status > /filters
RUN if nice -n 1 true; then echo ok; else echo refused; fi > /nice
RUN if linux64 -R true; then echo ok; else echo refused; fi > /personality
