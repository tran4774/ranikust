//! Isolated execution of `RUN` commands, and privilege probing.
//!
//! Creates the namespaces, mounts `proc`, `dev` and `sys`, enters the stage
//! rootfs with `pivot_root` (or `MS_MOVE` where seccomp blocks it), drops
//! capabilities, manages stub mountpoints, switches to the `USER` ids that
//! `rnk-rootfs` resolved in the rootfs, and executes the command. In sequential
//! kaniko mode it runs `RUN` directly on `/`, like kaniko.
//!
//! Also probes, by actually trying each step, what the environment allows this
//! crate: namespaces, the uid map, `proc`, `pivot_root` or `MS_MOVE`, and
//! `sysfs`. The binary runs these probes, with those of `rnk-snapshot`, and
//! picks the mode.
//!
//! When nothing can be mounted, `--parallel-stages` runs `RUN` with a bare
//! `chroot` into a per-stage directory instead: `/dev` nodes via `mknod` when
//! permitted, the static fake `/proc` and `/sys` files, and the
//! `JAVA_TOOL_OPTIONS` limits of ADR-0019. A bare `chroot` is never used when
//! mounting is possible.
//!
//! # Boundaries
//!
//! - Users and groups are looked up in the rootfs through `rnk-rootfs`, never
//!   on the host.
//! - Does not snapshot or diff the rootfs, or mount or probe overlays
//!   (`rnk-snapshot`).
