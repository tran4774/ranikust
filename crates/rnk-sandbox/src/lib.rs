//! Isolated execution of `RUN` commands, and privilege probing.
//!
//! Creates the namespaces, mounts `proc`, `dev` and `sys`, enters the stage
//! rootfs with `pivot_root` (or `MS_MOVE` where seccomp blocks it), drops
//! the bounding set, installs the `RUN`'s seccomp profile
//! ([`SeccompProfile`], Docker's by default), cuts the other capability
//! sets, manages stub mountpoints, switches to the `USER` ids that
//! `rnk-rootfs` resolved in the rootfs, and executes the command. In sequential
//! kaniko mode it runs `RUN` directly on `/`, like kaniko. Where ranikust's
//! cgroup v2 tree is delegated, each full-sandbox `RUN` also gets cgroups of
//! its own, ended with `cgroup.kill` (ADR-0110).
//!
//! Also probes, by actually trying each step, what the environment allows this
//! crate: namespaces, the uid map, `proc`, `pivot_root` or `MS_MOVE`, and
//! `sysfs`. The binary runs these probes, with those of `rnk-snapshot`, and
//! picks the mode. [`probe_user_namespace`] tries the steps a `RUN` in a
//! user namespace would need: root only reports it, rootless overlay mode
//! routes on it. [`enter_build_user_namespace`] moves the process into the
//! build's user namespace, each id map tried in a forked child first.
//! There, [`probe_with_fallbacks`] lets a `RUN` get a read-only rbind of
//! `/sys` where the user-namespace probe found a new `sysfs` refused
//! ([`UsernsReport::fallbacks`]), a `RUN` with a network namespace of its
//! own still trying a new one first (the rbind with no network device
//! where it does not mount), and a read-only rbind of `/proc` only
//! at the operator's request ([`Fallbacks::allow_proc_rbind`], ADR-0126,
//! ADR-0132).
//!
//! ADR-0014 planned a bare `chroot` per stage for `--parallel-stages` where
//! nothing can be mounted; it is not built, and the flag is refused
//! (ADR-0060). A bare `chroot` is never used when mounting
//! is possible.
//!
//! Makes the sealed copy of ranikust's binary a build runs from, so that
//! no process ranikust forks has a binary a `RUN` could write, and reports
//! which mount the binary as installed is on and whether a writable mount
//! reaches it, for `ranikust`'s refusal in kaniko mode on `/` (ADR-0101).
//!
//! # Use
//!
//! While the process is single-threaded: [`run_from_sealed_copy`] first,
//! which runs the process again from the copy, then
//! [`enter_build_namespace`], then [`probe`]; `--mode=auto` also calls
//! [`probe_user_namespace`], in overlay mode before the probes (the overlay
//! probe's joined thread may still be counted), in kaniko mode after them.
//! Then [`Sandbox::start`], which replays the stub journals of dead builds
//! and starts the spawner thread, and [`Sandbox::run`] per `RUN`.
//! Kaniko mode on `/` calls [`enter_build_namespace`] (whose failure only
//! says root may not mount), then [`trim_to_run_capabilities`], still
//! single-threaded; in `--mode=auto` then [`probe_user_namespace`]; then
//! [`Sandbox::start_direct`] (ADR-0078). A build in its own user namespace
//! calls [`probe_user_namespace`], then [`enter_build_user_namespace`] in
//! place of [`enter_build_namespace`], before any thread, then
//! [`probe_with_fallbacks`] in place of [`probe`]. Where a set-up failing
//! there may still fall back to another mode (ADR-0136), [`fork_to_wait`]
//! comes before: the child enters the namespace and builds, the parent
//! waits outside it.
//!
//! Phase 1 implements [`Isolation::Full`] as real root (`CAP_SYS_ADMIN`)
//! with `pivot_root` or `MS_MOVE`, and [`Isolation::Direct`]. Phase 3 adds a
//! `RUN` in the build's user namespace with the `proc` and `sysfs`
//! fallbacks; the bare-`chroot` path comes later
//! (`docs/agents/crates/rnk-sandbox.md`).
//!
//! # Boundaries
//!
//! - Users and groups are looked up in the rootfs through `rnk-rootfs`, never
//!   on the host.
//! - Does not snapshot or diff the rootfs, or mount or probe overlays
//!   (`rnk-snapshot`); the seal only reads whether `/` is one (ADR-0101).
//! - Writes the cgroupfs only in and below ranikust's own cgroup, and only
//!   where that cgroup is delegated (ADR-0110); sets no resource limit,
//!   only how many cgroups, and how deep, a `RUN` may make below its own.

mod caps;
mod cgroup;
mod child;
mod direct;
mod error;
mod fd;
mod journal;
mod plan;
mod probe;
mod relay;
mod seal;
mod seccomp;
mod setup;
mod spawner;
mod spec;
mod stop;
mod supervise;
mod sweep;
mod userns;

pub use child::RootEntry;
pub use error::{Error, Result, Step};
pub use probe::{Caps, Fallbacks, KernelFs, probe, probe_with_fallbacks};
pub use seal::{BinaryMount, SealedCopy, Sealing, run_from_sealed_copy};
pub use seccomp::SeccompProfile;
pub use setup::{enter_build_namespace, enter_build_user_namespace, trim_to_run_capabilities};
pub use spawner::Sandbox;
pub use spec::{
    BindMount, Isolation, MountKind, Network, NofileLimit, RunSpec, SecretFile, Stdio,
    forbid_core_dumps,
};
pub use stop::{StopSignal, StopSignals, block_stop_signals};
pub use supervise::{FORWARDED_STOP, Forked, Supervised, Waiting, fork_to_wait};
pub use userns::{
    BuildUserNamespace, IdMap, IdRange, MountResult, StepFailure, StepResult, SubordinateIds,
    UsernsReport, probe_user_namespace,
};
