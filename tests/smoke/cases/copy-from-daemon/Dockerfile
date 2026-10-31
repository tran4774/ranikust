# A process that leaves its RUN (`setsid`, kaniko#1697) keeps changing the
# stage's root while kaniko mode keeps it, empties it for the next stage
# and copies it back: dirs come and go, links into the state directory and
# /ranikust appear. The build may fail (fail closed); it must never put the
# state directory, /ranikust or a secret in the image.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS a
# Seconds the daemon waits: past the stage's own walk (which a change
# fails), into the keep and the later stages; tuned on a laptop, and the
# case holds whichever step the daemon meets.
ARG DELAY=0.4
RUN mkdir -p /app && echo a > /app/a \
 && (setsid sh -c 'sleep $DELAY; i=0; while [ $i -lt 3000 ]; do \
      mkdir -p /app/d$i && echo $i > /app/d$i/f; \
      ln -sfn /var/lib/ranikust /app/d$i/state; ln -sfn /ranikust /app/d$i/rnk; \
      rm -rf /app/d$((i-3)); i=$((i+1)); done' </dev/null >/dev/null 2>&1 &) \
 && sleep 0.3

FROM ${BASE} AS b
RUN echo b > /b

FROM a
COPY --from=b /b /b
COPY --from=a /app/a /copied-a
COPY check.sh /check.sh
CMD ["sh", "/check.sh"]
