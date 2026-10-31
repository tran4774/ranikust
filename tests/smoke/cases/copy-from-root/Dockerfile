# `COPY --from=<stage> /`: the whole stage, read from its kept copy.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS a
RUN mkdir -p /app && echo js > /app/main.js && ln -s /var/lib/ranikust /to-state

FROM ${BASE}
COPY --from=a / /all/
COPY check.sh /check.sh
CMD ["sh", "/check.sh"]
