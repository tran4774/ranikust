ARG BASE
FROM ${BASE}
# How many seccomp filters a RUN runs under, and whether setpriority(2)
# works: busybox's nice dies when it fails, its uname ignores the error.
RUN sed -n 's/^Seccomp_filters:[[:space:]]*//p' /proc/self/status > /filters
RUN if nice -n 1 true; then echo ok; else echo refused; fi > /nice
