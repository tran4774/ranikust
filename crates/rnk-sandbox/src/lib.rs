//! Isolated execution of `RUN` commands, and privilege probing.
//!
//! Creates the namespaces, mounts `proc`, `dev` and `sys`, enters the stage
//! rootfs with `pivot_root` (or `MS_MOVE` where seccomp blocks it), drops
//! the bounding set, installs the `RUN`'s seccomp profile
//! ([`SeccompProfile`], Docker's by default), cuts the other capability
//! sets, manages stub mountpoints, switches to the `USER` ids that
//! `rnk-rootfs` resolved in the rootfs, and executes the command. In sequential
//! kaniko mode it runs `RUN` directly on `/`, like kaniko.
//!
//! Also probes, by actually trying each step, what the environment allows this
//! crate: namespaces, the uid map, `proc`, `pivot_root` or `MS_MOVE`, and
//! `sysfs`. The binary runs these probes, with those of `rnk-snapshot`, and
//! picks the mode.
//!
//! ADR-0014 planned a bare `chroot` per stage for `--parallel-stages` where
//! nothing can be mounted; it is not built, and the flag is refused
//! (ADR-0060). A bare `chroot` is never used when mounting
//! is possible.
//!
//! # Use
//!
//! While the process is single-threaded: [`enter_build_namespace`], then
//! [`probe`]. Then [`Sandbox::start`], which replays the stub journals of
//! dead builds and starts the spawner thread, and [`Sandbox::run`] per `RUN`.
//! Kaniko mode on `/` calls [`enter_build_namespace`] (whose failure only
//! says root may not mount), then [`trim_to_run_capabilities`], still
//! single-threaded, then [`Sandbox::start_direct`] (ADR-0078).
//!
//! Phase 1 implements [`Isolation::Full`] as real root (`CAP_SYS_ADMIN`)
//! with `pivot_root` or `MS_MOVE`, and [`Isolation::Direct`]. User
//! namespaces, the `proc` and `sysfs` fallbacks and the bare-`chroot` path
//! come later (`docs/agents/crates/rnk-sandbox.md`).
//!
//! # Boundaries
//!
//! - Users and groups are looked up in the rootfs through `rnk-rootfs`, never
//!   on the host.
//! - Does not snapshot or diff the rootfs, or mount or probe overlays
//!   (`rnk-snapshot`).

mod caps;
mod child;
mod direct;
mod error;
mod journal;
mod plan;
mod probe;
mod relay;
mod seccomp;
mod setup;
mod spawner;
mod spec;
mod stop;
mod sweep;

pub use child::RootEntry;
pub use error::{Error, Result, Step};
pub use probe::{Caps, probe};
pub use seccomp::SeccompProfile;
pub use setup::{enter_build_namespace, trim_to_run_capabilities};
pub use spawner::Sandbox;
pub use spec::{
    BindMount, Isolation, MountKind, Network, NofileLimit, RunSpec, SecretFile, Stdio,
    forbid_core_dumps,
};
pub use stop::{StopSignal, StopSignals, block_stop_signals};
