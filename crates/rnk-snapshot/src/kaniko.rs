//! kaniko mode: the parallel walk, its hashes and the diff it computes
//! (`design/kaniko-mode.md#walk-hash-and-diff`, ADR-0039).

mod tree;
mod walk;

pub use walk::{
    Change, Changed, DEFAULT_WALK_WORKERS, EntryMeta, SnapshotMode, WalkDiff, WalkOptions,
    WalkStats, Walker,
};
