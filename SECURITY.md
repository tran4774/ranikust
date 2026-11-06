# Security policy

ranikust runs Dockerfile `RUN` commands as root, mounts filesystems, extracts
untrusted image layers and handles registry credentials. This page says what
counts as a vulnerability and how to report one. The reasoning behind it is in
the [threat model](docs/security/threat-model.md).

## Supported versions

ranikust is pre-alpha; its releases are pre-releases. Until 1.0, only the
latest release receives security fixes: a fix lands on the `main` branch
and ships in the next release, and there are no fixes to older releases.
Report a problem against the latest release or `main`.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**
(<https://github.com/tran4774/ranikust/security/advisories/new>). Do not open
a public issue, pull request or discussion for a suspected vulnerability.
There is no security email address.

Please include:

- the ranikust version or commit;
- the snapshot mode and how it was chosen (automatic or forced), the user
  ranikust ran as, and where it ran (host, Docker, Podman, Kubernetes, CI);
- the kernel version and distribution;
- a minimal Dockerfile, image or build context that reproduces the problem,
  and what happened compared with what you expected.

The maintainers aim to acknowledge a report within 7 days, agree on a fix and
a disclosure date with the reporter, and publish a GitHub security advisory
with the fix. The default embargo is 90 days.

A Low or Medium weakness the project finds itself, in a review or an audit,
and does not fix first, or that was already in a release, is tracked in a
private draft advisory and published after the release that fixes it
(closed instead, if no release ever contained the weakness), so a release
may ship with known Low or Medium weaknesses still open, with audit leads
not yet confirmed, and with a High or Critical weakness that a branch under
review is fixing. A residual the maintainer accepts has no fix release; the
[threat model](docs/security/threat-model.md#6-accepted-risks) documents it
([workflow](docs/agents/workflow.md#security-findings),
[ADR-0111](docs/adr/0111-a-low-or-medium-security-finding-is-a-draft-advisory-that-does-not-block-a-release.md)).

## In scope

These are vulnerabilities in ranikust, whatever the Dockerfile or image:

- **Escaping the state directory or store.** ranikust itself reads, writes
  or deletes anything outside its state directory, store, requested
  outputs, its own cgroup where it is delegated with the cgroups it makes
  below it (ADR-0110), and, in kaniko mode in a container, `/`
  because of a crafted layer, build context or Dockerfile: `..` components,
  symlinks, hardlinks, whiteouts, device nodes, setuid files, or races
  between a check and a use.
- **File descriptor or capability leaks.** A `RUN` process inherits a
  descriptor ranikust holds, keeps capabilities outside Docker's default set,
  or gets a wrong UID/GID mapping.
- **Sandbox escape in a mode that promises isolation.** In overlay mode
  (as root; rootless is planned, Phase 3) or in kaniko mode with mounts
  available, a `RUN`
  command reaches more of the host than `docker build` would allow it under
  the same privileges.
- **Credential leaks.** Registry credentials or build secrets end up in logs,
  layers, image configs, cache keys, tags or entries, error messages or
  another build's `RUN` environment.
- **Cache poisoning.** One build changes what another build receives from a
  shared store or layer cache beyond the documented sharing (for example,
  a layer whose content does not match its digest is accepted, or a layer
  is pushed under a key whose inputs its step did not read). Out of scope:
  poisoning through a cache repository or OCI layout that people other
  than the operator may write. The operator names the cache, and ranikust
  trusts it as it trusts the destination until cache entries are signed
  ([#37](https://github.com/tran4774/ranikust/issues/37);
  [threat model, section 3](docs/security/threat-model.md#3-inputs-and-how-much-they-are-trusted)).
  A cached layer still gets every check a base layer gets: escaping them
  is in scope.

## By design

These are documented properties, not vulnerabilities:

- **`RUN` runs as root.** A Dockerfile you build can do whatever root can do
  inside its sandbox. Whether to build an untrusted Dockerfile is your
  decision ([ADR-0003](docs/adr/0003-dockerfile-trust-and-default-isolation.md));
  `--network=none` tightens the default for the `RUN`, and the full sandbox
  applies Docker's seccomp profile (see below); `--userns` is planned
  (Phase 3) and not built.
- **kaniko mode inside an unprivileged container has no isolation.** Where
  ranikust cannot mount or create namespaces (Docker's default profile, most
  Kubernetes pods), `RUN` commands run in the same container as ranikust,
  as they do in kaniko. The container is the only boundary. ranikust keeps
  its own state and credentials out of their reach (anonymous files, a
  non-dumpable process, no `CAP_SYS_PTRACE` for `RUN`, no `RUN` process
  left running; [ADR-0051](docs/adr/0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)),
  but not what it cannot control: a value on its command line, credentials
  another process of the container holds in its environment, and files
  mounted into the container. Pass registry credentials in
  `DOCKER_AUTH_CONFIG`, secrets with `--secret`, and run ranikust as the
  container's PID 1. `--parallel-stages`, which would let stages read each
  other's files and secrets, is refused
  ([ADR-0060](docs/adr/0060-copy-leaves-special-files-out-where-run-has-no-nodev.md)). Kaniko
  mode outside a container where root may not mount is refused: `RUN`
  would run in a bare `chroot` on the host
  ([ADR-0050](docs/adr/0050-ranikust-picks-the-build-root-no-workdir-flag.md)).
- **`RUN` shares the host network** unless `--network=none` is given
  ([ADR-0010](docs/adr/0010-run-network-host-or-none.md)).
- **Refusing to run** where no mode works. Today that includes every regular
  user, root outside a container where it may not mount, root in a
  container whose `/proc/sys` is writable
  ([ADR-0054](docs/adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md)),
  and kaniko mode on a container's `/` with ranikust's binary reachable
  through a writable mount other than `/`'s (mount the binary, and any tree
  that holds it, read-only: Docker `:ro`, Kubernetes `readOnly: true`; a
  file with one link, no link of it bound read-write; on a root that is
  not an overlay (overlayfs or fuse-overlayfs), a binary in the root too),
  or on a mount it cannot determine, and kaniko mode on a container's `/`
  whose PID namespace another container or the host shares, when ranikust
  can open one of their processes through `/proc` as it starts, the only
  ones it counts (give the build container a PID namespace of its own: no
  Kubernetes `shareProcessNamespace` or `hostPID`, no `--pid`)
  ([ADR-0101](docs/adr/0101-a-build-runs-from-a-sealed-copy-of-the-binary.md),
  [threat model section 6](docs/security/threat-model.md#6-accepted-risks));
  the rootless mode and its AppArmor advice for Ubuntu 24.04 or later are
  Phase 3 ([ADR-0004](docs/adr/0004-support-every-privilege-level.md)).
- Vulnerabilities in base images, in the tools a Dockerfile runs, or in the
  kernel. Report kernel bugs to the kernel.

## Hardening

To build Dockerfiles you do not trust, run ranikust in a disposable VM or in a
container made for that one build, add `--network=none` when the build needs
no network, and give each trust domain its own store and cache repository.
`--network=none` isolates the `RUN` only: ranikust's own registry requests
still leave from its network namespace ([threat model, section 6](docs/security/threat-model.md#6-accepted-risks),
[#25](https://github.com/tran4774/ranikust/issues/25)). Every `RUN` in the
full sandbox runs under Docker's default seccomp profile, with no flag needed
(`--seccomp=unconfined` turns it off; in kaniko mode on `/` the container's
own profile applies;
[ADR-0109](docs/adr/0109-seccomp-profiles-are-compiled-to-bpf-in-rnk-sandbox-and-the-default-is-mobys.md)).
`--userns` is designed to tighten this further, but is not implemented yet
(Phase 3). The
[book](docs/book/src/where-to-run.md) tracks what each environment provides.
