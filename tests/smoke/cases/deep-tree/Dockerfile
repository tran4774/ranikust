# A stage leaves a tree 3000 levels deep, its path past PATH_MAX (T15):
# the build ends, the next stage starts from a root without it, and the
# build's directories go with it. case.sh has the checks.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS deep
RUN mkdir /deep && cd /deep && i=0 \
 && while [ "$i" -lt 3000 ]; do mkdir d && cd -P d && : >f && i=$((i + 1)); done \
 && echo "$i" >/levels

FROM ${BASE}
COPY --from=deep /levels /levels
RUN test ! -e /deep
CMD ["cat", "/levels"]
