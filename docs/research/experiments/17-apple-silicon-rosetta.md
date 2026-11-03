# Experiment 17: ranikust in OrbStack containers on Apple Silicon

| | |
|---|---|
| **Status** | Complete, 2026-11-04 |
| **Question** | Why does the x86_64 release refuse to start in an amd64 container on an Apple Silicon Mac, and does an aarch64 build run in an arm64 container there? |
| **Answer** | Rosetta, which OrbStack uses for amd64 containers, answers `ENOSYS` to `openat2` on a 7.2 kernel, so the start-up probe refuses. An aarch64 build passes the probe and built a one-`RUN` `alpine` image in kaniko mode in about 3.3 s, one round, with the `RUN` going through Rosetta. |
| **Related** | [ADR-0009](../../adr/0009-x86-64-first-aarch64-ready.md), [ADR-0028](../../adr/0028-minimum-kernel-linux-5-8.md); [rootfs design](../../design/rootfs.md); user guide, "Choosing where to run it" |
| **Original** | none: run directly for this report |

## Summary

- A user reported that `ranikust build --mode kaniko` in
  `docker run --platform linux/amd64 alpine:3.24` on OrbStack stopped with
  "this kernel (7.2.12-orbstack-…) lacks openat2 (ENOSYS); ranikust needs
  Linux 5.8 or later".
- The amd64 container's processes run under Rosetta, and Rosetta has no
  `openat2`: the x86_64 binary refuses, whatever the kernel
  **[measured]**.
- An aarch64 static build in an arm64 container passes the probe and builds
  an image in kaniko mode; the image stays `linux/amd64` (ADR-0009), and its
  `RUN` executes the amd64 `alpine` binaries through Rosetta **[measured]**.
- The probe now reports `ENOSYS` on a 5.8 or later release as coming from
  something between ranikust and the kernel, not from an old kernel.

## Questions

1. Is the refusal caused by the kernel or by the x86_64 emulation?
2. Does an aarch64 build run in an arm64 container on the same machine?
3. What platform does that build produce, and how does its `RUN` execute?

## Environment

- MacBook (Apple Silicon, `RELEASE_ARM64_T8132`), macOS 26.7 (25G229).
- OrbStack 2.2.5, `rosetta: true`; Docker Engine 29.8.2, `linux/arm64`,
  storage driver `overlay2` on btrfs; kernel 7.2.12-orbstack-00394.
- Containers with default options: root, Docker's default seccomp and
  capability set, no `--privileged`, default bridge network.
- ranikust from `main` at 6a0f2fa, release profile: `x86_64` static-pie
  through `just build-static`; `aarch64` static, linked by zig
  (`cargo zigbuild --release --locked --target aarch64-unknown-linux-musl -p ranikust`).

## Method

- **Images:** each binary in an image of its own, made with `docker import`
  from a tar holding only `/kaniko/ranikust`, a `Dockerfile` and `/tmp`
  (`--platform linux/amd64` and `linux/arm64`), so no image already on the
  machine was pulled or retagged. The aarch64 image also holds a CA bundle
  (see Limitations).
- **Dockerfile:** `FROM docker.io/library/alpine:3.20` and
  `RUN echo hello > /hello && uname -m > /arch`.
- **Runs:** `ranikust build -c /workspace -f /workspace/Dockerfile --no-push --mode kaniko`,
  with the context bind-mounted read-only and, for the aarch64 run,
  `--tar-path` on a bind-mounted output directory. One round each.
- **Emulation check:** in an amd64 container of the already present
  `alpine:3.24`, `/proc/self/maps` and `/proc/cpuinfo`.

## Results

| Variant | Platform | Probe | Build | Basis |
|---|---|---|---|---|
| x86_64 binary | `linux/amd64` (Rosetta) | refused: `openat2` `ENOSYS` | — | **[measured]** |
| aarch64 binary | `linux/arm64` (native) | passes | image built; 3.3 s for the whole `docker run`, 1 round | **[measured]** |

- The amd64 container maps `/mnt/rv/[rosetta]` into its processes and
  reports `vendor_id: VirtualApple` **[measured]**.
- The aarch64 build's tar holds a `linux/amd64` config; its `RUN` layer has
  `/arch` = `x86_64` and `/hello` = `hello` **[measured]**.
- With the change of this report, the x86_64 binary prints
  "openat2 is unavailable (…) although this kernel (7.2.12-orbstack-…) is
  Linux 5.8 or later: something between ranikust and the kernel lacks or
  refuses it…" **[measured]**.

## Findings

1. The refusal comes from Rosetta, not the kernel: the kernel is 7.2 and the
   x86_64 process runs under Rosetta (Results).
2. The aarch64 build works in an arm64 OrbStack container in kaniko mode
   (Results, row 2).
3. That build produces `linux/amd64` images whose `RUN` runs through Rosetta,
   since `--platform` accepts only `linux/amd64` (ADR-0009).

## Implications for ranikust

- The start-up probe reports `ENOSYS`, or a missing `statx` mount id, on a
  release that is 5.8 or later as hidden by something in between
  (`Cause::Hidden`), and the user guide describes the arm64-container
  workaround as untested. The check itself is unchanged (ADR-0028).
- An aarch64 release would make the workaround a supported path; that is a
  release decision (ADR-0009), not taken here.

## Limitations and open questions

- One machine, one round, OrbStack only. Docker Desktop with its Rosetta
  setting on is expected to behave the same **[inferred]**; with QEMU
  instead, the result depends on the QEMU version **[inferred]**.
- Only kaniko mode in a default container; overlay mode
  (`--privileged`) and the sandbox under Rosetta were not run.
- The aarch64 binary was linked by zig, not the release recipe's static-pie
  link.
- Without a CA bundle in the image, ranikust panicked while building its
  registry client, although it embeds its roots
  ([#67](https://github.com/tran4774/ranikust/issues/67)); the aarch64 run
  therefore had a bundle added.

## Reproduction

The commands are in Method. Nothing was left on the machine: the work
directory, the two imported images and the containers (`--rm`) were removed
after the runs.

## References

1. OrbStack, Rosetta setting (`orbctl config show`), read on the test
   machine, 2026-11-04.
