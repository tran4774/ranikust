# Four independent stages, each burning CPU (sha256 over MB MiB of zeros)
# and writing FILES small files, then a final stage reading all four.
# Dockerfile.chain is the same work forced one stage after the other.
ARG BASE
FROM $BASE AS s1
ARG FILES=300
ARG MB=4
RUN mkdir /out && cd /out && i=0 && while [ "$i" -lt "$FILES" ]; do echo "1 $i" >"f$i"; i=$((i + 1)); done && \
    dd if=/dev/zero bs=1M count="$MB" 2>/dev/null | sha256sum | cut -c1-16 >/out/sum

FROM $BASE AS s2
ARG FILES=300
ARG MB=4
RUN mkdir /out && cd /out && i=0 && while [ "$i" -lt "$FILES" ]; do echo "2 $i" >"f$i"; i=$((i + 1)); done && \
    dd if=/dev/zero bs=1M count="$MB" 2>/dev/null | sha256sum | cut -c1-16 >/out/sum

FROM $BASE AS s3
ARG FILES=300
ARG MB=4
RUN mkdir /out && cd /out && i=0 && while [ "$i" -lt "$FILES" ]; do echo "3 $i" >"f$i"; i=$((i + 1)); done && \
    dd if=/dev/zero bs=1M count="$MB" 2>/dev/null | sha256sum | cut -c1-16 >/out/sum

FROM $BASE AS s4
ARG FILES=300
ARG MB=4
RUN mkdir /out && cd /out && i=0 && while [ "$i" -lt "$FILES" ]; do echo "4 $i" >"f$i"; i=$((i + 1)); done && \
    dd if=/dev/zero bs=1M count="$MB" 2>/dev/null | sha256sum | cut -c1-16 >/out/sum

FROM $BASE
COPY --from=s1 /out/sum /s1
COPY --from=s2 /out/sum /s2
COPY --from=s3 /out/sum /s3
COPY --from=s4 /out/sum /s4
CMD ["sh", "-c", "cat /s1 /s2 /s3 /s4 | tr '\\n' ' '"]
