---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0010: `RUN` shares the host network; `--network=none`

## Context and Problem Statement

`docker build` gives each `RUN` a bridged network with NAT. Building that
needs a network namespace, a veth pair and NAT rules, which need privileges
ranikust often does not have (kaniko mode runs without any namespace). kaniko
runs every `RUN` in the container's own network. What network does a `RUN`
see?

## Considered Options

- A private bridged network per `RUN` (veth + NAT), like Docker.
- The network of the host or container that runs ranikust.
- The host network by default, plus an option for no network at all.

## Decision Outcome

`RUN` shares the network namespace ranikust runs in, and `--network=none` gives
it an empty network namespace instead; ranikust never sets up veth pairs or NAT.

- Default: no network namespace is created for `RUN`; it reaches what
  ranikust itself can reach.
- `--network=none`: `RUN` gets a new, empty network namespace (loopback only).

### Consequences

- Good, because the behaviour matches kaniko and works at every privilege
  level, with no network code to maintain.
- Bad, because `RUN` can reach services bound to the host's loopback and
  local network.
- Bad, because mounting a fresh `sysfs` needs privileges over the network
  namespace's owning user namespace: with the host network and a new user
  namespace it fails, and the sandbox falls back to a read-only rbind of
  `/sys`.

## Evidence

- [Experiment 02](../research/experiments/02-run-sandbox.md): `sysfs` mount
  failed with `EPERM` under `--network=host` and inside a self-created user
  namespace; rbind of `/sys` plus a read-only remount worked, as in rootless
  runc.
- [Survey 05](../research/survey/05-decisions.md) Q8;
  [survey 02](../research/survey/02-kaniko-mode-requirements.md) §3.

## More Information

`--network=none` was not run end to end in the survey. In kaniko mode inside a
container that cannot create namespaces, `--network=none` cannot be honoured;
the build must then fail with an explicit error rather than run with network
[inferred]. Sandbox details: [sandbox](../design/sandbox.md).
