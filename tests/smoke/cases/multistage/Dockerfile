# Stages: `a` and `b` are independent (with --jobs 2 they build at once),
# `c` starts FROM the stage `a`, and the final stage copies from all three.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS a
RUN echo a > /a

FROM ${BASE} AS b
RUN echo b > /b

FROM a AS c
RUN test -f /a && echo c > /c

FROM ${BASE}
COPY --from=a /a /out/a
COPY --from=b /b /out/b
COPY --from=c /c /out/c
CMD ["sh", "-c", "cat /out/a /out/b /out/c | tr '\\n' ' '"]
