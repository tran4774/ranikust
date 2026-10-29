---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0009: x86_64 releases first, aarch64 kept buildable

## Context and Problem Statement

Releasing for several CPU architectures multiplies the release and test
matrix. ranikust only needs x86_64 at first, but aarch64 runners are common
and adding them later must not require a redesign. What is supported now,
and what must the code do to stay portable?

## Considered Options

- x86_64 only, with no portability rules.
- x86_64 and aarch64 releases from the start.
- x86_64 releases only, with rules and CI that keep aarch64 buildable and
  tested.

## Decision Outcome

ranikust releases for x86_64 Linux only, while the code stays buildable and
unit-tested on aarch64 so an aarch64 release later needs no redesign.

- No architecture-specific code. Crates that use assembly (for example `sha2`,
  zlib) must have a pure-Rust fallback.
- Syscall numbers come from `rustix` or `libc`, never hard-coded. Seccomp
  profiles ([ADR-0003](0003-dockerfile-trust-and-default-isolation.md)) are
  generated per architecture.
- An image's architecture comes from the manifest list and the image config,
  never from the host: pick the right `platform` when pulling a base, write
  `architecture`/`variant` into the config. `--platform` exists, but the
  first releases accept only `linux/amd64`.
- CI cross-builds `aarch64-unknown-linux-musl` from the start (build, and run
  unit tests under qemu-user), so x86-only code cannot slip in.
- Multi-platform builds (an arm64 `RUN` on x86 through `binfmt_misc`) are not
  on the roadmap.

### Consequences

- Good, because one release target keeps the release and test matrix small.
- Good, because aarch64 stays a packaging task, not a port.
- Bad, because CI pays for a cross build, and every dependency must build for
  aarch64 musl.
- Bad, because aarch64 has only been exercised under emulation.

## Evidence

- [Experiment 05](../research/experiments/05-rust-ecosystem.md) §2.3:
  `cargo-zigbuild` 0.24.0 with zig 0.15.2 cross-built a static
  `aarch64-unknown-linux-musl` binary (4.0 MB with `ring`, 5.4 MB with
  `aws-lc-rs`) that ran correctly under `qemu-aarch64-static`, including TLS,
  gzip and zstd.
- [Survey 05](../research/survey/05-decisions.md) Q7 and §1.

## More Information

Build tooling: [ADR-0020](0020-static-musl-via-cargo-zigbuild.md). At the time
of writing, CI runs `cargo check` for the aarch64 target; running unit tests
under qemu-user is still to be added. Real aarch64 hardware is a phase 6 test
item.
