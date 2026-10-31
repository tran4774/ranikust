# A hostile stage read by a later one after another stage reused the root
# (kaniko mode keeps it first): links out of its root, into the state
# directory and /ranikust, a FIFO, a setuid file, a hardlink, a private
# directory of another user, a device node where root may make one (T2).
# The final stage is built FROM it and reads it with COPY --from, so it is
# kept whole and copied back.
ARG BASE=docker.io/library/alpine:3.20
FROM ${BASE} AS a
RUN mkdir -p /app/dist /app/private && echo js > /app/dist/main.js \
 && ln /app/dist/main.js /app/hard.js && ln -s /app/dist /dist \
 && ln -s /var/lib/ranikust /to-state && ln -s /ranikust /to-rnk \
 && ln -s / /to-root && ln -s /proc/self/environ /to-environ \
 && mkfifo /app/fifo && cp /bin/busybox /app/suid && chmod 4755 /app/suid \
 && echo k > /app/private/key && chown -R 1000:1000 /app/private \
 && chmod 700 /app/private && { mknod /app/null c 1 3 2>/dev/null || true; }

FROM ${BASE} AS b
RUN echo b > /b && rm -rf /app

FROM a
COPY --from=b /b /b
COPY --from=a /dist/main.js /via-link.js
COPY check.sh /check.sh
CMD ["sh", "/check.sh"]
