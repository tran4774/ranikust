# Rows 5 and 6b of ADR-0100's privilege matrix (#84; 6b in
# tests/vm/guest/matrix-user.sh): a RUN that needs an owner other than root
# mapped, and prints what its proc shows: PID 1's name and how many PID
# namespaces deep it sees itself.
ARG BASE
FROM ${BASE}
RUN mkdir /d && chown 1234:1234 /d \
 && echo "proc: pid 1 $(cat /proc/1/comm), NSpid levels $(awk '/^NSpid:/ { print NF - 1 }' /proc/self/status)"
