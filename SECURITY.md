# Security policy

ranikust runs Dockerfile `RUN` commands as root, mounts filesystems, extracts
untrusted image layers and handles registry credentials. This page says what
counts as a vulnerability and how to report one. The reasoning behind it is in
the [threat model](docs/security/threat-model.md).

## Supported versions

ranikust is pre-alpha and has no release yet. Until 1.0, only the latest
release will receive security fixes; before the first release, fixes land on
the `main` branch.

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

## In scope

These are vulnerabilities in ranikust, whatever the Dockerfile or image:

- **Escaping the state directory or store.** ranikust itself reads, writes
  or deletes anything outside its state directory, store, requested outputs
  and, in kaniko mode in a container, `/`
  because of a crafted layer, build context or Dockerfile: `..` components,
  symlinks, hardlinks, whiteouts, device nodes, setuid files, or races
  between a check and a use.
- **File descriptor or capability leaks.** A `RUN` process inherits a
  descriptor ranikust holds, keeps capabilities outside Docker's default set,
  or gets a wrong UID/GID mapping.
- **Sandbox escape in a mode that promises isolation.** In overlay mode
  (root or rootless) or in kaniko mode with mounts available, a `RUN`
  command reaches more of the host than `docker build` would allow it under
  the same privileges.
- **Credential leaks.** Registry credentials or build secrets end up in logs,
  layers, image configs, cache entries, error messages or another build's
  `RUN` environment.
- **Cache poisoning.** One build changes what another build receives from a
  shared store or registry cache beyond the documented sharing (for example,
  a layer whose content does not match its digest is accepted).

## By design

These are documented properties, not vulnerabilities:

- **`RUN` runs as root.** A Dockerfile you build can do whatever root can do
  inside its sandbox. Whether to build an untrusted Dockerfile is your
  decision ([ADR-0003](docs/adr/0003-dockerfile-trust-and-default-isolation.md));
  `--userns`, `--seccomp` and `--network=none` tighten the default.
- **kaniko mode inside an unprivileged container has no isolation.** Where
  ranikust cannot mount or create namespaces (Docker's default profile, most
  Kubernetes pods), `RUN` commands run in the same container as ranikust,
  as they do in kaniko. The container is the only boundary. With
  `--parallel-stages`, stages can also read each other's files and secrets
  ([ADR-0014](docs/adr/0014-kaniko-mode-sequential-by-default.md)). In
  kaniko mode outside a container without mounts, `RUN` enters the root
  ranikust made in its state directory with a bare `chroot`, which root can
  leave: the host is then the only boundary
  ([ADR-0043](docs/adr/0043-direct-run-on-a-workdir-chroots-with-placed-files.md)).
- **`RUN` shares the host network** unless `--network=none` is given
  ([ADR-0010](docs/adr/0010-run-network-host-or-none.md)).
- **Refusing to run** where no mode works, such as a regular user on Ubuntu
  24.04 or later without an AppArmor profile
  ([ADR-0004](docs/adr/0004-support-every-privilege-level.md)).
- Vulnerabilities in base images, in the tools a Dockerfile runs, or in the
  kernel. Report kernel bugs to the kernel.

## Hardening

To build Dockerfiles you do not trust: run ranikust in a disposable VM or a
container made for that one build, use `--userns` and `--seccomp=default`,
add `--network=none` when the build needs no network, and give each trust
domain its own store. These options are designed but not implemented yet;
the [book](docs/book/src/where-to-run.md) tracks what each environment
provides.
