ARG BASE
FROM ${BASE}
# How many seccomp filters a RUN runs under, and whether uname(2) works.
RUN sed -n 's/^Seccomp_filters:[[:space:]]*//p' /proc/self/status > /filters
RUN if uname >/dev/null 2>&1; then echo ok; else echo refused; fi > /uname
