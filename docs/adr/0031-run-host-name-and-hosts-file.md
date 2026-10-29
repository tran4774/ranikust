---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0031: A RUN sees the fixed host name `ranikust` and the host's /etc/hosts, or loopback without network

## Context and Problem Statement

Every `RUN` in the sandbox gets its own UTS namespace and writable copies of
`/etc/hostname` and `/etc/hosts` bound over the root's
([sandbox](../design/sandbox.md#per-run-sequence)). Something has to fill
them. kaniko creates no UTS namespace, so its `RUN` sees the host name and
the `/etc/hosts` of the container or pod kaniko runs in. A host name that
varies with the environment ends up in any file a `RUN` writes it to
(`uname -n`, build-info stamps), and tools such as `hostname -f`, `sudo` and
Java's `InetAddress.getLocalHost()` fail when the name does not resolve.
Questions [Q-075](../open-questions.md#q-075-which-host-name-does-a-run-see)
and [Q-076](../open-questions.md#q-076-what-etchosts-does-a-run-see).

## Considered Options

Host name:

- (a) As kaniko: the host name of the environment ranikust runs in. Varies
  per pod, so the bytes of a `RUN` that records it vary per host.
- (b) BuildKit's `buildkitsandbox` [inferred].
- (c) A fixed `ranikust`.
- (d) One of these plus a flag. Neither kaniko nor BuildKit has one.

`/etc/hosts`:

- (a) ranikust's own `/etc/hosts` plus the host name, for every network.
  Under `--network=none` it lists names that cannot be reached.
- (b) Loopback entries plus the host name, as BuildKit [inferred]. Drops the
  entries kaniko users rely on.
- (c) (a) with the host network, (b) with `--network=none`.

## Decision Outcome

A `RUN` sees the fixed host name `ranikust`, and an `/etc/hosts` that is
ranikust's own with the host network and loopback-only with
`--network=none`, both ending with the host name on `127.0.0.1`.

- The host name is the same in every environment and has no flag;
  `rnk-builder` may still set `RunSpec::hostname`.
- With the host network (`ADR-0010`), the `RUN` shares ranikust's network
  namespace, so the entries of ranikust's `/etc/hosts` (Kubernetes
  `hostAliases`, a local registry alias) are reachable and kept.
- `--add-host` is not part of this decision; it comes with the other
  registry and network flags
  ([Q-047](../open-questions.md#q-047-what-are-the-mirror-registry-map-and-helper-flags-called)).
- The three files are stubs: they never reach a layer. The smoke tests
  check this (`tests/smoke/lib.bash`).

### Consequences

- Good, because a `RUN` that records its host name gives the same bytes on
  every host, as `--reproducible` needs.
- Good, because the host name always resolves, so `hostname -f`, `sudo`
  and Java work in both network modes.
- Good, because builds that rely on the pod's `/etc/hosts` under kaniko
  keep working with the host network.
- Bad, because a `RUN` that records its host name differs from kaniko (the
  pod's name) and from BuildKit (`buildkitsandbox`) [inferred].
- Neutral, because the `/etc/hosts` under the host network depends on the
  host, but it is a stub and does not reach a layer.

## Evidence

- kaniko runs `RUN` with `exec.Command` and only `Setpgid`, without a UTS
  namespace (`pkg/commands/run.go:356`), and keeps the bind-mounted
  `/etc/hosts` and `/etc/hostname` out of layers through mountinfo
  (`DetectFilesystemIgnoreList`, `pkg/util/fs_util.go:630`) [code].
- `rnk-sandbox` already implements the decision: `DEFAULT_HOSTNAME`
  (`crates/rnk-sandbox/src/spec.rs`), `sethostname` in the child
  (`child.rs`), `hosts_file` and its test `hosts_files`
  (`plan.rs`) [code].
- The `smoke` suite of `just test-vm` sees `hostname` = `ranikust` inside
  the sandbox as real root on kernels 5.15 and 6.8, and `just smoke` the
  same in rootless podman [measured,
  [testing](../testing.md#the-vm-harness-tier-t2)].
- BuildKit's `buildkitsandbox` and its loopback `/etc/hosts` are recalled,
  not checked in BuildKit's source [inferred].

## More Information

- Answers Q-075 and Q-076. Related: `ADR-0003` (default isolation like
  `docker build`), `ADR-0010` (`RUN` shares the host network;
  `--network=none`).
- How it works: [sandbox](../design/sandbox.md#per-run-sequence).
- Revisit if a `--hostname` flag is asked for, or when `--add-host` lands.
