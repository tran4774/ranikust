//! kaniko mode: the root is walked and hashed instead of mounted
//! (`design/kaniko-mode.md`): the ignore list, the parallel walk, its hashes
//! and the diff it computes (`#walk-hash-and-diff`, ADR-0039), and the
//! exporter that writes that diff as a layer (`WalkDiff::export`).

mod export;
mod ignore;
mod tree;
mod walk;

pub use ignore::{IgnoreSpec, MountInfo, ignore_list, parse_mountinfo, read_mountinfo};
pub use walk::{
    Change, Changed, DEFAULT_WALK_WORKERS, EntryMeta, SnapshotMode, WalkDiff, WalkOptions,
    WalkStats, Walker,
};
