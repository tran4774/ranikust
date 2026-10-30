# The smallest build: one base, one RUN, a CMD.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE}
RUN echo "hello from ranikust" > /hello
CMD ["cat", "/hello"]
