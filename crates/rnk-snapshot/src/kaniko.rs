//! kaniko mode: the root is walked and hashed instead of mounted
//! (`design/kaniko-mode.md`): the ignore list, the parallel walk, its hashes
//! and the diff it computes (`#walk-hash-and-diff`, ADR-0039).

mod ignore;
mod tree;
mod walk;

pub use ignore::{IgnoreSpec, MountInfo, ignore_list, parse_mountinfo, read_mountinfo};
pub use walk::{
    Change, Changed, DEFAULT_WALK_WORKERS, EntryMeta, SnapshotMode, WalkDiff, WalkOptions,
    WalkStats, Walker,
};
