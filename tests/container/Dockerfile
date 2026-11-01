# The T2c test image (ADR-0063): Ubuntu 24.04, as the VM, with the static
# test binaries and their lists copied in by tests/container/lane.sh. No
# volume: nothing of the host is reachable from the container.
FROM ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
# iproute2 for the netadmin control check (`ip link set lo up`).
RUN apt-get update -qq \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends iproute2 >/dev/null \
    && rm -rf /var/lib/apt/lists/*
COPY run-profile.sh default.txt sysadmin.txt netadmin.txt /t2c/
COPY bin/ /t2c/bin/
