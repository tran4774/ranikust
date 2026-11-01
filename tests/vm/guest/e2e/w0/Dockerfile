# W0, the light e2e workload (tests/vm/guest/e2e.sh): a few seconds per
# builder, the whole pipeline. Its base (base-w0.py) is pushed to the VM's
# registry by the suite: busybox, a file with an xattr, a hardlink pair, a
# setuid file, a symlink, files deleted below.
ARG BASE_IMAGE
FROM ${BASE_IMAGE} AS base
# A hardlink, a symlink and a setuid file made by a RUN; a file and a
# directory of the base deleted (whiteouts).
RUN mkdir -p /opt/a && echo one > /opt/a/f && ln /opt/a/f /opt/a/hard && ln -s f /opt/a/soft \
    && cp /opt/suid /opt/a/suid && chmod 4755 /opt/a/suid \
    && rm /etc/remove-me && rm -r /opt/gone

# Three children of `base` and a grandchild: in kaniko mode with a directory
# per stage, the parent's directory is copied for some and handed to the
# last (ADR-0059, Q-410); `one` and `two` may run side by side (--jobs).
FROM base AS one
RUN echo one > /one.txt

FROM base AS two
RUN echo two > /two.txt

FROM two AS three
RUN echo three >> /two.txt && echo three > /three.txt

FROM base AS final
COPY --from=one /one.txt /out/one.txt
COPY --from=three /two.txt /three.txt /out/
COPY files/ /files/
RUN cat /out/one.txt /out/two.txt /out/three.txt > /out/all.txt && rm /files/delete-me \
    && stat -c '%A %h %n' /opt/a/* > /out/modes.txt
